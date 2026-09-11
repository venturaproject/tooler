//! The orchestration layer: `execute_playbook` (the top-level task loop), handler/
//! `on_failure:` dispatch, `when:`/`until:`/`failed_when:` evaluation, `loop:`
//! (sequential and `max_parallel:`), `parallel:`, `block:`/`rescue:`/`always:`, and
//! retry bookkeeping. Delegates each concrete action to `run_task_once` in `dispatch`.
use super::*;
use anyhow::{Result, anyhow, bail};
use colored::Colorize;
use serde::Serialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

// ── Runner ────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub(crate) struct TaskOutcome {
    pub(crate) name: String,
    pub(crate) status: &'static str,
    /// Whether this task's success is considered a real effect (see `changed_when:`).
    /// Always `false` for `skipped`/`ignored`/`failed` -- only a real `ok` success can
    /// be "changed", same scope `changed_when:` itself already has.
    pub(crate) changed: bool,
    pub(crate) error: Option<String>,
    /// Best-effort classification of `error`, so an agent can match on a stable
    /// category instead of parsing free-text -- see `classify_error`. `None` whenever
    /// `error` is `None`.
    pub(crate) error_kind: Option<&'static str>,
    /// Wall-clock time this attempt took, in milliseconds -- `0` for a task that never
    /// really ran (`when:`-skipped, or skipped by `--dry`), same "never touched
    /// anything" scope `--audit-log` already uses to decide what's worth timing.
    pub(crate) duration_ms: u64,
}

/// Best-effort classification of a task failure's message into a coarse, stable
/// category an agent can match on instead of parsing free-text `error`. Heuristic, not
/// exhaustive -- same "advisory, not a formal verifier" spirit as `--lint`'s findings:
/// message shapes this codebase itself controls are recognized by their known
/// prefixes/substrings; anything else (a raw ssh/rsync/db subprocess's own stderr,
/// mostly) falls back to "other" rather than being guessed at.
pub(crate) fn classify_error(msg: &str) -> &'static str {
    if msg.contains("refused to run without confirm: true") {
        "confirm_required"
    } else if msg.starts_with("assertion failed") {
        "assertion"
    } else if msg.starts_with("command exited with code") {
        "exit_code"
    } else if msg.contains("timed out") {
        "timeout"
    } else if msg.starts_with("HTTP ") {
        "http_status"
    } else if msg.contains("not configured") || msg.contains("tooler config set") {
        "config"
    } else {
        "other"
    }
}

/// Rejects a `notify:` name with no matching `playbook.handlers` entry upfront, rather
/// than silently never running it — walks into `block:`/`rescue:`/`always:` too, since
/// those tasks can `notify:` just like any other.
pub(crate) fn validate_handlers(playbook: &Playbook) -> Result<()> {
    let handler_names: std::collections::HashSet<&str> =
        playbook.handlers.iter().map(|h| h.name.as_str()).collect();

    fn walk(tasks: &[Task], handler_names: &std::collections::HashSet<&str>) -> Result<()> {
        for t in tasks {
            for n in &t.notify {
                if !handler_names.contains(n.as_str()) {
                    bail!("task '{}' notifies unknown handler '{}'", t.name, n);
                }
            }
            if let Some(b) = &t.block {
                walk(b, handler_names)?;
            }
            if let Some(r) = &t.rescue {
                walk(r, handler_names)?;
            }
            if let Some(a) = &t.always {
                walk(a, handler_names)?;
            }
            if let Some(p) = &t.parallel {
                walk(p, handler_names)?;
            }
        }
        Ok(())
    }
    walk(&playbook.tasks, &handler_names)?;
    walk(&playbook.on_failure, &handler_names)
}

/// Whether a task passes `--tags`/`--skip-tags` filtering: matches `--tags` (if set,
/// needs at least one overlapping tag) and matches none of `--skip-tags`. Only ever
/// applied to a playbook's own top-level tasks — same scoping `tag_filter` already has
/// (see `execute_playbook`), not nested `block:`/`rescue:`/`always:`/`include:` tasks.
/// Shared by `execute_playbook`'s task selection and `--list-tasks`/`--list-tags` so the
/// two stay consistent.
/// A task tagged `always` is included even when `--tags` wouldn't otherwise select it —
/// an escape hatch for a cleanup/logging task that should never be skipped by tag
/// filtering, the same special tag real Ansible has. It's still excluded by an explicit
/// `--skip-tags always` (or any other tag it also carries that's in `--skip-tags`) —
/// `always` only ever widens what `--tags` selects, it never overrides `--skip-tags`.
pub(crate) fn task_matches_tags(
    task: &Task,
    tag_filter: &Option<Vec<&str>>,
    skip_tag_filter: &Option<Vec<&str>>,
) -> bool {
    let always = task.tags.iter().any(|t| t == "always");
    let included = match tag_filter {
        None => true,
        Some(tags) => always || task.tags.iter().any(|t| tags.contains(&t.as_str())),
    };
    let excluded = match skip_tag_filter {
        None => false,
        Some(skip) => task.tags.iter().any(|t| skip.contains(&t.as_str())),
    };
    included && !excluded
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn execute_playbook(
    playbook: &Playbook,
    tag_filter: &Option<Vec<&str>>,
    skip_tag_filter: &Option<Vec<&str>>,
    notes: &Option<String>,
    vars: &mut HashMap<String, String>,
    include_stack: &mut Vec<PathBuf>,
    is_top_level: bool,
    env: &RunEnv,
) -> Result<()> {
    validate_handlers(playbook)?;

    let json = env.quiet;
    let sep = "─".repeat(56);

    if !json {
        println!(
            "\n{} {} {}",
            "PLAY".bold().cyan(),
            format!("[{}]", playbook.name).bold(),
            env.playbook_dir.display().to_string().dimmed()
        );
        if let Some(desc) = &playbook.description {
            println!("     {}", desc.dimmed());
        }
        println!("{}", sep.dimmed());
        if let Some(n) = notes {
            println!("\n{}", "NOTES".bold().yellow());
            println!("{}", sep.dimmed());
            for line in n.trim_end().lines() {
                println!("{line}");
            }
            println!("{}", sep.dimmed());
        }
    }

    let mut tasks: Vec<&Task> = playbook
        .tasks
        .iter()
        .filter(|t| task_matches_tags(t, tag_filter, skip_tag_filter))
        .collect();

    // --start-at-task: only ever set on the top-level RunEnv (see RunEnv.start_at), so
    // this only skips ahead in the outermost playbook's own task list.
    if let Some(start) = &env.start_at {
        let idx = tasks.iter().position(|t| &t.name == start).ok_or_else(|| {
            anyhow::anyhow!(
                "no task named '{start}' (check `tooler play <file> --dry` for task names)"
            )
        })?;
        if idx > 0 {
            if !json {
                println!(
                    "  {}",
                    format!("(starting at task '{start}' — {idx} earlier task(s) skipped)")
                        .dimmed()
                );
            }
            tasks.drain(..idx);
        }
    }

    let total = tasks.len();
    let mut ok = 0usize;
    let mut failed = 0usize;
    let mut skipped = 0usize;
    let mut outcomes: Vec<TaskOutcome> = Vec::new();
    let mut notified: Vec<String> = Vec::new();

    for (i, task) in tasks.iter().enumerate() {
        if !json {
            println!(
                "\n{} [{}/{}] {}",
                "TASK".bold().yellow(),
                i + 1,
                total,
                task.name.bold()
            );
        }

        if let Some(w) = &task.when
            && !eval_when(w, vars)
        {
            if !json {
                println!("  {}", format!("(skipped — when: {w} was false)").dimmed());
            }
            skipped += 1;
            outcomes.push(TaskOutcome {
                name: task.name.clone(),
                status: "skipped",
                changed: false,
                error: None,
                error_kind: None,
                duration_ms: 0,
            });
            if is_top_level && !env.dry {
                write_checkpoint(env, &playbook.name, &task.name, vars);
            }
            continue;
        }

        if task.flush_handlers {
            if env.dry {
                if !json {
                    println!("  {}", "(dry run — skipped)".dimmed());
                }
                skipped += 1;
                outcomes.push(TaskOutcome {
                    name: task.name.clone(),
                    status: "skipped",
                    changed: false,
                    error: None,
                    error_kind: None,
                    duration_ms: 0,
                });
            } else {
                let start = Instant::now();
                run_notified_handlers(
                    playbook,
                    &mut notified,
                    vars,
                    include_stack,
                    env,
                    json,
                    notes,
                    &sep,
                    is_top_level,
                    &mut ok,
                    &mut failed,
                    skipped,
                    &mut outcomes,
                )?;
                ok += 1;
                outcomes.push(TaskOutcome {
                    name: task.name.clone(),
                    status: "ok",
                    changed: false,
                    error: None,
                    error_kind: None,
                    duration_ms: start.elapsed().as_millis() as u64,
                });
            }
            if is_top_level && !env.dry {
                write_checkpoint(env, &playbook.name, &task.name, vars);
            }
            continue;
        }

        let start = Instant::now();
        let result = run_task(task, vars, include_stack, env);
        let duration_ms = start.elapsed().as_millis() as u64;

        match result {
            Ok(_) => {
                if env.dry {
                    if !json {
                        println!("  {}", "(dry run — skipped)".dimmed());
                    }
                    skipped += 1;
                    outcomes.push(TaskOutcome {
                        name: task.name.clone(),
                        status: "skipped",
                        changed: false,
                        error: None,
                        error_kind: None,
                        duration_ms,
                    });
                } else {
                    ok += 1;
                    let changed = match &task.changed_when {
                        Some(expr) => eval_when(expr, vars),
                        None => true,
                    };
                    outcomes.push(TaskOutcome {
                        name: task.name.clone(),
                        status: "ok",
                        changed,
                        error: None,
                        error_kind: None,
                        duration_ms,
                    });

                    if changed {
                        for h in &task.notify {
                            if !notified.contains(h) {
                                notified.push(h.clone());
                            }
                        }
                    }
                }
            }
            Err(e) => {
                let msg = redact_secrets(&e.to_string(), &task_secret_values(task));
                let kind = classify_error(&msg);
                if task.ignore_errors {
                    if !json {
                        println!("  {} failed (ignored): {e}", "!".yellow().bold());
                    }
                    skipped += 1;
                    outcomes.push(TaskOutcome {
                        name: task.name.clone(),
                        status: "ignored",
                        changed: false,
                        error: Some(msg),
                        error_kind: Some(kind),
                        duration_ms,
                    });
                } else {
                    failed += 1;
                    let reason = msg.clone();
                    outcomes.push(TaskOutcome {
                        name: task.name.clone(),
                        status: "failed",
                        changed: false,
                        error: Some(msg),
                        error_kind: Some(kind),
                        duration_ms,
                    });

                    run_failure_hook(
                        playbook,
                        &task.name,
                        &reason,
                        vars,
                        include_stack,
                        env,
                        json,
                        &sep,
                        &mut ok,
                        &mut failed,
                        &mut outcomes,
                    );

                    if json {
                        println!(
                            "{}",
                            serde_json::json!({
                                "playbook": playbook.name,
                                "dry": env.dry,
                                "notes": notes,
                                "tasks": outcomes,
                                "ok": ok,
                                "failed": failed,
                                "skipped": skipped,
                                "changed": outcomes.iter().filter(|o| o.changed).count(),
                                "success": false,
                            })
                        );
                        if is_top_level {
                            // Drop is skipped by process::exit — release the lock by hand.
                            if let Some(p) = &env.lock_path {
                                let _ = std::fs::remove_file(p);
                            }
                            std::process::exit(1);
                        }
                        bail!("playbook failed");
                    }

                    println!("  {} {}", "✗".red().bold(), e.to_string().red());
                    println!("\n{}", sep.dimmed());
                    println!(
                        "\n{} failed at task \"{}\". {}",
                        "PLAY".bold().red(),
                        task.name.bold(),
                        "Remaining tasks skipped.".dimmed()
                    );
                    print_recap(
                        ok,
                        failed,
                        skipped,
                        outcomes.iter().filter(|o| o.changed).count(),
                    );
                    bail!("playbook failed");
                }
            }
        }

        // Reached for every non-fatal outcome above (dry-skip, real success, ignored
        // failure) — a hard failure already returned/exited inside the match. Never
        // checkpoints in --dry, since dry mode does no real work to resume from.
        if is_top_level && !env.dry {
            write_checkpoint(env, &playbook.name, &task.name, vars);
        }
    }

    // Reached only if every regular task above succeeded (or was skipped/ignored) —
    // any unhandled failure already returned or exited above. Any handler a
    // flush_handlers: task didn't already run mid-playbook runs now.
    run_notified_handlers(
        playbook,
        &mut notified,
        vars,
        include_stack,
        env,
        json,
        notes,
        &sep,
        is_top_level,
        &mut ok,
        &mut failed,
        skipped,
        &mut outcomes,
    )?;

    // A fully-completed playbook has nothing left to resume — best-effort, never fails
    // the run over a stray delete error. Never touches the checkpoint in --dry: a dry
    // run does no real work, so it must not discard a real checkpoint from an earlier
    // failed run just because a preview happened to "succeed" afterward. --keep-checkpoint
    // opts out of the deletion entirely -- e.g. an MCP-driven session that appends and
    // runs one task at a time needs the checkpoint to survive every successful call, not
    // just a failed one, so the next call can --resume from it.
    if is_top_level
        && !env.dry
        && !env.keep_checkpoint
        && let Some(path) = &env.state_path
    {
        let _ = std::fs::remove_file(path);
    }

    if json {
        println!(
            "{}",
            serde_json::json!({
                "playbook": playbook.name,
                "dry": env.dry,
                "notes": notes,
                "tasks": outcomes,
                "ok": ok,
                "failed": failed,
                "skipped": skipped,
                "changed": outcomes.iter().filter(|o| o.changed).count(),
                "success": true,
            })
        );
        return Ok(());
    }

    println!("\n{}", sep.dimmed());
    print_recap(
        ok,
        failed,
        skipped,
        outcomes.iter().filter(|o| o.changed).count(),
    );
    Ok(())
}

/// Runs the playbook's `on_failure:` tasks after its task loop hit a non-ignored
/// failure, right before `execute_playbook` reports and bails. Best-effort by design:
/// a failing `on_failure:` task is logged and counted but never aborts the hook or
/// re-triggers it, and the run still exits non-zero on the original failure regardless.
/// A no-op when `on_failure:` is empty or in `--dry`. `{{failed_task}}` and
/// `{{failure_reason}}` are set for these tasks. Its outcomes are appended to
/// `outcomes` so they show in the same JSON `tasks` array / recap that then reports
/// `success: false`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_failure_hook(
    playbook: &Playbook,
    failed_task: &str,
    reason: &str,
    vars: &mut HashMap<String, String>,
    include_stack: &mut Vec<PathBuf>,
    env: &RunEnv,
    json: bool,
    sep: &str,
    ok: &mut usize,
    failed: &mut usize,
    outcomes: &mut Vec<TaskOutcome>,
) {
    if playbook.on_failure.is_empty() || env.dry {
        return;
    }

    vars.insert("failed_task".to_string(), failed_task.to_string());
    vars.insert("failure_reason".to_string(), reason.to_string());

    if !json {
        println!("\n{}", sep.dimmed());
    }

    for t in &playbook.on_failure {
        if !json {
            println!("\n{} [{}]", "ON_FAILURE".bold().magenta(), t.name.bold());
        }
        let start = Instant::now();
        let result = run_task(t, vars, include_stack, env);
        let duration_ms = start.elapsed().as_millis() as u64;
        match result {
            Ok(()) => {
                *ok += 1;
                outcomes.push(TaskOutcome {
                    name: t.name.clone(),
                    status: "ok",
                    changed: false,
                    error: None,
                    error_kind: None,
                    duration_ms,
                });
            }
            Err(e) => {
                *failed += 1;
                let msg = redact_secrets(&e.to_string(), &task_secret_values(t));
                if !json {
                    println!(
                        "  {} on_failure task failed (ignored): {e}",
                        "!".yellow().bold()
                    );
                }
                let kind = classify_error(&msg);
                outcomes.push(TaskOutcome {
                    name: t.name.clone(),
                    status: "failed",
                    changed: false,
                    error: Some(msg),
                    error_kind: Some(kind),
                    duration_ms,
                });
            }
        }
    }
}

/// Runs every currently-pending `notify:`ed handler and drains `notified` — shared by
/// `execute_playbook`'s natural end-of-run flush and a mid-run `flush_handlers:` task
/// (see its dispatch point above). A handler failure fails the whole playbook the same
/// way a regular task failure does (same JSON/text reporting shape), addressed to the
/// handler instead of an indexed task.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_notified_handlers(
    playbook: &Playbook,
    notified: &mut Vec<String>,
    vars: &mut HashMap<String, String>,
    include_stack: &mut Vec<PathBuf>,
    env: &RunEnv,
    json: bool,
    notes: &Option<String>,
    sep: &str,
    is_top_level: bool,
    ok: &mut usize,
    failed: &mut usize,
    skipped: usize,
    outcomes: &mut Vec<TaskOutcome>,
) -> Result<()> {
    for name in notified.drain(..) {
        let handler = playbook
            .handlers
            .iter()
            .find(|h| h.name == name)
            .expect("validated by validate_handlers");

        if !json {
            println!("\n{} [{}]", "HANDLER".bold().magenta(), handler.name.bold());
        }

        let start = Instant::now();
        let handler_result = run_task(handler, vars, include_stack, env);
        let duration_ms = start.elapsed().as_millis() as u64;
        match handler_result {
            Ok(()) => {
                *ok += 1;
                // A handler is a real Task -- honor its own changed_when: the same way
                // a regular task's success branch does, instead of hardcoding true.
                let changed = match &handler.changed_when {
                    Some(expr) => eval_when(expr, vars),
                    None => true,
                };
                outcomes.push(TaskOutcome {
                    name: handler.name.clone(),
                    status: "ok",
                    changed,
                    error: None,
                    error_kind: None,
                    duration_ms,
                });
            }
            Err(e) => {
                *failed += 1;
                let msg = redact_secrets(&e.to_string(), &task_secret_values(handler));
                let kind = classify_error(&msg);
                outcomes.push(TaskOutcome {
                    name: handler.name.clone(),
                    status: "failed",
                    changed: false,
                    error: Some(msg),
                    error_kind: Some(kind),
                    duration_ms,
                });

                if json {
                    println!(
                        "{}",
                        serde_json::json!({
                            "playbook": playbook.name,
                            "dry": env.dry,
                            "notes": notes,
                            "tasks": outcomes,
                            "ok": *ok,
                            "failed": *failed,
                            "skipped": skipped,
                            "changed": outcomes.iter().filter(|o| o.changed).count(),
                            "success": false,
                        })
                    );
                    if is_top_level {
                        if let Some(p) = &env.lock_path {
                            let _ = std::fs::remove_file(p);
                        }
                        std::process::exit(1);
                    }
                    bail!("playbook failed");
                }

                println!("  {} {}", "✗".red().bold(), e.to_string().red());
                println!("\n{}", sep.dimmed());
                println!(
                    "\n{} failed at handler \"{}\".",
                    "PLAY".bold().red(),
                    handler.name.bold()
                );
                print_recap(
                    *ok,
                    *failed,
                    skipped,
                    outcomes.iter().filter(|o| o.changed).count(),
                );
                bail!("playbook failed");
            }
        }
    }
    Ok(())
}

/// A minimal condition language over `render()`-substituted strings: "<a> == <b>",
/// "<a> != <b>", "<a> >= <b>", "<a> <= <b>", "<a> > <b>", "<a> < <b>", or a bare truthy
/// check. Not a full expression language — matches tooler's existing plain `{{var}}`
/// templating rather than adding a new one. `>=`/`<=` are checked before the
/// single-character `>`/`<` so `"5 >= 3"` doesn't get wrongly split on the bare `>` into
/// `"5 "`/`"= 3"`.
pub(crate) fn eval_when(expr: &str, vars: &HashMap<String, String>) -> bool {
    let rendered = render(expr, vars);
    let rendered = rendered.trim();
    if let Some((lhs, rhs)) = rendered.split_once(">=") {
        return compare_numeric(lhs, rhs, |a, b| a >= b);
    }
    if let Some((lhs, rhs)) = rendered.split_once("<=") {
        return compare_numeric(lhs, rhs, |a, b| a <= b);
    }
    if let Some((lhs, rhs)) = rendered.split_once("!=") {
        return lhs.trim() != rhs.trim();
    }
    if let Some((lhs, rhs)) = rendered.split_once("==") {
        return lhs.trim() == rhs.trim();
    }
    if let Some((lhs, rhs)) = rendered.split_once('>') {
        return compare_numeric(lhs, rhs, |a, b| a > b);
    }
    if let Some((lhs, rhs)) = rendered.split_once('<') {
        return compare_numeric(lhs, rhs, |a, b| a < b);
    }
    !rendered.is_empty() && rendered != "false" && rendered != "0"
}

/// Backs `eval_when`'s numeric operators. Parses both sides as `f64`; if either isn't a
/// number, the comparison is `false` rather than a guess — the same "don't pretend to
/// know" default this DSL already uses elsewhere (e.g. a `scrape:` field that doesn't
/// match becomes `""`, not an error or a wrong answer).
pub(crate) fn compare_numeric(lhs: &str, rhs: &str, op: impl Fn(f64, f64) -> bool) -> bool {
    match (lhs.trim().parse::<f64>(), rhs.trim().parse::<f64>()) {
        (Ok(a), Ok(b)) => op(a, b),
        _ => false,
    }
}

/// Expands `loop:` (if present) into one retried-`run_task_once` call per item, with
/// `{{item}}` added to that iteration's vars. The first failing iteration (after its own
/// retries are exhausted) fails the whole task — remaining items are not attempted. If
/// `register:` is set, `<reg>` gets the *last* iteration's captured value (simplest
/// well-defined rule for a loop+register combination) and `<reg>.results` gets a JSON
/// array of every iteration's value, in order — see `Task::register`.
pub(crate) fn run_task(
    task: &Task,
    vars: &mut HashMap<String, String>,
    include_stack: &mut Vec<PathBuf>,
    env: &RunEnv,
) -> Result<()> {
    if task.max_parallel.is_some() && task.loop_spec.is_none() && task.parallel.is_none() {
        bail!("max_parallel: is only supported combined with loop: or parallel:");
    }
    if task.continue_on_error && task.loop_spec.is_none() {
        bail!("continue_on_error: is only supported combined with loop:");
    }
    let Some(spec) = &task.loop_spec else {
        return run_task_once_with_retries(task, vars, include_stack, env);
    };
    let units = resolve_loop_units(spec, vars);

    if let Some(chunk_size) = task.max_parallel.filter(|&n| n > 1) {
        return run_loop_parallel(
            task,
            &units,
            chunk_size,
            vars,
            include_stack.as_slice(),
            env,
        );
    }

    let mut results: Vec<String> = Vec::new();
    let mut failures: Vec<String> = Vec::new();
    for (index, unit) in units.iter().enumerate() {
        let mut loop_vars = vars.clone();
        apply_loop_unit(unit, &mut loop_vars, env.quiet);
        match run_task_once_with_retries(task, &mut loop_vars, include_stack, env) {
            Ok(()) => {
                if let Some(reg) = &task.register {
                    let val = loop_vars.get(reg).cloned().unwrap_or_default();
                    vars.insert(reg.clone(), val.clone());
                    results.push(val);
                }
            }
            Err(e) if task.continue_on_error => {
                failures.push(format!("{}: {e}", loop_unit_label(index + 1, unit)));
                if task.register.is_some() {
                    results.push(String::new());
                }
            }
            Err(e) => return Err(e),
        }
    }
    if let Some(reg) = &task.register {
        let json =
            serde_json::to_string(&results).expect("serializing a Vec<String> to JSON cannot fail");
        vars.insert(format!("{reg}.results"), json);
    }
    if !failures.is_empty() {
        bail!(
            "{} of {} loop item(s) failed: {}",
            failures.len(),
            units.len(),
            failures.join("; ")
        );
    }
    Ok(())
}

/// A short label for a loop unit in a `continue_on_error:` failure summary — the item's
/// own value for a scalar, its position for a map (whose fields vary task to task), or
/// the batch's index/size. Shared by `run_task`/`run_loop_parallel`.
pub(crate) fn loop_unit_label(index: usize, unit: &LoopUnit) -> String {
    match unit {
        LoopUnit::Item(LoopItem::Scalar(s)) => format!("item {index} ({s})"),
        LoopUnit::Item(LoopItem::Map(_)) => format!("item {index}"),
        LoopUnit::Batch { items, index: bi } => {
            format!("batch {bi} ({} item(s))", items.len())
        }
    }
}

/// Inserts one `loop:` unit's vars into `loop_vars` and echoes the `→ ...` line — the
/// per-iteration setup shared by both the sequential and the parallel (`max_parallel:`)
/// `loop:` paths. An `Item` sets `{{item}}`/`{{item.<field>}}`; a `Batch` sets
/// `{{batch}}` (a JSON array), `{{batch_index}}`, and `{{batch_size}}`.
pub(crate) fn apply_loop_unit(
    unit: &LoopUnit,
    loop_vars: &mut HashMap<String, String>,
    quiet: bool,
) {
    match unit {
        LoopUnit::Item(LoopItem::Scalar(s)) => {
            loop_vars.insert("item".to_string(), s.clone());
            if !quiet {
                println!("  {} item={}", "→".dimmed(), s.dimmed());
            }
        }
        LoopUnit::Item(LoopItem::Map(m)) => {
            for (k, v) in m {
                loop_vars.insert(format!("item.{k}"), v.clone());
            }
            if !quiet {
                let joined = m
                    .iter()
                    .map(|(k, v)| format!("item.{k}={v}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                println!("  {} {joined}", "→".dimmed());
            }
        }
        LoopUnit::Batch { items, index } => {
            let json = serde_json::Value::Array(items.iter().map(loop_item_to_json).collect());
            loop_vars.insert("batch".to_string(), json.to_string());
            loop_vars.insert("batch_index".to_string(), index.to_string());
            loop_vars.insert("batch_size".to_string(), items.len().to_string());
            if !quiet {
                println!("  {} batch {index} ({} item(s))", "→".dimmed(), items.len());
            }
        }
    }
}

/// Runs `items` in chunks of `chunk_size`, all items within a chunk concurrently (one
/// thread each, via `std::thread::scope` — the same primitive `fleet::run_on_targets`'s
/// `parallel: true` branch uses), the next chunk only starting once the current one fully
/// joins. Each thread gets its own cloned `vars`/`include_stack` — a parallel loop item is
/// its own independent branch, so it doesn't need (and, for `include:`'s cycle detection,
/// shouldn't share) the others' mutable state.
///
/// Results are consumed in **original item order**, not completion order, so the
/// `register:`-captures-the-last-iteration's-value rule (and `<reg>.results`, the JSON
/// array of every iteration's value — see `Task::register`) stays deterministic despite
/// concurrent execution, and the *first* error in original order fails the task — matching
/// the sequential loop's "first failing iteration fails the task" contract as closely as
/// concurrency allows. One narrowing of that guarantee: within a chunk that contains a
/// failing item, that chunk's other already-started items still run to completion (they
/// can't be cancelled mid-flight) even though the task as a whole is reported failed.
pub(crate) fn run_loop_parallel(
    task: &Task,
    units: &[LoopUnit],
    chunk_size: usize,
    vars: &mut HashMap<String, String>,
    include_stack: &[PathBuf],
    env: &RunEnv,
) -> Result<()> {
    let mut last_registered: Option<String> = None;
    let mut all_registered: Vec<String> = Vec::new();
    let mut failures: Vec<String> = Vec::new();
    let mut index = 0usize;
    for chunk in units.chunks(chunk_size) {
        let results: Vec<Result<Option<String>>> = std::thread::scope(|scope| {
            let handles: Vec<_> = chunk
                .iter()
                .map(|unit| {
                    let mut loop_vars = vars.clone();
                    let mut stack = include_stack.to_vec();
                    apply_loop_unit(unit, &mut loop_vars, env.quiet);
                    scope.spawn(move || {
                        run_task_once_with_retries(task, &mut loop_vars, &mut stack, env)?;
                        Ok(task
                            .register
                            .as_ref()
                            .and_then(|r| loop_vars.get(r).cloned()))
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        for (unit, r) in chunk.iter().zip(results) {
            index += 1;
            match r {
                Ok(val) => {
                    if let Some(v) = &val {
                        all_registered.push(v.clone());
                    }
                    if val.is_some() {
                        last_registered = val;
                    }
                }
                Err(e) if task.continue_on_error => {
                    failures.push(format!("{}: {e}", loop_unit_label(index, unit)));
                    if task.register.is_some() {
                        all_registered.push(String::new());
                    }
                }
                Err(e) => return Err(e),
            }
        }
    }
    if let Some(reg) = &task.register {
        if let Some(val) = last_registered {
            vars.insert(reg.clone(), val);
        }
        let json = serde_json::to_string(&all_registered)
            .expect("serializing a Vec<String> to JSON cannot fail");
        vars.insert(format!("{reg}.results"), json);
    }
    if !failures.is_empty() {
        bail!(
            "{} of {} loop item(s) failed: {}",
            failures.len(),
            units.len(),
            failures.join("; ")
        );
    }
    Ok(())
}

/// Runs `children` concurrently (each its own thread with a cloned vars/include_stack,
/// the same `std::thread::scope` model as a `max_parallel:` `loop:`), in chunks of `cap`
/// — the next chunk only starts once the current one fully joins. After each chunk, every
/// key a child added or changed vs. the parent's `vars` is merged back in child order
/// (a deterministic last-writer-wins on a collision), so a later chunk and the tasks
/// after the `parallel:` block see it. The first child that fails in child order fails
/// the whole `parallel:` task; a chunk's other already-started children still run to
/// completion (they can't be cancelled mid-flight).
pub(crate) fn run_parallel_block(
    children: &[Task],
    cap: Option<usize>,
    vars: &mut HashMap<String, String>,
    include_stack: &[PathBuf],
    env: &RunEnv,
) -> Result<()> {
    let cap = cap.unwrap_or(children.len()).max(1);
    for chunk in children.chunks(cap) {
        let before = vars.clone();
        let results: Vec<Result<HashMap<String, String>>> = std::thread::scope(|scope| {
            let handles: Vec<_> = chunk
                .iter()
                .map(|child| {
                    let mut child_vars = vars.clone();
                    let mut stack = include_stack.to_vec();
                    if !env.quiet {
                        println!("    {} {}", "•".dimmed(), child.name.dimmed());
                    }
                    scope.spawn(move || {
                        run_task(child, &mut child_vars, &mut stack, env)?;
                        Ok(child_vars)
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        for (child, r) in chunk.iter().zip(results) {
            let child_vars =
                r.with_context(|| format!("parallel: child task '{}' failed", child.name))?;
            for (k, v) in child_vars {
                if before.get(&k) != Some(&v) {
                    vars.insert(k, v);
                }
            }
        }
    }
    Ok(())
}

/// Retries a single (non-loop-expanded) task invocation up to `task.retries` extra times,
/// waiting `task.delay` (default 1s) between attempts. A no-op wrapper when `retries:`
/// isn't set (attempts=1) or in `--dry` (nothing ever fails in dry mode, since every
/// action's real work is itself gated on `!env.dry`). Also the single funnel every
/// concrete task attempt passes through regardless of nesting (top-level, `block:`/
/// `rescue:`/`always:`, `include:`, each `loop:` iteration — sequential or parallel) —
/// see `write_audit_entry`, called here so `--audit-log` covers all of them uniformly.
pub(crate) fn run_task_once_with_retries(
    task: &Task,
    vars: &mut HashMap<String, String>,
    include_stack: &mut Vec<PathBuf>,
    env: &RunEnv,
) -> Result<()> {
    let start = Instant::now();
    let attempts = task.retries.unwrap_or(0) + 1;
    for attempt in 1..=attempts {
        // failed_when: converts an otherwise-successful attempt into the same Err path
        // a real failure takes, *before* the Ok/Err match below — so retry-on-error,
        // the final "give up" bail, and the audit-log entry are all reused verbatim.
        // until: (below) is only ever checked when failed_when: did NOT trigger, same
        // precedence Ansible's failed_when/until interaction follows.
        let outcome = match run_task_once(task, vars, include_stack, env) {
            Ok(())
                if !env.dry
                    && task
                        .failed_when
                        .as_deref()
                        .is_some_and(|expr| eval_when(expr, vars)) =>
            {
                Err(anyhow!(
                    "failed_when: '{}' was true",
                    task.failed_when.as_deref().unwrap()
                ))
            }
            other => other,
        };
        match outcome {
            Ok(()) => {
                let satisfied = match &task.until {
                    None => true,
                    Some(expr) => env.dry || eval_when(expr, vars),
                };
                if satisfied {
                    let status = if env.dry { "dry" } else { "ok" };
                    write_audit_entry(env, task, status, None, start.elapsed());
                    return Ok(());
                }
                if attempt < attempts {
                    let delay = task.delay.unwrap_or(1);
                    if !env.quiet {
                        println!(
                            "  {} attempt {attempt}/{attempts}: until: '{}' not yet true — \
                             retrying in {delay}s...",
                            "!".yellow().bold(),
                            task.until.as_deref().unwrap()
                        );
                    }
                    std::thread::sleep(Duration::from_secs(delay));
                    continue;
                }
                let msg = format!(
                    "until: '{}' was still false after {attempts} attempt(s)",
                    task.until.as_deref().unwrap()
                );
                write_audit_entry(env, task, "failed", Some(&msg), start.elapsed());
                bail!("{msg}");
            }
            Err(e) if attempt < attempts && !env.dry => {
                let delay = task.delay.unwrap_or(1);
                if !env.quiet {
                    println!(
                        "  {} attempt {attempt}/{attempts} failed: {e} — retrying in {delay}s...",
                        "!".yellow().bold()
                    );
                }
                std::thread::sleep(Duration::from_secs(delay));
            }
            Err(e) => {
                write_audit_entry(env, task, "failed", Some(&e.to_string()), start.elapsed());
                return Err(e);
            }
        }
    }
    unreachable!("loop always returns on the last attempt")
}

/// Resolves an `include:` target the same way a top-level playbook argument is
/// resolved, except a literal path is relative to *this playbook's own directory*
/// (`env.playbook_dir`, matching how `run:`/`env_check:` paths already resolve) rather
/// than the process's CWD — `include: ./helpers/build.yml` means "next to me."
pub(crate) fn resolve_include_path(file: &str, env: &RunEnv) -> Result<PathBuf> {
    if is_literal_path(file) {
        return env
            .playbook_dir
            .join(file)
            .canonicalize()
            .with_context(|| format!("Cannot resolve include: {file}"));
    }
    let dir = env.project_root.join("playbooks");
    for ext in ["yml", "yaml"] {
        let candidate = dir.join(format!("{file}.{ext}"));
        if candidate.exists() {
            return candidate
                .canonicalize()
                .with_context(|| format!("Cannot resolve include: {}", candidate.display()));
        }
    }
    bail!(
        "No playbook named '{file}' in {} for include (looked for {file}.yml, {file}.yaml)",
        dir.display()
    );
}

/// Runs a list of tasks in order (used for `block:`/`rescue:`/`always:`), stopping at
/// the first unhandled failure — the same when:/loop:/retries:/register: support as the
/// top-level playbook loop, just without its JSON-summary/recap bookkeeping, since a
/// `block:` counts as a single outcome from its caller's perspective (see `run_block`).
pub(crate) fn run_task_sequence(
    tasks: &[Task],
    vars: &mut HashMap<String, String>,
    include_stack: &mut Vec<PathBuf>,
    env: &RunEnv,
) -> Result<()> {
    for task in tasks {
        if !env.quiet {
            println!("    {} {}", "•".dimmed(), task.name.dimmed());
        }
        if let Some(w) = &task.when
            && !eval_when(w, vars)
        {
            if !env.quiet {
                println!(
                    "      {}",
                    format!("(skipped — when: {w} was false)").dimmed()
                );
            }
            continue;
        }
        if let Err(e) = run_task(task, vars, include_stack, env) {
            if task.ignore_errors {
                if !env.quiet {
                    println!("      {} failed (ignored): {e}", "!".yellow().bold());
                }
            } else {
                return Err(e);
            }
        }
    }
    Ok(())
}

/// `block:` runs first; on failure, `rescue:` (if any) runs and — if it succeeds —
/// recovers the block. `always:` then runs unconditionally, and a failure there fails
/// the block even after a successful rescue.
pub(crate) fn run_block(
    block: &[Task],
    rescue: &[Task],
    always: &[Task],
    vars: &mut HashMap<String, String>,
    include_stack: &mut Vec<PathBuf>,
    env: &RunEnv,
) -> Result<()> {
    let result = match run_task_sequence(block, vars, include_stack, env) {
        Ok(()) => Ok(()),
        Err(e) if rescue.is_empty() => Err(e),
        Err(e) => {
            if !env.quiet {
                println!(
                    "    {} block failed: {e} — running rescue",
                    "!".yellow().bold()
                );
            }
            run_task_sequence(rescue, vars, include_stack, env)
        }
    };
    if !always.is_empty() {
        if !env.quiet {
            println!("    {} running always", "→".dimmed());
        }
        run_task_sequence(always, vars, include_stack, env)?;
    }
    result
}

/// Prints a colored unified line diff between `old` and `new` when `env.diff` (`--diff`)
/// is set — a no-op otherwise, and a no-op when the two are identical. Shared by
/// `fs_write:`/`write_file:`, called right before each applies its write.
pub(crate) fn print_diff_if_enabled(env: &RunEnv, old: &str, new: &str) {
    if !env.diff || old == new {
        return;
    }
    use similar::{ChangeTag, TextDiff};
    for change in TextDiff::from_lines(old, new).iter_all_changes() {
        let line = change.to_string_lossy();
        match change.tag() {
            ChangeTag::Delete => print!("  {}{}", "-".red().bold(), line.red()),
            ChangeTag::Insert => print!("  {}{}", "+".green().bold(), line.green()),
            ChangeTag::Equal => {}
        }
    }
}
pub(crate) fn print_recap(ok: usize, failed: usize, skipped: usize, changed: usize) {
    println!(
        "\n{}  {}  {}  {}  {}",
        "RECAP".bold(),
        format!("ok={ok}").green().bold(),
        if failed > 0 {
            format!("failed={failed}").red().bold()
        } else {
            format!("failed={failed}").dimmed()
        },
        format!("skipped={skipped}").dimmed(),
        format!("changed={changed}").dimmed(),
    );
}
