//! `--lint`'s Checks A–D (shell-injection taint, undefined vars, unconfigured server/
//! mail/group profiles, and static notify:/include:/vars_files: resolution) and
//! `--explain` (resolve every task's vars and print the concrete action it would take).
use super::*;
use anyhow::Result;
use colored::Colorize;
use std::collections::HashMap;
use std::path::Path;

// ── --lint ────────────────────────────────────────────────────────────────────

/// One `--lint` finding: `task` names which task it's about, `message` describes it.
#[derive(Debug, Serialize)]
pub(crate) struct LintFinding {
    pub(crate) task: String,
    pub(crate) message: String,
}

/// Register: source actions Check A treats as "untrusted" — a third-party API response,
/// a scraped page, DB rows an attacker could influence, or inbox content — same examples
/// the README's own Trust model section already calls out for `| quote`.
pub(crate) fn tainted_source_action(task: &Task) -> Option<&'static str> {
    if task.http.is_some() {
        Some("http")
    } else if task.scrape.is_some() {
        Some("scrape")
    } else if task.db_query.is_some() {
        Some("db_query")
    } else if task.mail_check.is_some() {
        Some("mail_check")
    } else {
        None
    }
}

/// The command string Check A scans for unquoted tainted tokens — the same three action
/// types the README's Trust model section names as reaching a shell line unescaped.
pub(crate) fn shell_command(task: &Task) -> Option<&str> {
    if let Some(spec) = &task.run {
        Some(match spec {
            RunSpec::Simple(s) => s.as_str(),
            RunSpec::Structured { command, .. } => command.as_str(),
        })
    } else if let Some(spec) = &task.ssh {
        Some(spec.command.as_str())
    } else if let Some(spec) = &task.fleet {
        Some(spec.command.as_str())
    } else {
        None
    }
}

/// The fields Check B scans for a `{{var}}` reference nothing defines — task-level
/// condition strings plus the same three shell-command fields `shell_command` covers.
/// Not exhaustive (doesn't scan e.g. `http: {url}`, `db_query: {sql}`, `write_file:
/// {content}`) — a deliberate scope limit to keep the heuristic simple and its false
/// positives predictable, documented in the README alongside `--lint`'s other caveats.
pub(crate) fn lintable_fields(task: &Task) -> Vec<&str> {
    let mut fields: Vec<&str> = shell_command(task).into_iter().collect();
    for s in [
        &task.debug,
        &task.when,
        &task.changed_when,
        &task.failed_when,
        &task.until,
    ]
    .into_iter()
    .flatten()
    {
        fields.push(s.as_str());
    }
    match &task.assert {
        Some(AssertSpec::Simple(expr)) => fields.push(expr.as_str()),
        Some(AssertSpec::Structured { that, .. }) => {
            fields.extend(that.iter().map(String::as_str));
        }
        None => {}
    }
    fields
}

/// Scans `s` for every `{{...}}` template token, in the same shape `render_with`'s
/// substitution loop parses them (`parse_pipeline` reuse) — but collecting `(token,
/// pipeline)` pairs instead of substituting. Shared by `--lint`'s checks.
pub(crate) fn find_tokens(s: &str) -> Vec<(&str, Vec<FilterOp<'_>>)> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            break;
        };
        out.push(parse_pipeline(after[..end].trim()));
        rest = &after[end + 2..];
    }
    out
}

/// The part of a `{{token}}` before its first `.` — `"resp.status"` -> `"resp"`, so a
/// dotted-key reference (`<reg>.status`/`<reg>.results`/`item.<field>`) is checked
/// against the same plain name `register:`/`set_fact:`/`loop:` would have added.
pub(crate) fn token_base_name(token: &str) -> &str {
    token.split('.').next().unwrap_or(token)
}

/// A literal `server:`/`group:` field on a task, tagged with which profile namespace to
/// check it against for Check C — see `task_profile_refs`.
pub(crate) enum ProfileRef<'a> {
    Ssh(&'a str),
    Mail(&'a str),
    Group(&'a str),
}

/// Every literal (non-`{{templated}}` — can't be statically resolved, same scope limit
/// Check B already has) server:/mail-profile/group reference on `task`, for Check C.
/// `ssh:`/`fs_cat:`/`fs_write:`/`systemd_restart:`/`systemd_status:`/`logs_tail:`/
/// `logs_grep:`/`ps_list:`/`ps_kill:`/`stat:`/`db_query:`/`db_exec:`/`sync_db:`/
/// `sync_files:`/`deploy:` all resolve their `server:` through `ctx.config.server` (see
/// `crate::commands::ssh::resolve_server`); `mail:`/`mail_check:` resolve theirs
/// through the separate `ctx.config.mail` — confirmed by reading each dispatch site,
/// not assumed, since they share the same field name but are different namespaces.
/// `fleet:`'s `servers:` is a comma-separated list of the *same* `ctx.config.server`
/// names, so each entry becomes its own `Ssh` ref; its `group:` is a genuinely
/// different namespace (`ctx.config.group`, confirmed via `fleet::resolve_targets`) and
/// gets `Group`. `fleet: {all: true}` needs no check at all — targeting every
/// configured server is always resolvable, nothing to look up.
pub(crate) fn task_profile_refs(task: &Task) -> Vec<ProfileRef<'_>> {
    let mut out = Vec::new();
    let lit = |s: &str| !s.contains("{{");
    macro_rules! ssh_ref {
        ($f:expr) => {
            if let Some(s) = $f
                && lit(s)
            {
                out.push(ProfileRef::Ssh(s));
            }
        };
    }
    ssh_ref!(task.ssh.as_ref().map(|s| s.server.as_str()));
    ssh_ref!(task.fs_cat.as_ref().map(|s| s.server.as_str()));
    ssh_ref!(task.fs_write.as_ref().map(|s| s.server.as_str()));
    ssh_ref!(task.systemd_restart.as_ref().map(|s| s.server.as_str()));
    ssh_ref!(task.systemd_status.as_ref().map(|s| s.server.as_str()));
    ssh_ref!(task.logs_tail.as_ref().map(|s| s.server.as_str()));
    ssh_ref!(task.logs_grep.as_ref().map(|s| s.server.as_str()));
    ssh_ref!(task.ps_list.as_ref().map(|s| s.server.as_str()));
    ssh_ref!(task.ps_kill.as_ref().map(|s| s.server.as_str()));
    ssh_ref!(task.stat.as_ref().map(|s| s.server.as_str()));
    ssh_ref!(task.db_query.as_ref().map(|s| s.server.as_str()));
    ssh_ref!(task.db_exec.as_ref().map(|s| s.server.as_str()));
    ssh_ref!(task.db_load.as_ref().map(|s| s.server.as_str()));
    ssh_ref!(task.sync_db.as_ref().map(|s| s.server.as_str()));
    ssh_ref!(task.sync_files.as_ref().map(|s| s.server.as_str()));
    ssh_ref!(task.deploy.as_ref().map(|s| s.server.as_str()));
    ssh_ref!(task.upload.as_ref().map(|s| s.server.as_str()));
    ssh_ref!(task.cron.as_ref().map(|s| s.server.as_str()));
    ssh_ref!(task.template.as_ref().and_then(|s| s.server.as_deref()));
    if let Some(spec) = &task.fleet {
        if let Some(servers) = &spec.servers
            && lit(servers)
        {
            for name in servers.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                out.push(ProfileRef::Ssh(name));
            }
        }
        if let Some(group) = &spec.group
            && lit(group)
        {
            out.push(ProfileRef::Group(group));
        }
    }
    if let Some(s) = &task.mail_check
        && lit(&s.server)
    {
        out.push(ProfileRef::Mail(&s.server));
    }
    if let Some(s) = &task.mail
        && let Some(server) = &s.server
        && lit(server)
    {
        out.push(ProfileRef::Mail(server));
    }
    out
}

/// `--lint`: static analysis over the parsed YAML only (same zero-side-effect parsing as
/// `--list-tasks`) — see `PlayArgs.lint`'s doc comment for what it checks. `playbook_dir`
/// is only used to best-effort read `vars_files:` entries for Check B; an unreadable or
/// vault-encrypted one (no `TOOLER_VAULT_PASSWORD` set) is skipped, never an error —
/// `--lint` never fails, it only ever reports findings. `ctx` is only used for Check C's
/// server:/mail-profile lookups (`ctx.config`, already in memory) and the OS keychain
/// existence probe (`secrets::get_secret`) -- both read-only, no connections, matching
/// the zero-side-effect promise every other `--lint` check already makes.
pub(crate) fn lint_playbook(
    playbook: &Playbook,
    playbook_dir: &Path,
    project_root: &Path,
    ctx: &Context,
) -> Vec<LintFinding> {
    let mut known: std::collections::HashSet<String> = playbook.vars.keys().cloned().collect();
    for vf in &playbook.vars_files {
        if let Ok(loaded) = load_vars_file(&playbook_dir.join(vf)) {
            known.extend(loaded.into_keys());
        }
    }
    let mut tainted: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut saw_include_vars = false;
    let mut findings = Vec::new();
    lint_tasks(
        &playbook.tasks,
        &mut known,
        &mut tainted,
        &mut saw_include_vars,
        &mut findings,
        ctx,
    );
    // on_failure: tasks run in the same var scope as the main run — lint them with the
    // running known/tainted state as it stands after every regular task.
    lint_tasks(
        &playbook.on_failure,
        &mut known,
        &mut tainted,
        &mut saw_include_vars,
        &mut findings,
        ctx,
    );
    check_d(playbook, playbook_dir, project_root, &mut findings);
    findings
}

/// Check D — static resolution of names and paths that only fail (or silently
/// mis-resolve) at run time: a `notify:` that matches no handler, and an `include:` /
/// `include_vars:` / `vars_files:` path that resolves to no file. Advisory only, like
/// every other `--lint` check.
pub(crate) fn check_d(
    playbook: &Playbook,
    playbook_dir: &Path,
    project_root: &Path,
    findings: &mut Vec<LintFinding>,
) {
    let handler_names: std::collections::HashSet<&str> =
        playbook.handlers.iter().map(|h| h.name.as_str()).collect();

    // `include:` resolution mirrors `resolve_include_path` — a bare name is looked up
    // under `<project_root>/playbooks/`, anything with a `/` or a `.yml`/`.yaml`
    // extension is treated as a path relative to this playbook's own directory.
    let include_resolves = |file: &str| -> bool {
        if is_literal_path(file) {
            playbook_dir.join(file).exists()
        } else {
            ["yml", "yaml"].iter().any(|ext| {
                project_root
                    .join("playbooks")
                    .join(format!("{file}.{ext}"))
                    .exists()
            })
        }
    };

    fn walk(
        tasks: &[Task],
        handler_names: &std::collections::HashSet<&str>,
        playbook_dir: &Path,
        include_resolves: &impl Fn(&str) -> bool,
        findings: &mut Vec<LintFinding>,
    ) {
        for task in tasks {
            for n in &task.notify {
                if !n.contains("{{") && !handler_names.contains(n.as_str()) {
                    findings.push(LintFinding {
                        task: task.name.clone(),
                        message: format!(
                            "notify: '{n}' matches no handler in handlers: -- this fails at runtime"
                        ),
                    });
                }
            }
            if let Some(spec) = &task.include {
                let file = spec.file();
                if !file.contains("{{") && !include_resolves(file) {
                    findings.push(LintFinding {
                        task: task.name.clone(),
                        message: format!("include: '{file}' resolves to no file"),
                    });
                }
            }
            if let Some(file) = &task.include_vars
                && !file.contains("{{")
                && !playbook_dir.join(file).exists()
            {
                findings.push(LintFinding {
                    task: task.name.clone(),
                    message: format!("include_vars: '{file}' not found"),
                });
            }
            for branch in [&task.block, &task.rescue, &task.always, &task.parallel]
                .into_iter()
                .flatten()
            {
                walk(
                    branch,
                    handler_names,
                    playbook_dir,
                    include_resolves,
                    findings,
                );
            }
        }
    }

    for tasks in [&playbook.tasks, &playbook.handlers, &playbook.on_failure] {
        walk(
            tasks,
            &handler_names,
            playbook_dir,
            &include_resolves,
            findings,
        );
    }

    for vf in &playbook.vars_files {
        if !playbook_dir.join(vf).exists() {
            findings.push(LintFinding {
                task: format!("vars_files: {vf}"),
                message: format!("vars_files: '{vf}' not found -- vars from it won't be available"),
            });
        }
    }
}

/// Walks `tasks` in execution order, threading the same running `known`/`tainted` state
/// into nested `block:`/`rescue:`/`always:` (all three, conservatively — only one branch
/// actually runs at a time, but none of them create a separate var scope at runtime, see
/// `run_block`/`run_task_sequence`, so a static lint errs toward fewer false positives by
/// assuming any of them could have). `include:` is never recursed into, same scoping
/// `--list-tasks` already has — a separate file, possibly not resolvable here.
pub(crate) fn lint_tasks(
    tasks: &[Task],
    known: &mut std::collections::HashSet<String>,
    tainted: &mut std::collections::HashSet<String>,
    saw_include_vars: &mut bool,
    findings: &mut Vec<LintFinding>,
    ctx: &Context,
) {
    for task in tasks {
        // Check B — skipped entirely once an include_vars: task has been seen, since its
        // target's contents aren't known statically and could define anything.
        if !*saw_include_vars {
            for field in lintable_fields(task) {
                for (token, _) in find_tokens(field) {
                    // A quoted string literal (`{{ '[1,2]' | sum }}`) isn't a var ref.
                    if token.starts_with('\'') || token.starts_with('"') {
                        continue;
                    }
                    let base = token_base_name(token);
                    if matches!(base, "item" | "batch" | "batch_index" | "batch_size") {
                        if task.loop_spec.is_none() {
                            findings.push(LintFinding {
                                task: task.name.clone(),
                                message: format!(
                                    "{{{{{token}}}}} referenced but this task has no loop:"
                                ),
                            });
                        }
                        continue;
                    }
                    // Check C (secret half) -- a full 3-part secret.<profile>.<key>
                    // token is checked against the real OS keychain; a malformed
                    // shorter form is left to render()'s own runtime handling, not
                    // this check's job. state:/env: stay blanket-skipped: state: has
                    // no "real environment" to check against, env: resolves from this
                    // process's own environment at render time -- a different kind of
                    // reference this check doesn't extend to.
                    if base == "secret" {
                        let parts: Vec<&str> = token.splitn(3, '.').collect();
                        if let [_, profile, key] = parts[..] {
                            match crate::secrets::get_secret(profile, key) {
                                Ok(Some(_)) => {}
                                Ok(None) => findings.push(LintFinding {
                                    task: task.name.clone(),
                                    message: format!(
                                        "{{{{{token}}}}} references a secret that isn't set -- \
                                         tooler config set secret.{profile}.{key} <value> (or secret_set: in a playbook)"
                                    ),
                                }),
                                // Keychain unavailable in this environment -- not the
                                // playbook's fault, don't flag it as a playbook problem.
                                Err(_) => {}
                            }
                        }
                        continue;
                    }
                    if base == "state" || base == "env" || base == "now" {
                        continue;
                    }
                    if !known.contains(base) {
                        findings.push(LintFinding {
                            task: task.name.clone(),
                            message: format!(
                                "{{{{{token}}}}} isn't defined by any earlier vars:/vars_files:/register:/set_fact: in this playbook — possible typo (or set by --var/--vars-file at run time, which --lint can't see)"
                            ),
                        });
                    }
                }
            }
        }

        // Check A — untrusted data reaching a shell command unquoted.
        if let Some(cmd) = shell_command(task) {
            for (token, pipeline) in find_tokens(cmd) {
                let base = token_base_name(token);
                let quoted = pipeline.iter().any(|f| matches!(f, FilterOp::Quote));
                if tainted.contains(base) && !quoted {
                    findings.push(LintFinding {
                        task: task.name.clone(),
                        message: format!(
                            "{{{{{token}}}}} comes from a registered result of an untrusted \
                             source and reaches run:/ssh:/fleet: without | quote — possible \
                             shell injection"
                        ),
                    });
                }
            }
        }

        // Check C (server:/mail-profile/group half) -- a literal server:/group: field
        // naming a profile or group this environment never configured.
        for r in task_profile_refs(task) {
            match r {
                ProfileRef::Ssh(name) if !ctx.config.server.contains_key(name) => {
                    findings.push(LintFinding {
                        task: task.name.clone(),
                        message: format!(
                            "server: '{name}' isn't a configured server profile -- tooler server add {name} ..."
                        ),
                    });
                }
                ProfileRef::Mail(name) if !ctx.config.mail.contains_key(name) => {
                    findings.push(LintFinding {
                        task: task.name.clone(),
                        message: format!(
                            "server: '{name}' isn't a configured mail profile -- tooler config set mail.{name}.host ..."
                        ),
                    });
                }
                ProfileRef::Group(name) if !ctx.config.group.contains_key(name) => {
                    findings.push(LintFinding {
                        task: task.name.clone(),
                        message: format!(
                            "group: '{name}' isn't a configured group -- tooler group add {name} ..."
                        ),
                    });
                }
                _ => {}
            }
        }

        // Update the running state for tasks that come after this one.
        if let Some(reg) = &task.register {
            known.insert(reg.clone());
            if tainted_source_action(task).is_some() {
                tainted.insert(reg.clone());
            }
        }
        if let Some(facts) = &task.set_fact {
            known.extend(facts.keys().cloned());
        }
        if task.include_vars.is_some() {
            *saw_include_vars = true;
        }

        if let Some(block) = &task.block {
            lint_tasks(block, known, tainted, saw_include_vars, findings, ctx);
        }
        if let Some(rescue) = &task.rescue {
            lint_tasks(rescue, known, tainted, saw_include_vars, findings, ctx);
        }
        if let Some(always) = &task.always {
            lint_tasks(always, known, tainted, saw_include_vars, findings, ctx);
        }
        if let Some(parallel) = &task.parallel {
            lint_tasks(parallel, known, tainted, saw_include_vars, findings, ctx);
        }
    }
}

pub(crate) fn print_lint_findings(
    playbook_name: &str,
    findings: &[LintFinding],
    ctx: &Context,
) -> Result<()> {
    if ctx.output == OutputFormat::Json {
        println!(
            "{}",
            serde_json::json!({"playbook": playbook_name, "findings": findings})
        );
        return Ok(());
    }
    println!(
        "{} {}",
        "PLAY".bold().cyan(),
        format!("[{playbook_name}]").bold()
    );
    if findings.is_empty() {
        println!("{}", "No issues found.".green());
    } else {
        for f in findings {
            println!("  {} {}: {}", "!".yellow().bold(), f.task.bold(), f.message);
        }
        println!(
            "\n{}",
            format!(
                "{} finding(s) — advisory only, review before running",
                findings.len()
            )
            .yellow()
        );
    }
    Ok(())
}

/// `--explain`: walks the playbook's tasks (and nested `block:`/`rescue:`/`always:`/
/// `parallel:`), resolving each one's `{{vars}}` and printing the concrete action it
/// would take, without running anything. `set_fact:`/`state_set:` values are threaded
/// forward so later tasks resolve against them; a `register:`ed value can't be known
/// statically, so it stays literal.
pub(crate) fn print_explain(
    playbook: &Playbook,
    playbook_dir: &Path,
    project_root: &Path,
    vars: &mut HashMap<String, String>,
    ctx: &Context,
) -> Result<()> {
    fn walk(
        tasks: &[Task],
        vars: &mut HashMap<String, String>,
        playbook_dir: &Path,
        project_root: &Path,
        out: &mut Vec<serde_json::Value>,
    ) {
        for task in tasks {
            let mut entry = serde_json::Map::new();
            entry.insert("name".into(), task.name.clone().into());
            entry.insert("action".into(), task_action_label(task).into());
            if let Some(w) = &task.when {
                entry.insert("when".into(), render(w, vars).into());
            }
            entry.insert(
                "explain".into(),
                explain_task(task, vars, playbook_dir, project_root),
            );
            out.push(serde_json::Value::Object(entry));

            // Thread forward the two actions whose effect is knowable statically.
            if let Some(facts) = &task.set_fact {
                for (k, v) in facts {
                    vars.insert(k.clone(), render(v, vars));
                }
            }
            if let Some(state) = &task.state_set {
                for (k, v) in state {
                    vars.insert(format!("state.{k}"), render(v, vars));
                }
            }
            for branch in [&task.block, &task.rescue, &task.always, &task.parallel]
                .into_iter()
                .flatten()
            {
                walk(branch, vars, playbook_dir, project_root, out);
            }
        }
    }

    let mut tasks_out = Vec::new();
    walk(
        &playbook.tasks,
        vars,
        playbook_dir,
        project_root,
        &mut tasks_out,
    );
    let doc = serde_json::json!({"playbook": playbook.name, "tasks": tasks_out});

    if ctx.output == OutputFormat::Json {
        println!("{doc}");
        return Ok(());
    }
    println!(
        "{} {}",
        "PLAY".bold().cyan(),
        format!("[{}]", playbook.name).bold()
    );
    for t in &tasks_out {
        println!(
            "\n{} {} {}",
            "•".cyan(),
            t["name"].as_str().unwrap_or("").bold(),
            format!("({})", t["action"].as_str().unwrap_or("")).dimmed()
        );
        if let Some(w) = t.get("when").and_then(|w| w.as_str()) {
            println!("  when: {w}");
        }
        if let Some(obj) = t["explain"].as_object() {
            for (k, v) in obj {
                let vs = match v {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                println!("  {}: {}", k.dimmed(), vs);
            }
        }
    }
    Ok(())
}

/// The concrete, `{{var}}`-resolved fields of one task's action, for `--explain`. A
/// best-effort match — the actions an agent most needs to see before a run get real
/// detail; anything else returns an empty object (the task still lists its name/action).
pub(crate) fn explain_task(
    task: &Task,
    vars: &HashMap<String, String>,
    playbook_dir: &Path,
    project_root: &Path,
) -> serde_json::Value {
    use serde_json::json;
    let r = |s: &str| render(s, vars);

    if let Some(spec) = &task.run {
        let (cmd, env) = match spec {
            RunSpec::Simple(c) => (r(c), serde_json::Map::new()),
            RunSpec::Structured { command, env } => (
                r(command),
                env.iter().map(|(k, v)| (k.clone(), r(v).into())).collect(),
            ),
        };
        return json!({"command": cmd, "env": serde_json::Value::Object(env)});
    }
    if let Some(spec) = &task.ssh {
        return json!({"server": r(&spec.server), "command": r(&spec.command), "sudo": spec.sudo});
    }
    if let Some(spec) = &task.fleet {
        let target = spec
            .servers
            .as_ref()
            .map(|s| format!("servers={}", r(s)))
            .or_else(|| spec.group.as_ref().map(|g| format!("group={}", r(g))))
            .unwrap_or_else(|| "all".into());
        return json!({"target": target, "command": r(&spec.command), "sudo": spec.sudo});
    }
    if let Some(spec) = &task.http {
        return json!({
            "method": spec.method.to_uppercase(),
            "url": r(&spec.url),
            "headers": spec.headers.keys().cloned().collect::<Vec<_>>(),
            "has_body": spec.body.is_some(),
            "paginated": spec.paginate.is_some(),
        });
    }
    if let Some(spec) = &task.db_query {
        return json!({"server": r(&spec.server), "sql": r(&spec.sql)});
    }
    if let Some(spec) = &task.db_exec {
        return json!({"server": r(&spec.server), "sql": r(&spec.sql)});
    }
    if let Some(spec) = &task.db_load {
        return json!({
            "server": r(&spec.server), "table": r(&spec.table), "file": r(&spec.file),
            "mode": format!("{:?}", spec.mode).to_lowercase(),
            "format": format!("{:?}", spec.format).to_lowercase(),
        });
    }
    if let Some(spec) = &task.deploy {
        let mut steps = Vec::new();
        if spec.pull {
            steps.push("pull");
        }
        if spec.build.is_some() {
            steps.push("build");
        }
        if spec.restart.is_some() {
            steps.push("restart");
        }
        if spec.health_url.is_some() {
            steps.push("health-check");
        }
        return json!({"server": r(&spec.server), "path": r(&spec.path), "steps": steps});
    }
    if let Some(spec) = &task.upload {
        return json!({"server": r(&spec.server), "local": r(&spec.local), "remote": r(&spec.remote)});
    }
    if let Some(spec) = &task.cron {
        let op = spec
            .add
            .as_ref()
            .map(|a| format!("add {}", r(a)))
            .or_else(|| spec.remove.as_ref().map(|p| format!("remove /{}/", r(p))))
            .unwrap_or_else(|| "list".into());
        return json!({"server": r(&spec.server), "op": op});
    }
    if let Some(spec) = &task.fs_write {
        return json!({"server": r(&spec.server), "path": r(&spec.path)});
    }
    if let Some(spec) = &task.fs_cat {
        return json!({"server": r(&spec.server), "path": r(&spec.path)});
    }
    if let Some(spec) = &task.systemd_restart {
        return json!({"server": r(&spec.server), "unit": r(&spec.unit), "sudo": spec.sudo});
    }
    if let Some(spec) = &task.sync_db {
        return json!({"server": r(&spec.server)});
    }
    if let Some(spec) = &task.sync_files {
        return json!({"server": r(&spec.server), "from": r(&spec.from), "to": r(&spec.to)});
    }
    if let Some(spec) = &task.write_file {
        return json!({"path": r(&spec.path), "append": spec.append});
    }
    if let Some(spec) = &task.template {
        return json!({
            "src": r(&spec.src),
            "dest": r(&spec.dest),
            "server": spec.server.as_deref().map(r),
        });
    }
    if let Some(spec) = &task.write_csv {
        return json!({"path": r(&spec.path)});
    }
    if let Some(spec) = &task.read_csv {
        return json!({"path": r(&spec.path)});
    }
    if let Some(spec) = &task.mail {
        return json!({"to": r(&spec.to), "subject": r(&spec.subject)});
    }
    if let Some(facts) = &task.set_fact {
        return json!(
            facts
                .iter()
                .map(|(k, v)| (k.clone(), serde_json::Value::from(r(v))))
                .collect::<serde_json::Map<_, _>>()
        );
    }
    if let Some(state) = &task.state_set {
        return json!(
            state
                .iter()
                .map(|(k, v)| (format!("state.{k}"), serde_json::Value::from(r(v))))
                .collect::<serde_json::Map<_, _>>()
        );
    }
    if let Some(msg) = &task.debug {
        return json!({ "message": r(msg) });
    }
    if let Some(spec) = &task.assert {
        let conds: Vec<String> = match spec {
            AssertSpec::Simple(c) => vec![r(c)],
            AssertSpec::Structured { that, .. } => that.iter().map(|c| r(c)).collect(),
        };
        return json!({ "conditions": conds });
    }
    if let Some(spec) = &task.include {
        let file = r(spec.file());
        let resolved = if is_literal_path(&file) {
            playbook_dir.join(&file).display().to_string()
        } else {
            project_root
                .join("playbooks")
                .join(format!("{file}.yml"))
                .display()
                .to_string()
        };
        return json!({ "file": resolved });
    }
    json!({})
}
