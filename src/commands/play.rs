use crate::{context::Context, output::OutputFormat, project};
use anyhow::{Context as _, Result, bail};
use clap::Args;
use colored::Colorize;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Args)]
pub struct PlayArgs {
    /// Playbook YAML file to run (omit with --init to generate a sample)
    pub file: Option<String>,

    /// Preview tasks without executing them
    #[arg(long)]
    pub dry: bool,

    /// Override a variable: --var key=value (repeatable)
    #[arg(long = "var", short = 'e')]
    pub vars: Vec<String>,

    /// Run only tasks matching these tags (comma-separated)
    #[arg(long)]
    pub tags: Option<String>,

    /// Generate a sample playbook.yml in the current directory
    #[arg(long)]
    pub init: bool,

    /// Print the companion playbooks/<name>.md notes (if any) and exit without running
    #[arg(long)]
    pub notes: bool,
}

// ── YAML schema ───────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct Playbook {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    vars: HashMap<String, String>,
    tasks: Vec<Task>,
    /// Tasks triggered by `notify:`, run at most once each after all regular tasks
    /// succeed, in first-notified order. Matched by `name` — see `validate_handlers`.
    #[serde(default)]
    handlers: Vec<Task>,
}

#[derive(Debug, Deserialize, Default)]
struct Task {
    name: String,
    #[serde(default)]
    ignore_errors: bool,
    #[serde(default)]
    tags: Vec<String>,
    /// Simple condition evaluated once against the playbook's vars, before any `loop:`
    /// expansion (does not see `{{item}}`). Supports "<a> == <b>", "<a> != <b>", or a
    /// bare truthy check after `{{var}}` substitution — not a full expression language.
    #[serde(default)]
    when: Option<String>,
    /// Run this task once per item, with `{{item}}` available to the action for that
    /// iteration. The first failing iteration fails the task (and, unless
    /// `ignore_errors`, the whole playbook) — remaining items are not attempted.
    #[serde(default, rename = "loop")]
    loop_items: Option<Vec<String>>,
    /// Capture this task's output into a variable, usable by later tasks via
    /// `{{name}}`. Supported on run/ssh/fleet only (see `run_task_once`). Inside a
    /// `loop:`, only the last iteration's value persists.
    #[serde(default)]
    register: Option<String>,
    /// Retry this task up to N times (total attempts = retries + 1) before giving up.
    /// Applies per `loop:` iteration if combined with `loop:`. Ignored in `--dry`.
    #[serde(default)]
    retries: Option<u32>,
    /// Seconds to wait between retry attempts (default 1 if `retries:` is set).
    #[serde(default)]
    delay: Option<u64>,
    /// Handler names (matching an entry in the playbook's `handlers:`) to trigger when
    /// this task succeeds and is considered "changed" (see `changed_when`). Deduplicated
    /// and run at most once each, after all regular tasks succeed.
    #[serde(default)]
    notify: Vec<String>,
    /// Condition (same syntax as `when:`) deciding whether this task's success counts as
    /// "changed" for `notify:` purposes. Absent means always changed on success —
    /// matches how a plain shell command has no built-in idempotency signal.
    #[serde(default)]
    changed_when: Option<String>,

    // Actions — only one should be set per task
    run: Option<String>,
    check_url: Option<String>,
    check_port: Option<CheckPortSpec>,
    env_check: Option<EnvCheckSpec>,
    ssh: Option<SshSpec>,
    fleet: Option<FleetSpec>,
    /// Run another whole playbook (by bare playbooks/ name, or a path relative to this
    /// playbook's own directory) as a single task. See `resolve_include_path`.
    include: Option<String>,
    /// Fails the task immediately (not skips) unless the condition (same syntax as
    /// `when:`) holds.
    assert: Option<String>,
    /// Run these tasks in order as a single unit; see `rescue`/`always`. Counts as one
    /// outcome in the parent's recap — its own tasks aren't flattened into the parent's
    /// totals (same scope line as `include:`).
    block: Option<Vec<Task>>,
    /// Run only if `block:` failed; if these succeed, the block is considered recovered.
    rescue: Option<Vec<Task>>,
    /// Always run after `block:`/`rescue:`, regardless of outcome; a failure here fails
    /// the block even after a successful rescue.
    always: Option<Vec<Task>>,
}

#[derive(Debug, Deserialize)]
struct SshSpec {
    /// Server profile name (see: tooler server list)
    server: String,
    command: String,
    #[serde(default)]
    sudo: bool,
}

#[derive(Debug, Deserialize)]
struct FleetSpec {
    /// Comma-separated server profile names (mutually exclusive with all/group)
    #[serde(default)]
    servers: Option<String>,
    /// Named server group (see: tooler group list)
    #[serde(default)]
    group: Option<String>,
    #[serde(default)]
    all: bool,
    command: String,
    #[serde(default)]
    sudo: bool,
    /// Run on all targeted servers concurrently instead of one at a time
    #[serde(default)]
    parallel: bool,
}

#[derive(Debug, Deserialize)]
struct CheckPortSpec {
    host: String,
    port: u16,
    #[serde(default = "default_timeout")]
    timeout: u64,
}

#[derive(Debug, Deserialize)]
struct EnvCheckSpec {
    reference: String,
    #[serde(default = "default_env_target")]
    target: String,
}

fn default_timeout() -> u64 {
    5
}
fn default_env_target() -> String {
    ".env".to_string()
}

// ── Entrypoint ────────────────────────────────────────────────────────────────

/// A playbook argument is a literal path (existing, unchanged behavior) if it contains a
/// `/` or already ends in `.yml`/`.yaml`; otherwise it's a bare name, resolved against
/// `<project_root>/playbooks/<name>.yml` (then `.yaml`).
fn is_literal_path(s: &str) -> bool {
    s.contains('/') || s.ends_with(".yml") || s.ends_with(".yaml")
}

fn resolve_playbook_file(file: &str, project_root: &Path) -> Result<PathBuf> {
    if is_literal_path(file) {
        return PathBuf::from(file)
            .canonicalize()
            .with_context(|| format!("Cannot resolve path: {file}"));
    }
    let dir = project_root.join("playbooks");
    for ext in ["yml", "yaml"] {
        let candidate = dir.join(format!("{file}.{ext}"));
        if candidate.exists() {
            return candidate
                .canonicalize()
                .with_context(|| format!("Cannot resolve path: {}", candidate.display()));
        }
    }
    bail!(
        "No playbook named '{file}' in {} (looked for {file}.yml, {file}.yaml).\n  \
         Create one with: tooler play --init {file}",
        dir.display()
    );
}

/// Companion runbook path for a resolved playbook file — same directory/stem, `.md`
/// extension. Works uniformly whether `file_path` came from `playbooks/<name>.yml` or a
/// literal path, since it's derived from the already-resolved path.
fn notes_path(file_path: &Path) -> PathBuf {
    file_path.with_extension("md")
}

fn read_notes(file_path: &Path) -> Option<String> {
    std::fs::read_to_string(notes_path(file_path)).ok()
}

fn print_notes_only(file_path: &Path, ctx: &Context) -> Result<()> {
    let notes = read_notes(file_path);
    if ctx.output == OutputFormat::Json {
        println!(
            "{}",
            serde_json::json!({"file": file_path.display().to_string(), "notes": notes})
        );
        return Ok(());
    }
    match notes {
        Some(n) => println!("{}", n.trim_end()),
        None => println!("{}", "No notes for this playbook.".dimmed()),
    }
    Ok(())
}

/// Mostly-static, per-run execution context threaded through the dispatch chain —
/// bundled into one struct because the parameter list (playbook_dir, project_root, dry,
/// quiet, ctx, plus mutable vars/include_stack passed alongside) got too long to stay
/// readable as positional args once `register:`/`include:` needed threading through too.
struct RunEnv<'a> {
    playbook_dir: PathBuf,
    project_root: PathBuf,
    dry: bool,
    quiet: bool,
    ctx: &'a Context,
}

pub fn run(args: PlayArgs, ctx: &Context) -> Result<()> {
    let (_, project_root) = project::load()?;
    let playbooks_dir = project_root.join("playbooks");

    if args.init {
        let name = args.file.as_deref().unwrap_or("playbook");
        let path = if is_literal_path(name) {
            PathBuf::from(name)
        } else {
            std::fs::create_dir_all(&playbooks_dir)?;
            playbooks_dir.join(format!("{name}.yml"))
        };
        return write_sample(&path, ctx);
    }

    let Some(file) = args.file.as_deref() else {
        return list_playbooks(&playbooks_dir, ctx);
    };

    let file_path = resolve_playbook_file(file, &project_root)?;

    if args.notes {
        return print_notes_only(&file_path, ctx);
    }
    let notes = read_notes(&file_path);

    let playbook_dir = file_path.parent().unwrap_or(Path::new(".")).to_path_buf();

    let content = std::fs::read_to_string(&file_path)
        .with_context(|| format!("Cannot read playbook: {file}"))?;

    let mut playbook: Playbook =
        serde_yaml::from_str(&content).with_context(|| format!("Invalid YAML in {file}"))?;

    // Merge CLI --var overrides into playbook vars
    for var in &args.vars {
        if let Some((k, v)) = var.split_once('=') {
            playbook
                .vars
                .insert(k.trim().to_string(), v.trim().to_string());
        } else {
            bail!("--var must be in key=value format, got: '{var}'");
        }
    }

    let tag_filter: Option<Vec<&str>> = args
        .tags
        .as_deref()
        .map(|t| t.split(',').map(str::trim).collect());

    let mut vars = playbook.vars.clone();
    let mut include_stack: Vec<PathBuf> = vec![file_path];
    let env = RunEnv {
        playbook_dir,
        project_root,
        dry: args.dry,
        quiet: ctx.output == OutputFormat::Json,
        ctx,
    };

    execute_playbook(
        &playbook,
        &tag_filter,
        &notes,
        &mut vars,
        &mut include_stack,
        true,
        &env,
    )
}

// ── Runner ────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct TaskOutcome {
    name: String,
    status: &'static str,
    error: Option<String>,
}

/// Rejects a `notify:` name with no matching `playbook.handlers` entry upfront, rather
/// than silently never running it — walks into `block:`/`rescue:`/`always:` too, since
/// those tasks can `notify:` just like any other.
fn validate_handlers(playbook: &Playbook) -> Result<()> {
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
        }
        Ok(())
    }
    walk(&playbook.tasks, &handler_names)
}

#[allow(clippy::too_many_arguments)]
fn execute_playbook(
    playbook: &Playbook,
    tag_filter: &Option<Vec<&str>>,
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

    let tasks: Vec<&Task> = playbook
        .tasks
        .iter()
        .filter(|t| match tag_filter {
            None => true,
            Some(tags) => t.tags.iter().any(|tag| tags.contains(&tag.as_str())),
        })
        .collect();

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
                error: None,
            });
            continue;
        }

        let result = run_task(task, vars, include_stack, env);

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
                        error: None,
                    });
                } else {
                    ok += 1;
                    outcomes.push(TaskOutcome {
                        name: task.name.clone(),
                        status: "ok",
                        error: None,
                    });

                    let changed = match &task.changed_when {
                        Some(expr) => eval_when(expr, vars),
                        None => true,
                    };
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
                if task.ignore_errors {
                    if !json {
                        println!(
                            "  {} {} — {}",
                            "!".yellow().bold(),
                            "failed (ignored):".yellow(),
                            e
                        );
                    }
                    skipped += 1;
                    outcomes.push(TaskOutcome {
                        name: task.name.clone(),
                        status: "ignored",
                        error: Some(e.to_string()),
                    });
                } else {
                    failed += 1;
                    outcomes.push(TaskOutcome {
                        name: task.name.clone(),
                        status: "failed",
                        error: Some(e.to_string()),
                    });

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
                                "success": false,
                            })
                        );
                        if is_top_level {
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
                    print_recap(ok, failed, skipped);
                    bail!("playbook failed");
                }
            }
        }
    }

    // Reached only if every regular task above succeeded (or was skipped/ignored) —
    // any unhandled failure already returned or exited above. Handlers use the same
    // failure-reporting shape as a regular task failure, addressed to the handler
    // instead of an indexed task.
    for name in &notified {
        let handler = playbook
            .handlers
            .iter()
            .find(|h| &h.name == name)
            .expect("validated by validate_handlers");

        if !json {
            println!("\n{} [{}]", "HANDLER".bold().magenta(), handler.name.bold());
        }

        match run_task(handler, vars, include_stack, env) {
            Ok(()) => {
                ok += 1;
                outcomes.push(TaskOutcome {
                    name: handler.name.clone(),
                    status: "ok",
                    error: None,
                });
            }
            Err(e) => {
                failed += 1;
                outcomes.push(TaskOutcome {
                    name: handler.name.clone(),
                    status: "failed",
                    error: Some(e.to_string()),
                });

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
                            "success": false,
                        })
                    );
                    if is_top_level {
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
                print_recap(ok, failed, skipped);
                bail!("playbook failed");
            }
        }
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
                "success": true,
            })
        );
        return Ok(());
    }

    println!("\n{}", sep.dimmed());
    print_recap(ok, failed, skipped);
    Ok(())
}

/// A minimal condition language over `render()`-substituted strings: "<a> == <b>",
/// "<a> != <b>", or a bare truthy check. Not a full expression language — matches
/// tooler's existing plain `{{var}}` templating rather than adding a new one.
fn eval_when(expr: &str, vars: &HashMap<String, String>) -> bool {
    let rendered = render(expr, vars);
    let rendered = rendered.trim();
    if let Some((lhs, rhs)) = rendered.split_once("!=") {
        return lhs.trim() != rhs.trim();
    }
    if let Some((lhs, rhs)) = rendered.split_once("==") {
        return lhs.trim() == rhs.trim();
    }
    !rendered.is_empty() && rendered != "false" && rendered != "0"
}

/// Expands `loop:` (if present) into one retried-`run_task_once` call per item, with
/// `{{item}}` added to that iteration's vars. The first failing iteration (after its own
/// retries are exhausted) fails the whole task — remaining items are not attempted. If
/// `register:` is set, only the *last* iteration's captured value persists into the
/// outer `vars` (simplest well-defined rule for a loop+register combination).
fn run_task(
    task: &Task,
    vars: &mut HashMap<String, String>,
    include_stack: &mut Vec<PathBuf>,
    env: &RunEnv,
) -> Result<()> {
    let Some(items) = &task.loop_items else {
        return run_task_once_with_retries(task, vars, include_stack, env);
    };
    for item in items {
        let mut loop_vars = vars.clone();
        loop_vars.insert("item".to_string(), item.clone());
        if !env.quiet {
            println!("  {} item={}", "→".dimmed(), item.dimmed());
        }
        run_task_once_with_retries(task, &mut loop_vars, include_stack, env)?;
        if let Some(reg) = &task.register
            && let Some(val) = loop_vars.get(reg)
        {
            vars.insert(reg.clone(), val.clone());
        }
    }
    Ok(())
}

/// Retries a single (non-loop-expanded) task invocation up to `task.retries` extra times,
/// waiting `task.delay` (default 1s) between attempts. A no-op wrapper when `retries:`
/// isn't set (attempts=1) or in `--dry` (nothing ever fails in dry mode, since every
/// action's real work is itself gated on `!env.dry`).
fn run_task_once_with_retries(
    task: &Task,
    vars: &mut HashMap<String, String>,
    include_stack: &mut Vec<PathBuf>,
    env: &RunEnv,
) -> Result<()> {
    let attempts = task.retries.unwrap_or(0) + 1;
    for attempt in 1..=attempts {
        match run_task_once(task, vars, include_stack, env) {
            Ok(()) => return Ok(()),
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
            Err(e) => return Err(e),
        }
    }
    unreachable!("loop always returns on the last attempt")
}

/// Resolves an `include:` target the same way a top-level playbook argument is
/// resolved, except a literal path is relative to *this playbook's own directory*
/// (`env.playbook_dir`, matching how `run:`/`env_check:` paths already resolve) rather
/// than the process's CWD — `include: ./helpers/build.yml` means "next to me."
fn resolve_include_path(file: &str, env: &RunEnv) -> Result<PathBuf> {
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
fn run_task_sequence(
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
fn run_block(
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

fn run_task_once(
    task: &Task,
    vars: &mut HashMap<String, String>,
    include_stack: &mut Vec<PathBuf>,
    env: &RunEnv,
) -> Result<()> {
    if task.register.is_some()
        && (task.check_url.is_some()
            || task.check_port.is_some()
            || task.env_check.is_some()
            || task.include.is_some()
            || task.assert.is_some()
            || task.block.is_some())
    {
        bail!(
            "register: is not supported for check_url/check_port/env_check/include/assert/block tasks"
        );
    }

    if let Some(expr) = &task.assert {
        if !eval_when(expr, vars) {
            bail!("assertion failed: {expr}");
        }
        return Ok(());
    }

    if let Some(block_tasks) = &task.block {
        return run_block(
            block_tasks,
            task.rescue.as_deref().unwrap_or(&[]),
            task.always.as_deref().unwrap_or(&[]),
            vars,
            include_stack,
            env,
        );
    }

    if let Some(cmd) = &task.run {
        let rendered_cmd = render(cmd, vars);
        if !env.quiet {
            println!("  {} {}", "$".bold().green(), rendered_cmd.dimmed());
        }
        if !env.dry {
            let start = Instant::now();
            let (success, code, captured) = if task.register.is_some() {
                let output = std::process::Command::new("sh")
                    .arg("-c")
                    .arg(&rendered_cmd)
                    .current_dir(&env.playbook_dir)
                    .output()?;
                let captured = String::from_utf8_lossy(&output.stdout).trim().to_string();
                (
                    output.status.success(),
                    output.status.code(),
                    Some(captured),
                )
            } else {
                let status = std::process::Command::new("sh")
                    .arg("-c")
                    .arg(&rendered_cmd)
                    .current_dir(&env.playbook_dir)
                    .status()?;
                (status.success(), status.code(), None)
            };
            let elapsed = start.elapsed();
            if success {
                if !env.quiet {
                    println!(
                        "  {} {}",
                        "✓ ok".green().bold(),
                        format!("({:.1}s)", elapsed.as_secs_f32()).dimmed()
                    );
                }
                if let (Some(reg), Some(val)) = (&task.register, captured) {
                    vars.insert(reg.clone(), val);
                }
            } else {
                bail!("command exited with code {}", code.unwrap_or(1));
            }
        }
        return Ok(());
    }

    if let Some(url) = &task.check_url {
        let url = render(url, vars);
        if !env.quiet {
            println!("  {} {}", "→".bold(), url.dimmed());
        }
        if !env.dry {
            check_url(&url, env.quiet)?;
        }
        return Ok(());
    }

    if let Some(spec) = &task.check_port {
        let host = render(&spec.host, vars);
        if !env.quiet {
            println!("  {} {}:{}", "→".bold(), host.dimmed(), spec.port);
        }
        if !env.dry {
            check_port(&host, spec.port, spec.timeout, env.quiet)?;
        }
        return Ok(());
    }

    if let Some(spec) = &task.env_check {
        let reference = env.playbook_dir.join(render(&spec.reference, vars));
        let target = env.playbook_dir.join(render(&spec.target, vars));
        if !env.quiet {
            println!(
                "  {} {} → {}",
                "→".bold(),
                reference.display().to_string().dimmed(),
                target.display().to_string().dimmed()
            );
        }
        if !env.dry {
            env_check(
                &reference.to_string_lossy(),
                &target.to_string_lossy(),
                env.quiet,
            )?;
        }
        return Ok(());
    }

    if let Some(spec) = &task.ssh {
        let server_name = render(&spec.server, vars);
        let cmd = render(&spec.command, vars);
        let full_cmd = crate::commands::fleet::exec_command(&cmd, spec.sudo);
        if !env.quiet {
            println!(
                "  {} {} {} {}",
                "→".bold(),
                full_cmd.dimmed(),
                "on".dimmed(),
                server_name.dimmed()
            );
        }
        if !env.dry {
            let server = crate::commands::ssh::resolve_server(env.ctx, &server_name)?;
            let (stdout, stderr, success) =
                crate::db::ssh_exec_capture_lenient(&server, &full_cmd)?;
            if success {
                let out = stdout.trim();
                if !env.quiet && !out.is_empty() {
                    println!("{out}");
                }
                if let Some(reg) = &task.register {
                    vars.insert(reg.clone(), out.to_string());
                }
            } else {
                let err = stderr.trim();
                bail!(
                    "{}",
                    if err.is_empty() {
                        "command failed"
                    } else {
                        err
                    }
                );
            }
        }
        return Ok(());
    }

    if let Some(spec) = &task.fleet {
        let command = render(&spec.command, vars);
        if !env.quiet {
            println!("  {} {}", "→".bold(), command.dimmed());
        }
        if !env.dry {
            let results = crate::commands::fleet::run_on_targets(
                env.ctx,
                spec.servers.as_deref(),
                spec.all,
                spec.group.as_deref(),
                &command,
                spec.sudo,
                spec.parallel,
            )?;
            let ok_count = results.iter().filter(|r| r.success).count();
            let total = results.len();
            let all_succeeded = ok_count == total;
            if !env.quiet {
                for r in &results {
                    if r.success {
                        println!("    {} {}", "✓".green().bold(), r.server.cyan());
                    } else {
                        println!("    {} {}", "✗".red().bold(), r.server.cyan());
                    }
                }
            }
            if let Some(reg) = &task.register {
                vars.insert(reg.clone(), format!("{ok_count}/{total}"));
            }
            if !all_succeeded {
                bail!("{ok_count}/{total} servers succeeded");
            }
        }
        return Ok(());
    }

    if let Some(include_file) = &task.include {
        let rendered = render(include_file, vars);
        let include_path = resolve_include_path(&rendered, env)?;
        if include_stack.contains(&include_path) {
            let chain = include_stack
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(" -> ");
            bail!(
                "include cycle detected: '{}' is already being included ({chain})",
                include_path.display()
            );
        }
        if !env.quiet {
            println!(
                "  {} include {}",
                "→".bold(),
                include_path.display().to_string().dimmed()
            );
        }
        if !env.dry {
            let content = std::fs::read_to_string(&include_path).with_context(|| {
                format!("Cannot read included playbook: {}", include_path.display())
            })?;
            let sub_playbook: Playbook = serde_yaml::from_str(&content)
                .with_context(|| format!("Invalid YAML in {}", include_path.display()))?;
            for (k, v) in &sub_playbook.vars {
                vars.entry(k.clone()).or_insert_with(|| v.clone());
            }
            let sub_notes = read_notes(&include_path);
            let sub_env = RunEnv {
                playbook_dir: include_path
                    .parent()
                    .unwrap_or(Path::new("."))
                    .to_path_buf(),
                project_root: env.project_root.clone(),
                dry: env.dry,
                quiet: env.quiet,
                ctx: env.ctx,
            };
            include_stack.push(include_path);
            let result = execute_playbook(
                &sub_playbook,
                &None,
                &sub_notes,
                vars,
                include_stack,
                false,
                &sub_env,
            );
            include_stack.pop();
            result?;
        }
        return Ok(());
    }

    bail!(
        "task '{}' has no action (run, check_url, check_port, env_check, ssh, fleet, include)",
        task.name
    );
}

// ── Actions ───────────────────────────────────────────────────────────────────

fn check_url(url: &str, quiet: bool) -> Result<()> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;
    match client.get(url).send() {
        Ok(r) if r.status().is_success() => {
            if !quiet {
                println!(
                    "  {} {} ({})",
                    "✓".green().bold(),
                    url,
                    r.status().as_u16().to_string().green()
                );
            }
            Ok(())
        }
        Ok(r) => bail!("HTTP {}", r.status().as_u16()),
        Err(e) => bail!("{e}"),
    }
}

fn check_port(host: &str, port: u16, timeout_secs: u64, quiet: bool) -> Result<()> {
    use std::net::ToSocketAddrs;
    let addr = format!("{host}:{port}");
    let socket = addr
        .to_socket_addrs()
        .with_context(|| format!("Cannot resolve '{addr}'"))?
        .next()
        .with_context(|| format!("No address for '{addr}'"))?;

    std::net::TcpStream::connect_timeout(&socket, Duration::from_secs(timeout_secs))
        .map(|_| {
            if !quiet {
                println!("  {} {host}:{port} is open", "✓".green().bold());
            }
        })
        .map_err(|e| anyhow::anyhow!("{host}:{port} — {e}"))
}

fn env_check(reference: &str, target: &str, quiet: bool) -> Result<()> {
    let parse = |path: &str| -> Result<std::collections::HashSet<String>> {
        let content =
            std::fs::read_to_string(path).with_context(|| format!("Cannot read {path}"))?;
        Ok(content
            .lines()
            .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
            .filter_map(|l| l.split_once('=').map(|(k, _)| k.trim().to_string()))
            .collect())
    };

    let ref_keys = parse(reference)?;
    let tgt_keys = parse(target)?;
    let mut missing: Vec<&String> = ref_keys.iter().filter(|k| !tgt_keys.contains(*k)).collect();

    if missing.is_empty() {
        if !quiet {
            println!("  {} all keys present in {target}", "✓".green().bold());
        }
        Ok(())
    } else {
        missing.sort();
        bail!(
            "missing in {target}: {}",
            missing
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Resolves a single `{{token}}`: the playbook's own vars first, then `env.<name>` (the
/// process environment) and `secret.<profile>.<key>` (the OS keychain, same store as
/// `tooler config set profile.<name>.token`/OAuth2 profiles — see `secrets::get_secret`).
/// `None` means "leave the token literal" — covers both a genuinely unknown name and a
/// secret lookup that failed (unset key, or the keychain itself being unreachable), so a
/// playbook never crashes over a missing secret, it just doesn't get substituted.
fn resolve_token(token: &str, vars: &HashMap<String, String>) -> Option<String> {
    if let Some(v) = vars.get(token) {
        return Some(v.clone());
    }
    if let Some(name) = token.strip_prefix("env.") {
        return std::env::var(name).ok();
    }
    if let Some(rest) = token.strip_prefix("secret.") {
        let (profile, key) = rest.split_once('.')?;
        return crate::secrets::get_secret(profile, key).ok().flatten();
    }
    None
}

/// Single-pass `{{token}}` substitution — see `resolve_token` for resolution order.
/// Unresolvable tokens are left exactly as written, same as the old known-vars-only
/// replace loop this superseded.
fn render(s: &str, vars: &HashMap<String, String>) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            out.push_str("{{");
            rest = after;
            continue;
        };
        let token = after[..end].trim();
        out.push_str(&resolve_token(token, vars).unwrap_or_else(|| format!("{{{{{token}}}}}")));
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    out
}

fn print_recap(ok: usize, failed: usize, skipped: usize) {
    println!(
        "\n{}  {}  {}  {}",
        "RECAP".bold(),
        format!("ok={ok}").green().bold(),
        if failed > 0 {
            format!("failed={failed}").red().bold()
        } else {
            format!("failed={failed}").dimmed()
        },
        format!("skipped={skipped}").dimmed(),
    );
}

// ── Sample playbook ───────────────────────────────────────────────────────────

fn write_sample(path: &Path, ctx: &Context) -> Result<()> {
    let json = ctx.output == OutputFormat::Json;
    let path_str = path.display().to_string();
    if path.exists() {
        let message = format!("'{}' already exists.", path_str);
        if json {
            println!(
                "{}",
                serde_json::json!({"path": path_str, "error": message})
            );
            std::process::exit(1);
        }
        bail!(message);
    }

    let sample = r#"name: My Playbook
description: Sample tooler playbook

vars:
  host: localhost
  port: "8080"
  env_file: .env

tasks:
  - name: Check env file is complete
    env_check:
      reference: .env.example
      target: "{{env_file}}"

  - name: Build project
    run: cargo build --release
    tags: [build]

  - name: Run tests
    run: cargo test
    tags: [test]
    ignore_errors: true

  - name: Health check
    check_url: http://{{host}}:{{port}}/health
    tags: [deploy]

  - name: Verify database port
    check_port:
      host: "{{host}}"
      port: 5432
      timeout: 3
    tags: [deploy]
"#;

    std::fs::write(path, sample)?;
    if json {
        println!("{}", serde_json::json!({"created": path_str}));
        return Ok(());
    }
    println!("{} {}", "created".green().bold(), path_str.cyan());
    let invocation =
        if path.parent().and_then(Path::file_name) == Some(std::ffi::OsStr::new("playbooks")) {
            path.file_stem().unwrap().to_string_lossy().to_string()
        } else {
            path_str
        };
    println!(
        "{}",
        format!("Run it with: tooler play {invocation}").dimmed()
    );
    Ok(())
}

// ── Listing ───────────────────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
struct PlaybookHeader {
    #[serde(default)]
    description: Option<String>,
}

fn list_playbooks(dir: &Path, ctx: &Context) -> Result<()> {
    let json = ctx.output == OutputFormat::Json;

    let mut entries: Vec<(String, PathBuf)> = Vec::new();
    if dir.is_dir() {
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            if path.extension().is_some_and(|e| e == "yml" || e == "yaml") {
                let name = path.file_stem().unwrap().to_string_lossy().to_string();
                entries.push((name, path));
            }
        }
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let describe = |path: &Path| -> Option<String> {
        let content = std::fs::read_to_string(path).ok()?;
        serde_yaml::from_str::<PlaybookHeader>(&content)
            .ok()?
            .description
    };

    if json {
        let playbooks: Vec<_> = entries
            .iter()
            .map(|(name, path)| {
                serde_json::json!({
                    "name": name,
                    "file": path.display().to_string(),
                    "description": describe(path),
                    "has_notes": notes_path(path).exists(),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::json!({"dir": dir.display().to_string(), "playbooks": playbooks})
        );
        return Ok(());
    }

    if entries.is_empty() {
        println!("{}", "No playbooks found.".dimmed());
        println!("Create one with: {}", "tooler play --init <name>".dimmed());
        return Ok(());
    }

    println!(
        "{} {}",
        "playbooks:".bold().cyan(),
        dir.display().to_string().dimmed()
    );
    println!("{}", "─".repeat(40).dimmed());
    for (name, path) in &entries {
        let desc = describe(path).unwrap_or_default();
        let notes_marker = if notes_path(path).exists() {
            " [notes]".dimmed().to_string()
        } else {
            String::new()
        };
        println!("  {:20} {}{}", name.bold(), desc.dimmed(), notes_marker);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_literal_path_detects_slash_and_extensions() {
        assert!(is_literal_path("playbook.yml"));
        assert!(is_literal_path("playbook.yaml"));
        assert!(is_literal_path("./playbooks/deploy.yml"));
        assert!(is_literal_path("sub/deploy"));
    }

    #[test]
    fn is_literal_path_false_for_bare_names() {
        assert!(!is_literal_path("deploy"));
        assert!(!is_literal_path("smoke-test"));
    }

    fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn eval_when_equality() {
        let v = vars(&[("env", "prod")]);
        assert!(eval_when("{{env}} == prod", &v));
        assert!(!eval_when("{{env}} == staging", &v));
    }

    #[test]
    fn eval_when_inequality() {
        let v = vars(&[("env", "prod")]);
        assert!(eval_when("{{env}} != staging", &v));
        assert!(!eval_when("{{env}} != prod", &v));
    }

    #[test]
    fn eval_when_truthy_bare_value() {
        let v = vars(&[("enabled", "yes")]);
        assert!(eval_when("{{enabled}}", &v));
        let v = vars(&[("enabled", "false")]);
        assert!(!eval_when("{{enabled}}", &v));
        let v = vars(&[("enabled", "")]);
        assert!(!eval_when("{{enabled}}", &v));
    }

    #[test]
    fn notes_path_swaps_yml_extension_for_md() {
        assert_eq!(
            notes_path(Path::new("playbooks/deploy.yml")),
            PathBuf::from("playbooks/deploy.md")
        );
        assert_eq!(
            notes_path(Path::new("playbooks/deploy.yaml")),
            PathBuf::from("playbooks/deploy.md")
        );
    }

    #[test]
    fn register_is_rejected_for_check_and_include_actions() {
        let ctx = Context::new(
            OutputFormat::Json,
            "default".to_string(),
            crate::config::Config::default(),
        );
        let env = RunEnv {
            playbook_dir: PathBuf::from("."),
            project_root: PathBuf::from("."),
            dry: true,
            quiet: true,
            ctx: &ctx,
        };
        let mut vars = HashMap::new();
        let mut include_stack = Vec::new();

        let task = Task {
            register: Some("x".to_string()),
            check_url: Some("http://example.com".to_string()),
            ..Default::default()
        };
        assert!(run_task_once(&task, &mut vars, &mut include_stack, &env).is_err());

        let task = Task {
            register: Some("x".to_string()),
            include: Some("sub.yml".to_string()),
            ..Default::default()
        };
        assert!(run_task_once(&task, &mut vars, &mut include_stack, &env).is_err());
    }

    #[test]
    fn register_is_allowed_for_run_action() {
        let ctx = Context::new(
            OutputFormat::Json,
            "default".to_string(),
            crate::config::Config::default(),
        );
        // dry: true — no real subprocess runs, so this only exercises the upfront
        // register-validation guard, not actual command execution.
        let env = RunEnv {
            playbook_dir: PathBuf::from("."),
            project_root: PathBuf::from("."),
            dry: true,
            quiet: true,
            ctx: &ctx,
        };
        let mut vars = HashMap::new();
        let mut include_stack = Vec::new();
        let task = Task {
            register: Some("x".to_string()),
            run: Some("echo hi".to_string()),
            ..Default::default()
        };
        assert!(run_task_once(&task, &mut vars, &mut include_stack, &env).is_ok());
    }

    fn dry_env(ctx: &Context) -> RunEnv<'_> {
        RunEnv {
            playbook_dir: PathBuf::from("."),
            project_root: PathBuf::from("."),
            dry: true,
            quiet: true,
            ctx,
        }
    }

    #[test]
    fn assert_true_succeeds_and_assert_false_fails() {
        let ctx = Context::new(
            OutputFormat::Json,
            "default".to_string(),
            crate::config::Config::default(),
        );
        let env = dry_env(&ctx);
        let mut vars = vars(&[("env", "prod")]);
        let mut include_stack = Vec::new();

        let task = Task {
            assert: Some("{{env}} == prod".to_string()),
            ..Default::default()
        };
        assert!(run_task_once(&task, &mut vars, &mut include_stack, &env).is_ok());

        let task = Task {
            assert: Some("{{env}} == staging".to_string()),
            ..Default::default()
        };
        assert!(run_task_once(&task, &mut vars, &mut include_stack, &env).is_err());
    }

    #[test]
    fn render_still_substitutes_known_vars() {
        let v = vars(&[("name", "world")]);
        assert_eq!(render("hello {{name}}", &v), "hello world");
    }

    #[test]
    fn render_leaves_unknown_and_unresolvable_tokens_literal() {
        let v = vars(&[]);
        assert_eq!(render("{{nope}}", &v), "{{nope}}");
        // A profile/key that was never stored resolves to None the same way an unknown
        // plain var does — never crashes, just leaves the token as-is.
        assert_eq!(
            render("{{secret.nonexistent_profile.nonexistent_key}}", &v),
            "{{secret.nonexistent_profile.nonexistent_key}}"
        );
    }

    #[test]
    fn validate_handlers_rejects_unknown_notify_target() {
        let playbook = Playbook {
            name: "t".to_string(),
            description: None,
            vars: HashMap::new(),
            handlers: vec![],
            tasks: vec![Task {
                name: "t1".to_string(),
                notify: vec!["nonexistent_handler".to_string()],
                ..Default::default()
            }],
        };
        assert!(validate_handlers(&playbook).is_err());
    }

    #[test]
    fn validate_handlers_accepts_a_matching_notify_target() {
        let playbook = Playbook {
            name: "t".to_string(),
            description: None,
            vars: HashMap::new(),
            handlers: vec![Task {
                name: "restart".to_string(),
                ..Default::default()
            }],
            tasks: vec![Task {
                name: "t1".to_string(),
                notify: vec!["restart".to_string()],
                ..Default::default()
            }],
        };
        assert!(validate_handlers(&playbook).is_ok());
    }
}
