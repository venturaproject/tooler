//! `run_task_once` — the per-action-type dispatch every concrete task attempt funnels
//! through (one `if let Some(spec) = &task.<action>` arm per DSL action) — plus the
//! `exec_*` helpers it calls for the actions with real side effects, `confirm_gate`/
//! `task_action_label` (used by `--list-tasks`/`--audit-log`), and the secret-aware
//! error redaction `write_audit_entry` applies before logging (see `task_secret_values`).
use super::*;
use anyhow::{Result, anyhow, bail};
use colored::Colorize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// The only place `fs_write:` actually overwrites a remote file — requires
/// `&Confirmed<FsWriteSpec>`, obtainable only via `Confirmed::require`, so this can never
/// run against an unconfirmed spec. Returns the rendered content written, for the byte
/// count / `register:`.
pub(crate) fn exec_fs_write(
    confirmed: &Confirmed<FsWriteSpec>,
    path: &str,
    vars: &HashMap<String, String>,
    env: &RunEnv,
) -> Result<String> {
    let server_name = render(&confirmed.server, vars);
    let content = render(&confirmed.content, vars);
    let server = crate::commands::ssh::resolve_server(env.ctx, &server_name)?;
    if env.diff {
        // A nonexistent remote file is a legitimate "new file" diff (all-added) —
        // tolerate the cat failing rather than treat it as an error.
        let old = crate::db::ssh_exec_capture(&server, &crate::commands::fs::cat_cmd(path))
            .unwrap_or_default();
        print_diff_if_enabled(env, &old, &content);
    }
    let (_, stderr, success) = crate::db::ssh_exec_with_stdin(
        &server,
        &crate::commands::fs::write_cmd(path),
        content.as_bytes(),
    )?;
    if !success {
        let err = stderr.trim();
        bail!("{}", if err.is_empty() { "write failed" } else { err });
    }
    Ok(content)
}

/// Renders a `template:` task's `src` file with minijinja. Each playbook var is exposed
/// as parsed JSON where it parses (so `{% for r in rows %}` iterates a `register:`ed
/// array), otherwise as a plain string; `now` (RFC3339 UTC) is always available. A
/// template syntax or render error is surfaced verbatim (minijinja points at the line).
pub(crate) fn render_template(src: &Path, vars: &HashMap<String, String>) -> Result<String> {
    let template = std::fs::read_to_string(src)
        .with_context(|| format!("template: reading {}", src.display()))?;

    let mut ctx = serde_json::Map::new();
    for (k, v) in vars {
        let val = serde_json::from_str::<serde_json::Value>(v)
            .unwrap_or_else(|_| serde_json::Value::String(v.clone()));
        ctx.insert(k.clone(), val);
    }
    ctx.entry("now".to_string())
        .or_insert_with(|| serde_json::Value::String(chrono::Utc::now().to_rfc3339()));

    let mut env = minijinja::Environment::new();
    env.add_template("t", &template)
        .map_err(|e| anyhow!("template: {e}"))?;
    env.get_template("t")
        .and_then(|t| t.render(minijinja::Value::from_serialize(&ctx)))
        .map_err(|e| anyhow!("template: {e}"))
}

/// The only place `systemd_restart:` actually restarts a remote unit — see
/// `exec_fs_write`'s doc comment for why this takes `&Confirmed<SystemdRestartSpec>`.
pub(crate) fn exec_systemd_restart(
    confirmed: &Confirmed<SystemdRestartSpec>,
    unit: &str,
    vars: &HashMap<String, String>,
    env: &RunEnv,
) -> Result<()> {
    let server_name = render(&confirmed.server, vars);
    let sudo_pass = confirmed.sudo_pass.as_deref().map(|s| render(s, vars));
    let server = crate::commands::ssh::resolve_server(env.ctx, &server_name)?;
    crate::db::ssh_exec_capture(
        &server,
        &crate::commands::systemd::restart_cmd(unit, confirmed.sudo, sudo_pass.as_deref()),
    )?;
    Ok(())
}

/// The only place `ps_kill:` actually sends a signal to a remote process — see
/// `exec_fs_write`'s doc comment for why this takes `&Confirmed<PsKillSpec>`.
pub(crate) fn exec_ps_kill(
    confirmed: &Confirmed<PsKillSpec>,
    signal: &str,
    vars: &HashMap<String, String>,
    env: &RunEnv,
) -> Result<()> {
    let server_name = render(&confirmed.server, vars);
    let sudo_pass = confirmed.sudo_pass.as_deref().map(|s| render(s, vars));
    let server = crate::commands::ssh::resolve_server(env.ctx, &server_name)?;
    crate::db::ssh_exec_capture(
        &server,
        &crate::commands::ps::kill_cmd(confirmed.pid, signal, confirmed.sudo, sudo_pass.as_deref()),
    )?;
    Ok(())
}

/// The only place `db_exec:` actually runs its statement — see `exec_fs_write`'s doc
/// comment for why this takes `&Confirmed<DbExecSpec>`. Returns `db::run_exec`'s output
/// string, for `register:`.
pub(crate) fn exec_db_exec(
    confirmed: &Confirmed<DbExecSpec>,
    sql: &str,
    vars: &HashMap<String, String>,
    env: &RunEnv,
) -> Result<String> {
    let server_name = render(&confirmed.server, vars);
    let server = crate::commands::ssh::resolve_server(env.ctx, &server_name)?;
    let creds = resolve_conn_creds(
        &server,
        confirmed.env.as_deref(),
        confirmed.engine.as_deref(),
        confirmed.host.as_deref(),
        confirmed.port,
        confirmed.database.as_deref(),
        confirmed.user.as_deref(),
        confirmed.password.as_deref(),
        vars,
    )?;
    crate::db::run_exec(&server, &creds, sql)
}

/// The only place `db_load:` actually runs — reads the local file (confined to the
/// playbook dir), converts a `format: json` array to CSV first, then streams it into the
/// remote table via `db::run_load`. Returns the client's summary line, for `register:`.
pub(crate) fn exec_db_load(
    confirmed: &Confirmed<DbLoadSpec>,
    vars: &HashMap<String, String>,
    env: &RunEnv,
) -> Result<String> {
    let file = join_confined(&env.playbook_dir, &render(&confirmed.file, vars))?;
    let raw = std::fs::read(&file)
        .with_context(|| format!("db_load: reading local file {}", file.display()))?;

    let (delimiter, data, has_header) = match confirmed.format {
        DbLoadFormat::Csv => (b',', raw, confirmed.headers),
        DbLoadFormat::Tsv => (b'\t', raw, confirmed.headers),
        DbLoadFormat::Json => {
            let text =
                String::from_utf8(raw).context("db_load: format: json file is not valid UTF-8")?;
            let value: serde_json::Value = serde_json::from_str(&text)
                .context("db_load: format: json file is not valid JSON")?;
            let serde_json::Value::Array(elements) = value else {
                bail!("db_load: format: json file must contain a JSON array");
            };
            // A JSON object array always writes a header row here, so load it back with one.
            (b',', json_array_to_csv(&elements, true, b',')?, true)
        }
    };

    let server = crate::commands::ssh::resolve_server(env.ctx, &render(&confirmed.server, vars))?;
    let creds = resolve_conn_creds(
        &server,
        confirmed.env.as_deref(),
        confirmed.engine.as_deref(),
        confirmed.host.as_deref(),
        confirmed.port,
        confirmed.database.as_deref(),
        confirmed.user.as_deref(),
        confirmed.password.as_deref(),
        vars,
    )?;
    let table = render(&confirmed.table, vars);
    let opts = crate::db::LoadOpts {
        table: &table,
        columns: confirmed.columns.as_deref(),
        truncate: matches!(confirmed.mode, DbLoadMode::Truncate),
        replace: matches!(confirmed.mode, DbLoadMode::Upsert),
        has_header,
        delimiter: delimiter as char,
    };
    crate::db::run_load(&server, &creds, &data, &opts)
}

/// The only place `secret_set:` actually writes to the OS keychain — see
/// `exec_fs_write`'s doc comment for why this takes `&Confirmed<SecretSetSpec>`. The
/// value is never printed or returned, matching every other secret-shaped value's
/// content-hiding convention in this DSL.
pub(crate) fn exec_secret_set(
    confirmed: &Confirmed<SecretSetSpec>,
    profile: &str,
    key: &str,
    vars: &HashMap<String, String>,
) -> Result<()> {
    let value = render(&confirmed.value, vars);
    crate::secrets::set_secret(profile, key, &value)
}

/// The only place `deploy:` actually runs — see `commands::deploy::apply_deploy_steps`,
/// the exact same function the standalone `tooler deploy run` CLI command calls, so
/// both go through identical step logic and identical error wording.
pub(crate) fn exec_deploy(
    confirmed: &Confirmed<DeploySpec>,
    vars: &HashMap<String, String>,
    env: &RunEnv,
) -> Result<()> {
    let server_name = render(&confirmed.server, vars);
    let server = crate::commands::ssh::resolve_server(env.ctx, &server_name)?;
    let path = render(&confirmed.path, vars);
    let build = confirmed.build.as_deref().map(|b| render(b, vars));
    let restart = confirmed.restart.as_deref().map(|r| render(r, vars));
    let health_url = confirmed.health_url.as_deref().map(|u| render(u, vars));
    let sudo_pass = confirmed.sudo_pass.as_deref().map(|p| render(p, vars));
    crate::commands::deploy::apply_deploy_steps(
        &server,
        &path,
        confirmed.pull,
        build.as_deref(),
        restart.as_deref(),
        health_url.as_deref(),
        confirmed.health_timeout,
        confirmed.health_retries,
        confirmed.health_delay,
        confirmed.sudo,
        sudo_pass.as_deref(),
    )
}

/// The only place `upload:` actually runs — `commands::ssh::run_scp`, the exact
/// primitive `tooler ssh copy` uses. Returns the local file's byte size for `register:`.
pub(crate) fn exec_upload(
    confirmed: &Confirmed<UploadSpec>,
    vars: &HashMap<String, String>,
    env: &RunEnv,
) -> Result<u64> {
    let local = env.playbook_dir.join(render(&confirmed.local, vars));
    let meta = std::fs::metadata(&local)
        .with_context(|| format!("upload: local file not found: {}", local.display()))?;
    if !meta.is_file() {
        bail!("upload: not a regular file: {}", local.display());
    }
    let server = crate::commands::ssh::resolve_server(env.ctx, &render(&confirmed.server, vars))?;
    crate::commands::ssh::run_scp(&server, &local, &render(&confirmed.remote, vars))?;
    Ok(meta.len())
}

/// The only place `cron:` actually runs — the extracted `commands::cron` helpers, the
/// same ones `tooler cron list/add/remove` call. Validates exactly-one-of
/// `add`/`remove`/`list` first. Returns the value `register:` should capture.
pub(crate) fn exec_cron(
    task: &Task,
    spec: &CronSpec,
    vars: &HashMap<String, String>,
    env: &RunEnv,
) -> Result<String> {
    let n = spec.add.is_some() as u8 + spec.remove.is_some() as u8 + spec.list as u8;
    if n != 1 {
        bail!("cron: needs exactly one of add/remove/list");
    }

    // The confirm gate for a mutation is checked before anything resolves a server or
    // opens a connection, same promise the Trust model makes for every other gated action.
    if spec.add.is_some() || spec.remove.is_some() {
        let _ = Confirmed::require(spec, "cron", &task.name)?;
    }
    let server = crate::commands::ssh::resolve_server(env.ctx, &render(&spec.server, vars))?;

    if let Some(line) = &spec.add {
        let line = render(line, vars);
        if !env.quiet {
            println!("  {} cron add {}", "→".bold(), line.dimmed());
        }
        crate::commands::cron::add_cron_line(&server, &line)?;
        if !env.quiet {
            println!("  {} added", "✓ ok".green().bold());
        }
        return Ok("true".to_string());
    }

    if let Some(pattern) = &spec.remove {
        let pattern = render(pattern, vars);
        if !env.quiet {
            println!("  {} cron remove /{}/", "→".bold(), pattern.dimmed());
        }
        let removed = crate::commands::cron::remove_cron_lines(&server, &pattern)?;
        if !env.quiet {
            if removed.is_empty() {
                println!("  {}", "(no lines matched)".dimmed());
            } else {
                println!(
                    "  {} removed {} line(s)",
                    "✓ ok".green().bold(),
                    removed.len()
                );
            }
        }
        return Ok(removed.len().to_string());
    }

    // list
    if !env.quiet {
        println!("  {} cron list", "→".bold());
    }
    let entries = crate::commands::cron::list_cron_entries(&server)?;
    if !env.quiet {
        let noun = if entries.len() == 1 {
            "entry"
        } else {
            "entries"
        };
        println!("  {} {} {noun}", "✓ ok".green().bold(), entries.len());
    }
    Ok(serde_json::to_string(&entries).unwrap_or_else(|_| "[]".to_string()))
}

pub(crate) fn run_task_once(
    task: &Task,
    vars: &mut HashMap<String, String>,
    include_stack: &mut Vec<PathBuf>,
    env: &RunEnv,
) -> Result<()> {
    // flush_handlers: only makes sense with access to execute_playbook's own
    // notified/handlers bookkeeping — reaching here means it was used inside
    // block:/rescue:/always:/run_task_sequence, which has no such state.
    if task.flush_handlers {
        bail!(
            "flush_handlers: is only supported as a direct playbook task, not inside \
             block:/rescue:/always: (task '{}')",
            task.name
        );
    }

    if task.register.is_some()
        && (task.check_url.is_some()
            || task.check_port.is_some()
            || task.env_check.is_some()
            || task.include.is_some()
            || task.assert.is_some()
            || task.block.is_some()
            || task.parallel.is_some()
            || task.debug.is_some()
            || task.set_fact.is_some()
            || task.wait_for.is_some()
            || task.confirm.is_some()
            || task.include_vars.is_some())
    {
        bail!(
            "register: is not supported for check_url/check_port/env_check/include/assert/block/parallel/debug/set_fact/wait_for/confirm/include_vars tasks"
        );
    }

    if task.timeout.is_some()
        && task.run.is_none()
        && task.ssh.is_none()
        && task.fleet.is_none()
        && task.sync_db.is_none()
        && task.sync_files.is_none()
    {
        bail!("timeout: is only supported on run:/ssh:/fleet:/sync_db:/sync_files: tasks");
    }

    if let Some(children) = &task.parallel {
        if task.loop_spec.is_some() {
            bail!(
                "parallel: can't be combined with loop: (task '{}')",
                task.name
            );
        }
        if task.block.is_some() || task.rescue.is_some() || task.always.is_some() {
            bail!(
                "parallel: can't be combined with block:/rescue:/always: (task '{}')",
                task.name
            );
        }
        if let Some(bad) = children.iter().find(|c| c.confirm.is_some()) {
            bail!(
                "parallel: child '{}' uses confirm: — an interactive pause from a worker \
                 thread isn't supported; move it out of parallel: or run with --yes",
                bad.name
            );
        }
        return run_parallel_block(
            children,
            task.max_parallel,
            vars,
            include_stack.as_slice(),
            env,
        );
    }

    if let Some(spec) = &task.wait_for {
        let set_count = [
            spec.check_url.is_some(),
            spec.check_port.is_some(),
            spec.ssh.is_some(),
            spec.file_exists.is_some(),
            spec.file_absent.is_some(),
        ]
        .into_iter()
        .filter(|b| *b)
        .count();
        if set_count != 1 {
            bail!(
                "wait_for: needs exactly one of check_url/check_port/ssh/file_exists/file_absent, \
                 task '{}' has {set_count}",
                task.name
            );
        }
    }

    if let Some(msg) = &task.debug {
        println!("  {} {}", "ℹ".cyan().bold(), render(msg, vars));
        return Ok(());
    }

    if let Some(msg) = &task.confirm {
        let rendered = render(msg, vars);
        if env.dry {
            if !env.quiet {
                println!(
                    "  {} (dry run — would prompt: {rendered})",
                    "?".cyan().bold()
                );
            }
            return Ok(());
        }
        if env.auto_yes {
            if !env.quiet {
                println!("  {} {rendered} — confirmed via --yes", "?".cyan().bold());
            }
            return Ok(());
        }
        if env.quiet {
            bail!(
                "confirm: '{rendered}' requires --yes when running non-interactively \
                 (--output json, or driven by an agent over MCP) — it never blocks on stdin"
            );
        }
        print!("  {} {rendered} [y/N] ", "?".cyan().bold());
        std::io::Write::flush(&mut std::io::stdout()).ok();
        let mut input = String::new();
        std::io::stdin()
            .read_line(&mut input)
            .context("failed to read confirm: answer from stdin")?;
        let answer = input.trim().to_lowercase();
        if answer == "y" || answer == "yes" {
            return Ok(());
        }
        bail!("aborted at confirm: '{rendered}'");
    }

    if let Some(facts) = &task.set_fact {
        for (k, v) in facts {
            let rendered = render(v, vars);
            if !env.quiet {
                println!("  {} {} = {}", "ƒ".cyan().bold(), k, rendered.dimmed());
            }
            vars.insert(k.clone(), rendered);
        }
        return Ok(());
    }

    if let Some(path) = &task.include_vars {
        let rendered = render(path, vars);
        let resolved = env.playbook_dir.join(&rendered);
        if !env.quiet {
            println!(
                "  {} {}",
                "→ vars".bold(),
                resolved.display().to_string().dimmed()
            );
        }
        if !env.dry {
            let loaded = load_vars_file(&resolved)?;
            let count = loaded.len();
            vars.extend(loaded);
            if !env.quiet {
                println!("  {} {count} var(s) loaded", "✓ ok".green().bold());
            }
        }
        return Ok(());
    }

    if let Some(facts) = &task.state_set {
        for (k, v) in facts {
            let rendered = render(v, vars);
            if !env.quiet {
                println!(
                    "  {} state.{} = {}",
                    "ƒ".cyan().bold(),
                    k,
                    rendered.dimmed()
                );
            }
            vars.insert(format!("state.{k}"), rendered);
        }
        // The actual persistence, like every other action's real work, is skipped in
        // --dry — the in-memory vars.insert() above is enough to preview what the
        // resulting {{state.*}} values would be within this run.
        if !env.dry {
            write_persisted_state(env, vars);
        }
        return Ok(());
    }

    if let Some(spec) = &task.assert {
        let (conditions, msg): (&[String], Option<&str>) = match spec {
            AssertSpec::Simple(expr) => (std::slice::from_ref(expr), None),
            AssertSpec::Structured { that, msg } => (that.as_slice(), msg.as_deref()),
        };
        for expr in conditions {
            if !eval_when(expr, vars) {
                match msg {
                    Some(m) => bail!("assertion failed: {m} (condition: {expr})"),
                    None => bail!("assertion failed: {expr}"),
                }
            }
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

    if let Some(spec) = &task.run {
        let (cmd_str, env_vars): (&str, Option<&HashMap<String, String>>) = match spec {
            RunSpec::Simple(s) => (s.as_str(), None),
            RunSpec::Structured { command, env } => (command.as_str(), Some(env)),
        };
        let rendered_cmd = render(cmd_str, vars);
        if !env.quiet {
            println!(
                "  {} {}",
                "$".bold().green(),
                render_for_display(cmd_str, vars).dimmed()
            );
        }
        if !env.dry {
            let start = Instant::now();
            let mut cmd = std::process::Command::new("sh");
            cmd.arg("-c")
                .arg(&rendered_cmd)
                .current_dir(&env.playbook_dir);
            if let Some(env_vars) = env_vars {
                for (k, v) in env_vars {
                    cmd.env(k, render(v, vars));
                }
            }
            let (success, code, captured) =
                run_with_timeout(cmd, task.register.is_some(), task.timeout)?;
            let elapsed = start.elapsed();
            if let Some(reg) = &task.register {
                vars.insert(
                    format!("{reg}.exit_code"),
                    code.map(|c| c.to_string()).unwrap_or_default(),
                );
                if let Some(val) = &captured {
                    vars.insert(reg.clone(), val.clone());
                }
            }
            if success {
                if !env.quiet {
                    println!(
                        "  {} {}",
                        "✓ ok".green().bold(),
                        format!("({:.1}s)", elapsed.as_secs_f32()).dimmed()
                    );
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

    if let Some(spec) = &task.http {
        let url = render(&spec.url, vars);
        // Kept as the rendered *relative* string (not the resolved absolute path) so a
        // `register:`ed value composes directly with any other path-taking task
        // (`mail: {attachments: [...]}`, `read_csv:`, ...) — all of which resolve their
        // own paths relative to this same playbook directory via `join_confined`, which
        // rejects absolute paths outright.
        let download_rel = spec.download.as_ref().map(|d| render(d, vars));
        let download_path = download_rel
            .as_ref()
            .map(|d| join_confined(&env.playbook_dir, d))
            .transpose()?;
        if download_path.is_some() && spec.paginate.is_some() {
            bail!(
                "http: can't combine download: and paginate: (task '{}')",
                task.name
            );
        }
        if !env.quiet {
            let suffix = match (&download_path, &spec.paginate) {
                (Some(p), _) => format!(" → {}", p.display().to_string().dimmed()),
                (None, Some(pg)) => format!(
                    " {}",
                    format!("(paginated, ≤{} pages)", pg.max_pages).dimmed()
                ),
                (None, None) => String::new(),
            };
            println!(
                "  {} {} {}{suffix}",
                spec.method.to_uppercase().bold(),
                "→".bold(),
                url.dimmed(),
            );
        }
        if !env.dry {
            if let Some(out_path) = &download_path {
                let (_bytes_written, status) = http_download(spec, &url, vars, out_path)?;
                if let Some(reg) = &task.register {
                    vars.insert(format!("{reg}.status"), status.to_string());
                    vars.insert(reg.clone(), download_rel.clone().unwrap_or_default());
                }
            } else if let Some(pg) = &spec.paginate {
                let (items_json, last_status, pages) = http_paginate(spec, pg, &url, vars)?;
                if !env.quiet {
                    println!("  {} {pages} page(s)", "✓".green());
                }
                if let Some(reg) = &task.register {
                    vars.insert(format!("{reg}.status"), last_status.to_string());
                    vars.insert(format!("{reg}.pages"), pages.to_string());
                    vars.insert(reg.clone(), items_json);
                }
            } else {
                let (body, status) = http_request(spec, &url, vars)?;
                if let Some(reg) = &task.register {
                    vars.insert(format!("{reg}.status"), status.to_string());
                    vars.insert(reg.clone(), body);
                }
            }
        }
        return Ok(());
    }

    if let Some(spec) = &task.scrape {
        let url = render(&spec.url, vars);
        if !env.quiet {
            println!("  {} {}", "→".bold(), url.dimmed());
        }
        if !env.dry {
            let items = scrape(spec, &url, vars)?;
            if !env.quiet {
                println!(
                    "  {} scraped {} item(s)",
                    "✓ ok".green().bold(),
                    items.len()
                );
            }
            if let Some(reg) = &task.register {
                vars.insert(
                    reg.clone(),
                    serde_json::to_string(&items).unwrap_or_default(),
                );
            }
        }
        return Ok(());
    }

    if let Some(spec) = &task.wait_for {
        // Exactly one of these is Some — enforced upfront above.
        let file_exists_path = spec
            .file_exists
            .as_deref()
            .map(|p| join_confined(&env.playbook_dir, &render(p, vars)))
            .transpose()?;
        let file_absent_path = spec
            .file_absent
            .as_deref()
            .map(|p| join_confined(&env.playbook_dir, &render(p, vars)))
            .transpose()?;
        let describe = if let Some(url) = &spec.check_url {
            format!("{} to respond", render(url, vars))
        } else if let Some(port_spec) = &spec.check_port {
            format!(
                "{}:{} to accept connections",
                render(&port_spec.host, vars),
                port_spec.port
            )
        } else if let Some(path) = &file_exists_path {
            format!("{} to exist", path.display())
        } else if let Some(path) = &file_absent_path {
            format!("{} to no longer exist", path.display())
        } else {
            let ssh_spec = spec.ssh.as_ref().expect("validated: exactly one check set");
            format!(
                "'{}' to succeed on {}",
                render(&ssh_spec.command, vars),
                render(&ssh_spec.server, vars)
            )
        };
        if !env.quiet {
            println!(
                "  {} waiting for {describe} (up to {}s)...",
                "→".bold(),
                spec.timeout
            );
        }
        if !env.dry {
            let deadline = Instant::now() + Duration::from_secs(spec.timeout);
            loop {
                let attempt_ok = if let Some(url) = &spec.check_url {
                    check_url(&render(url, vars), true).is_ok()
                } else if let Some(port_spec) = &spec.check_port {
                    check_port(
                        &render(&port_spec.host, vars),
                        port_spec.port,
                        port_spec.timeout,
                        true,
                    )
                    .is_ok()
                } else if let Some(path) = &file_exists_path {
                    path.exists()
                } else if let Some(path) = &file_absent_path {
                    !path.exists()
                } else {
                    let ssh_spec = spec.ssh.as_ref().expect("validated: exactly one check set");
                    let server_name = render(&ssh_spec.server, vars);
                    let full_cmd = crate::commands::fleet::exec_command(
                        &render(&ssh_spec.command, vars),
                        ssh_spec.sudo,
                    );
                    crate::commands::ssh::resolve_server(env.ctx, &server_name)
                        .ok()
                        .and_then(|server| {
                            crate::db::ssh_exec_capture_lenient(&server, &full_cmd).ok()
                        })
                        .map(|(_, _, success, _)| success)
                        .unwrap_or(false)
                };
                if attempt_ok {
                    if !env.quiet {
                        println!("  {} {describe}", "✓ ok".green().bold());
                    }
                    break;
                }
                if Instant::now() >= deadline {
                    bail!("timed out after {}s waiting for {describe}", spec.timeout);
                }
                std::thread::sleep(Duration::from_secs(spec.interval));
            }
        }
        return Ok(());
    }

    if let Some(spec) = &task.report {
        let format = spec.format.to_lowercase();
        if !matches!(format.as_str(), "html" | "pdf" | "excel") {
            bail!(
                "report: format must be one of html/pdf/excel, task '{}' has '{}'",
                task.name,
                spec.format
            );
        }
        let title = render(&spec.title, vars);
        let out_path = env.playbook_dir.join(render(&spec.out, vars));
        if !env.quiet {
            println!(
                "  {} report format={format} out={}",
                "→".bold(),
                out_path.display()
            );
        }
        if !env.dry {
            let sources: Vec<report::Source> = spec
                .sources
                .iter()
                .map(|(name, raw)| {
                    let rendered = render(raw, vars);
                    let value = serde_json::from_str(&rendered)
                        .unwrap_or(serde_json::Value::String(rendered));
                    report::Source {
                        name: name.clone(),
                        value,
                    }
                })
                .collect();
            let bytes = match format.as_str() {
                "html" => report::html::build(&title, &sources),
                "pdf" => report::pdf::build(&title, &sources),
                "excel" => report::excel::build(&title, &sources),
                _ => unreachable!("validated above"),
            }
            .with_context(|| format!("building {format} report"))?;
            std::fs::write(&out_path, &bytes)
                .with_context(|| format!("writing report: {}", out_path.display()))?;
            if !env.quiet {
                println!(
                    "  {} {} bytes -> {}",
                    "✓ ok".green().bold(),
                    bytes.len(),
                    out_path.display()
                );
            }
            if let Some(reg) = &task.register {
                vars.insert(reg.clone(), bytes.len().to_string());
            }
        }
        return Ok(());
    }

    if let Some(spec) = &task.write_file {
        let out_path = join_confined(&env.playbook_dir, &render(&spec.path, vars))?;
        if !env.quiet {
            println!(
                "  {} {}",
                if spec.append {
                    "→ append".bold()
                } else {
                    "→ write".bold()
                },
                out_path.display().to_string().dimmed()
            );
        }
        if !env.dry {
            let content = render(&spec.content, vars);
            if env.diff {
                let old = std::fs::read(&out_path)
                    .map(|b| String::from_utf8_lossy(&b).into_owned())
                    .unwrap_or_default();
                // Appending only changes the tail — diff the resulting full content, not
                // just the fragment, so it reads as "old" -> "old + new tail".
                let new = if spec.append {
                    format!("{old}{content}")
                } else {
                    content.clone()
                };
                print_diff_if_enabled(env, &old, &new);
            }
            if let Some(parent) = out_path.parent() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!("creating parent directory for {}", out_path.display())
                })?;
            }
            let bytes_written = content.len();
            if spec.append {
                use std::io::Write;
                let mut f = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&out_path)
                    .with_context(|| format!("opening {} for append", out_path.display()))?;
                f.write_all(content.as_bytes())
                    .with_context(|| format!("appending to {}", out_path.display()))?;
            } else {
                std::fs::write(&out_path, &content)
                    .with_context(|| format!("writing {}", out_path.display()))?;
            }
            if !env.quiet {
                println!(
                    "  {} {} bytes -> {}",
                    "✓ ok".green().bold(),
                    bytes_written,
                    out_path.display()
                );
            }
            if let Some(reg) = &task.register {
                vars.insert(reg.clone(), bytes_written.to_string());
            }
        }
        return Ok(());
    }

    if let Some(spec) = &task.template {
        let src = join_confined(&env.playbook_dir, &render(&spec.src, vars))?;
        let dest = render(&spec.dest, vars);
        let server_name = spec.server.as_deref().map(|s| render(s, vars));
        if !env.quiet {
            match &server_name {
                Some(s) => println!(
                    "  {} {} → {}:{}",
                    "→ template".bold(),
                    src.display().to_string().dimmed(),
                    s.dimmed(),
                    dest.dimmed()
                ),
                None => println!(
                    "  {} {} → {}",
                    "→ template".bold(),
                    src.display().to_string().dimmed(),
                    dest.dimmed()
                ),
            }
        }
        if !env.dry {
            let rendered = render_template(&src, vars)?;
            let bytes = rendered.len();
            match &server_name {
                Some(sn) => {
                    let confirmed = Confirmed::require(spec, "template", &task.name)?;
                    let _ = &confirmed;
                    let server = crate::commands::ssh::resolve_server(env.ctx, sn)?;
                    if env.diff {
                        let old = crate::db::ssh_exec_capture(
                            &server,
                            &crate::commands::fs::cat_cmd(&dest),
                        )
                        .unwrap_or_default();
                        print_diff_if_enabled(env, &old, &rendered);
                    }
                    let (_, stderr, ok) = crate::db::ssh_exec_with_stdin(
                        &server,
                        &crate::commands::fs::write_cmd(&dest),
                        rendered.as_bytes(),
                    )?;
                    if !ok {
                        let e = stderr.trim();
                        bail!(
                            "{}",
                            if e.is_empty() {
                                "template write failed"
                            } else {
                                e
                            }
                        );
                    }
                }
                None => {
                    let out_path = join_confined(&env.playbook_dir, &dest)?;
                    if env.diff {
                        let old = std::fs::read(&out_path)
                            .map(|b| String::from_utf8_lossy(&b).into_owned())
                            .unwrap_or_default();
                        print_diff_if_enabled(env, &old, &rendered);
                    }
                    if let Some(parent) = out_path.parent() {
                        std::fs::create_dir_all(parent).ok();
                    }
                    std::fs::write(&out_path, &rendered)
                        .with_context(|| format!("writing {}", out_path.display()))?;
                }
            }
            if !env.quiet {
                println!("  {} {bytes} bytes", "✓ ok".green().bold());
            }
            if let Some(reg) = &task.register {
                vars.insert(reg.clone(), bytes.to_string());
            }
        }
        return Ok(());
    }

    if let Some(spec) = &task.read_csv {
        let in_path = join_confined(&env.playbook_dir, &render(&spec.path, vars))?;
        if !env.quiet {
            println!(
                "  {} {}",
                "→ read".bold(),
                in_path.display().to_string().dimmed()
            );
        }
        if !env.dry {
            let delimiter = match &spec.delimiter {
                Some(d) if d.len() == 1 => d.as_bytes()[0],
                Some(d) => bail!("read_csv: delimiter must be a single character, got '{d}'"),
                None => b',',
            };
            let mut reader = csv::ReaderBuilder::new()
                .has_headers(spec.headers)
                .delimiter(delimiter)
                .from_path(&in_path)
                .with_context(|| format!("opening {}", in_path.display()))?;

            let rows: Vec<serde_json::Value> = if spec.headers {
                let headers = reader
                    .headers()
                    .with_context(|| format!("reading header row of {}", in_path.display()))?
                    .clone();
                reader
                    .records()
                    .map(|r| {
                        let record =
                            r.with_context(|| format!("reading a row of {}", in_path.display()))?;
                        let mut obj = serde_json::Map::new();
                        for (col, cell) in headers.iter().zip(record.iter()) {
                            obj.insert(
                                col.to_string(),
                                serde_json::Value::String(cell.to_string()),
                            );
                        }
                        Ok(serde_json::Value::Object(obj))
                    })
                    .collect::<Result<Vec<_>>>()?
            } else {
                reader
                    .records()
                    .map(|r| {
                        let record =
                            r.with_context(|| format!("reading a row of {}", in_path.display()))?;
                        Ok(serde_json::Value::Array(
                            record
                                .iter()
                                .map(|c| serde_json::Value::String(c.to_string()))
                                .collect(),
                        ))
                    })
                    .collect::<Result<Vec<_>>>()?
            };

            if !env.quiet {
                println!("  {} {} row(s)", "✓ ok".green().bold(), rows.len());
            }
            if let Some(reg) = &task.register {
                vars.insert(
                    reg.clone(),
                    serde_json::to_string(&rows).unwrap_or_default(),
                );
            }
        }
        return Ok(());
    }

    if let Some(spec) = &task.write_csv {
        let out_path = join_confined(&env.playbook_dir, &render(&spec.path, vars))?;
        if !env.quiet {
            println!(
                "  {} {}",
                "→ write".bold(),
                out_path.display().to_string().dimmed()
            );
        }
        if !env.dry {
            let delimiter = match &spec.delimiter {
                Some(d) if d.len() == 1 => d.as_bytes()[0],
                Some(d) => bail!("write_csv: delimiter must be a single character, got '{d}'"),
                None => b',',
            };
            let rendered = render(&spec.data, vars);
            let value: serde_json::Value = serde_json::from_str(&rendered)
                .with_context(|| format!("write_csv: '{}' data is not valid JSON", task.name))?;
            let serde_json::Value::Array(elements) = value else {
                bail!(
                    "write_csv: task '{}' data must be a JSON array, got {}",
                    task.name,
                    rendered.chars().take(60).collect::<String>()
                );
            };

            let row_count = elements.len();
            let bytes = json_array_to_csv(&elements, spec.headers, delimiter)?;
            if let Some(parent) = out_path.parent() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!("creating parent directory for {}", out_path.display())
                })?;
            }
            std::fs::write(&out_path, &bytes)
                .with_context(|| format!("writing {}", out_path.display()))?;

            if !env.quiet {
                println!("  {} {} row(s)", "✓ ok".green().bold(), row_count);
            }
            if let Some(reg) = &task.register {
                vars.insert(reg.clone(), row_count.to_string());
            }
        }
        return Ok(());
    }

    if task.git_summary.is_some() {
        if !env.quiet {
            println!("  {} git summary", "→".bold());
        }
        if !env.dry {
            let s = crate::commands::git::compute_summary(Some(&env.playbook_dir))?;
            if !env.quiet {
                let tag = s.tag.as_deref().unwrap_or("—");
                let ab = if s.has_upstream {
                    format!(" (↑{} ↓{})", s.ahead, s.behind)
                } else {
                    String::new()
                };
                println!(
                    "  branch: {}{ab}  tag: {}  status: {}",
                    s.branch,
                    tag,
                    if s.clean { "clean" } else { "dirty" }
                );
                for line in &s.recent {
                    println!("    {}", line.dimmed());
                }
            }
            if let Some(reg) = &task.register {
                vars.insert(reg.clone(), serde_json::to_string(&s).unwrap_or_default());
            }
        }
        return Ok(());
    }

    if let Some(spec) = &task.git_changelog {
        let from = spec.from.as_ref().map(|f| render(f, vars));
        if !env.quiet {
            println!("  {} git changelog", "→".bold());
        }
        if !env.dry {
            let c =
                crate::commands::git::compute_changelog(Some(&env.playbook_dir), from.as_deref())?;
            if !env.quiet {
                if c.features.is_empty() && c.fixes.is_empty() && c.other.is_empty() {
                    println!("  {}", "(no commits since last tag)".dimmed());
                } else {
                    for m in &c.features {
                        println!("  feat: {m}");
                    }
                    for m in &c.fixes {
                        println!("  fix: {m}");
                    }
                    for m in &c.other {
                        println!("  {m}");
                    }
                }
            }
            if let Some(reg) = &task.register {
                vars.insert(reg.clone(), serde_json::to_string(&c).unwrap_or_default());
            }
        }
        return Ok(());
    }

    if let Some(spec) = &task.gh_prs {
        let repo = spec.repo.as_ref().map(|r| render(r, vars));
        let after_s = spec.after.as_ref().map(|a| render(a, vars));
        let before_s = spec.before.as_ref().map(|b| render(b, vars));
        let state = render(&spec.state, vars);
        if !env.quiet {
            println!(
                "  {} gh pr list{}",
                "→".bold(),
                repo.as_deref()
                    .map(|r| format!(" in {r}"))
                    .unwrap_or_default()
                    .dimmed()
            );
        }
        if !env.dry {
            let after = after_s
                .as_deref()
                .map(crate::commands::gh::parse_date)
                .transpose()?;
            let before = before_s
                .as_deref()
                .map(crate::commands::gh::parse_date)
                .transpose()?;
            let prs = crate::commands::gh::fetch_prs(
                Some(&env.playbook_dir),
                repo.as_deref(),
                after,
                before,
                &state,
                spec.limit,
            )?;
            if !env.quiet {
                println!("  {} {} pr(s)", "✓ ok".green().bold(), prs.len());
            }
            if let Some(reg) = &task.register {
                vars.insert(reg.clone(), serde_json::to_string(&prs).unwrap_or_default());
            }
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
            let display_cmd = crate::commands::fleet::exec_command(
                &render_for_display(&spec.command, vars),
                spec.sudo,
            );
            println!(
                "  {} {} {} {}",
                "→".bold(),
                display_cmd.dimmed(),
                "on".dimmed(),
                server_name.dimmed()
            );
        }
        if !env.dry {
            let server = crate::commands::ssh::resolve_server(env.ctx, &server_name)?;
            let (stdout, stderr, success, code) =
                crate::db::ssh_exec_capture_lenient_with_timeout(&server, &full_cmd, task.timeout)?;
            // Set before the success check (not only inside the `if success` branch),
            // same as run:'s <reg>.exit_code -- so it survives a subsequent bail! when
            // vars is a &mut the caller already holds, letting ignore_errors: true plus
            // a later {{reg.exit_code}} read see what actually happened.
            if let Some(reg) = &task.register {
                vars.insert(
                    format!("{reg}.exit_code"),
                    code.map(|c| c.to_string()).unwrap_or_default(),
                );
            }
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
        let servers = spec.servers.as_deref().map(|s| render(s, vars));
        let group = spec.group.as_deref().map(|s| render(s, vars));
        if !env.quiet {
            println!(
                "  {} {}",
                "→".bold(),
                render_for_display(&spec.command, vars).dimmed()
            );
        }
        if !env.dry {
            let results = crate::commands::fleet::run_on_targets(
                env.ctx,
                servers.as_deref(),
                spec.all,
                group.as_deref(),
                &command,
                spec.sudo,
                spec.parallel,
                spec.batch_size,
                task.timeout,
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
                // Per-server detail (server/success/stdout/stderr/exit_code) alongside
                // the plain ok_count/total summary above -- same <name>.results
                // convention loop:/db_query:/etc. already use, so an agent can find
                // exactly which server(s) failed and why via
                // loop: {from: "{{reg.results}}"} or a | json: pull, not just "3/5".
                let json = serde_json::to_string(&results)
                    .expect("serializing Vec<ExecResult> to JSON cannot fail");
                vars.insert(format!("{reg}.results"), json);
            }
            if !all_succeeded {
                bail!("{ok_count}/{total} servers succeeded");
            }
        }
        return Ok(());
    }

    if let Some(spec) = &task.fs_cat {
        let server_name = render(&spec.server, vars);
        let path = render(&spec.path, vars);
        if !env.quiet {
            println!(
                "  {} {} on {}",
                "→ cat".bold(),
                path.dimmed(),
                server_name.dimmed()
            );
        }
        if !env.dry {
            let server = crate::commands::ssh::resolve_server(env.ctx, &server_name)?;
            let content =
                crate::db::ssh_exec_capture(&server, &crate::commands::fs::cat_cmd(&path))?;
            if !env.quiet {
                println!("  {} {} byte(s)", "✓ ok".green().bold(), content.len());
            }
            if let Some(reg) = &task.register {
                vars.insert(reg.clone(), content);
            }
        }
        return Ok(());
    }

    if let Some(spec) = &task.fs_write {
        let server_name = render(&spec.server, vars);
        let path = render(&spec.path, vars);
        if !env.quiet {
            println!(
                "  {} {} on {}",
                "→ write".bold(),
                path.dimmed(),
                server_name.dimmed()
            );
        }
        if !env.dry {
            let confirmed = Confirmed::require(spec, "fs_write", &task.name)?;
            let content = exec_fs_write(&confirmed, &path, vars, env)?;
            if !env.quiet {
                println!("  {} {} byte(s)", "✓ ok".green().bold(), content.len());
            }
            if let Some(reg) = &task.register {
                vars.insert(reg.clone(), content.len().to_string());
            }
        }
        return Ok(());
    }

    if let Some(spec) = &task.systemd_restart {
        let server_name = render(&spec.server, vars);
        let unit = render(&spec.unit, vars);
        if !env.quiet {
            println!(
                "  {} restart {} on {}",
                "→".bold(),
                unit.dimmed(),
                server_name.dimmed()
            );
        }
        if !env.dry {
            let confirmed = Confirmed::require(spec, "systemd_restart", &task.name)?;
            exec_systemd_restart(&confirmed, &unit, vars, env)?;
            if !env.quiet {
                println!("  {} {} restarted", "✓ ok".green().bold(), unit);
            }
            if let Some(reg) = &task.register {
                vars.insert(reg.clone(), "true".to_string());
            }
        }
        return Ok(());
    }

    if let Some(spec) = &task.systemd_status {
        let server_name = render(&spec.server, vars);
        let unit = render(&spec.unit, vars);
        if !env.quiet {
            println!(
                "  {} status {} on {}",
                "→".bold(),
                unit.dimmed(),
                server_name.dimmed()
            );
        }
        if !env.dry {
            let server = crate::commands::ssh::resolve_server(env.ctx, &server_name)?;
            let (stdout, stderr, active, _) = crate::db::ssh_exec_capture_lenient(
                &server,
                &crate::commands::systemd::status_cmd(&unit),
            )?;
            let output = crate::commands::systemd::merge_output(stdout, stderr);
            if !env.quiet {
                let marker = if active {
                    "●".green().bold()
                } else {
                    "●".red().bold()
                };
                println!("{marker} {unit}");
                print!("{output}");
            }
            if let Some(reg) = &task.register {
                vars.insert(format!("{reg}.active"), active.to_string());
                vars.insert(reg.clone(), output);
            }
        }
        return Ok(());
    }

    if let Some(spec) = &task.logs_tail {
        let server_name = render(&spec.server, vars);
        let path = render(&spec.path, vars);
        if !env.quiet {
            println!(
                "  {} {} on {}",
                "→ tail".bold(),
                path.dimmed(),
                server_name.dimmed()
            );
        }
        if !env.dry {
            let server = crate::commands::ssh::resolve_server(env.ctx, &server_name)?;
            let output = crate::db::ssh_exec_capture(
                &server,
                &crate::commands::logs::tail_cmd(&path, spec.lines),
            )?;
            let lines: Vec<&str> = output.lines().collect();
            if !env.quiet {
                println!("  {} {} line(s)", "✓ ok".green().bold(), lines.len());
            }
            if let Some(reg) = &task.register {
                vars.insert(
                    reg.clone(),
                    serde_json::to_string(&lines).unwrap_or_default(),
                );
            }
        }
        return Ok(());
    }

    if let Some(spec) = &task.logs_grep {
        let server_name = render(&spec.server, vars);
        let path = render(&spec.path, vars);
        let pattern = render(&spec.pattern, vars);
        if !env.quiet {
            println!(
                "  {} '{}' in {} on {}",
                "→ grep".bold(),
                pattern.dimmed(),
                path.dimmed(),
                server_name.dimmed()
            );
        }
        if !env.dry {
            let server = crate::commands::ssh::resolve_server(env.ctx, &server_name)?;
            let (stdout, stderr, success, _) = crate::db::ssh_exec_capture_lenient(
                &server,
                &crate::commands::logs::grep_cmd(&path, &pattern),
            )?;
            let output = if success {
                stdout
            } else if stderr.trim().is_empty() {
                String::new()
            } else {
                bail!("{}", stderr.trim());
            };
            let (lines, truncated) = crate::commands::logs::cap_lines(output, spec.max_lines);
            if !env.quiet {
                let suffix = if truncated { ", truncated" } else { "" };
                println!(
                    "  {} {} match(es){suffix}",
                    "✓ ok".green().bold(),
                    lines.len()
                );
            }
            if let Some(reg) = &task.register {
                vars.insert(
                    reg.clone(),
                    serde_json::to_string(&lines).unwrap_or_default(),
                );
            }
        }
        return Ok(());
    }

    if let Some(spec) = &task.ps_list {
        let server_name = render(&spec.server, vars);
        let filter = spec.filter.as_deref().map(|f| render(f, vars));
        if !env.quiet {
            println!("  {} processes on {}", "→".bold(), server_name.dimmed());
        }
        if !env.dry {
            let server = crate::commands::ssh::resolve_server(env.ctx, &server_name)?;
            let output = crate::db::ssh_exec_capture(&server, "ps aux")?;
            let rows = crate::commands::ps::apply_filter(
                crate::commands::ps::parse_ps_aux(&output),
                filter.as_deref(),
            );
            if !env.quiet {
                println!("  {} {} process(es)", "✓ ok".green().bold(), rows.len());
            }
            if let Some(reg) = &task.register {
                vars.insert(
                    reg.clone(),
                    serde_json::to_string(&rows).unwrap_or_default(),
                );
            }
        }
        return Ok(());
    }

    if let Some(spec) = &task.ps_kill {
        let server_name = render(&spec.server, vars);
        let signal = render(&spec.signal, vars);
        if !env.quiet {
            println!(
                "  {} SIG{} to pid {} on {}",
                "→".bold(),
                signal.dimmed(),
                spec.pid,
                server_name.dimmed()
            );
        }
        if !env.dry {
            let confirmed = Confirmed::require(spec, "ps_kill", &task.name)?;
            exec_ps_kill(&confirmed, &signal, vars, env)?;
            if !env.quiet {
                println!("  {} sent", "✓ ok".green().bold());
            }
            if let Some(reg) = &task.register {
                vars.insert(reg.clone(), "true".to_string());
            }
        }
        return Ok(());
    }

    if let Some(spec) = &task.stat {
        let server_name = render(&spec.server, vars);
        if !env.quiet {
            println!("  {} stat on {}", "→".bold(), server_name.dimmed());
        }
        if !env.dry {
            let server = crate::commands::ssh::resolve_server(env.ctx, &server_name)?;
            let output = crate::db::ssh_exec_capture(&server, &crate::commands::stat::stat_cmd())?;
            let (uptime, memory, disk) = crate::commands::stat::parse_sections(&output);
            if !env.quiet {
                println!("{uptime}\n{memory}\n{disk}");
            }
            if let Some(reg) = &task.register {
                vars.insert(
                    reg.clone(),
                    serde_json::json!({"uptime": uptime, "memory": memory, "disk": disk})
                        .to_string(),
                );
            }
        }
        return Ok(());
    }

    if let Some(spec) = &task.sync_db {
        let server_name = render(&spec.server, vars);
        let describe_side = |side: &DbSyncSide| -> String {
            side.database
                .as_deref()
                .or(side.env.as_deref())
                .map(|s| render(s, vars))
                .unwrap_or_default()
        };
        let from_creds_preview = describe_side(&spec.from);
        let to_creds_preview = describe_side(&spec.to);
        if !env.quiet {
            println!(
                "  {} db sync {} → {} on {}",
                "→".bold(),
                from_creds_preview.dimmed(),
                to_creds_preview.dimmed(),
                server_name.dimmed()
            );
        }
        if !env.dry {
            let server = crate::commands::ssh::resolve_server(env.ctx, &server_name)?;
            let from_creds = resolve_db_sync_creds(&server, &spec.from, vars)?;
            let to_creds = resolve_db_sync_creds(&server, &spec.to, vars)?;
            let dump_cmd = crate::db::dump_command(&from_creds, true);
            let bytes =
                crate::db::ssh_exec_capture_bytes_with_timeout(&server, &dump_cmd, task.timeout)?;
            let restore_cmd = crate::db::restore_command(&to_creds, true);
            let (_, stderr, success) = crate::db::ssh_exec_with_stdin_with_timeout(
                &server,
                &restore_cmd,
                &bytes,
                task.timeout,
            )?;
            if !success {
                bail!("db sync failed: {}", stderr.trim());
            }
            if !env.quiet {
                println!(
                    "  {} synced {} bytes into {}",
                    "✓ ok".green().bold(),
                    bytes.len(),
                    to_creds.database.cyan()
                );
            }
            if let Some(reg) = &task.register {
                vars.insert(reg.clone(), bytes.len().to_string());
            }
        }
        return Ok(());
    }

    if let Some(spec) = &task.sync_files {
        let server_name = render(&spec.server, vars);
        let from = ensure_trailing_slash(&render(&spec.from, vars));
        let to = render(&spec.to, vars);
        if !env.quiet {
            println!(
                "  {} rsync {} → {} on {}",
                "→".bold(),
                ensure_trailing_slash(&render_for_display(&spec.from, vars)).dimmed(),
                render_for_display(&spec.to, vars).dimmed(),
                server_name.dimmed()
            );
        }
        if !env.dry {
            let server = crate::commands::ssh::resolve_server(env.ctx, &server_name)?;
            let cmd = format!(
                "rsync -a{} {} {}",
                if spec.delete { " --delete" } else { "" },
                crate::db::shell_quote(&from),
                crate::db::shell_quote(&to),
            );
            let (stdout, stderr, success, _) =
                crate::db::ssh_exec_capture_lenient_with_timeout(&server, &cmd, task.timeout)?;
            if success {
                let out = stdout.trim();
                if !env.quiet {
                    if !out.is_empty() {
                        println!("{out}");
                    }
                    println!("  {} synced", "✓ ok".green().bold());
                }
                if let Some(reg) = &task.register {
                    vars.insert(reg.clone(), out.to_string());
                }
            } else {
                let err = stderr.trim();
                bail!("{}", if err.is_empty() { "rsync failed" } else { err });
            }
        }
        return Ok(());
    }

    if let Some(spec) = &task.db_query {
        let server_name = render(&spec.server, vars);
        let sql = render(&spec.sql, vars);
        if !env.quiet {
            println!(
                "  {} {} on {}",
                "→".bold(),
                render_for_display(&spec.sql, vars).dimmed(),
                server_name.dimmed()
            );
        }
        if !env.dry {
            let server = crate::commands::ssh::resolve_server(env.ctx, &server_name)?;
            let creds = resolve_conn_creds(
                &server,
                spec.env.as_deref(),
                spec.engine.as_deref(),
                spec.host.as_deref(),
                spec.port,
                spec.database.as_deref(),
                spec.user.as_deref(),
                spec.password.as_deref(),
                vars,
            )?;
            let (rows, truncated) = crate::db::run_query(&server, &creds, &sql, spec.max_rows)?;
            if !env.quiet {
                println!(
                    "  {} {} row(s){}",
                    "✓ ok".green().bold(),
                    rows.len(),
                    if truncated { " (truncated)" } else { "" }
                );
            }
            if let Some(reg) = &task.register {
                vars.insert(
                    reg.clone(),
                    serde_json::to_string(&rows).unwrap_or_default(),
                );
            }
        }
        return Ok(());
    }

    if let Some(spec) = &task.db_exec {
        let server_name = render(&spec.server, vars);
        let sql = render(&spec.sql, vars);
        if !env.quiet {
            println!(
                "  {} {} on {}",
                "→".bold(),
                render_for_display(&spec.sql, vars).dimmed(),
                server_name.dimmed()
            );
        }
        if !env.dry {
            let confirmed = Confirmed::require(spec, "db_exec", &task.name)?;
            let output = exec_db_exec(&confirmed, &sql, vars, env)?;
            if !env.quiet {
                println!("  {} {}", "✓ ok".green().bold(), output.dimmed());
            }
            if let Some(reg) = &task.register {
                vars.insert(reg.clone(), output);
            }
        }
        return Ok(());
    }

    if let Some(spec) = &task.db_load {
        let server_name = render(&spec.server, vars);
        let table = render(&spec.table, vars);
        let file = render(&spec.file, vars);
        if !env.quiet {
            println!(
                "  {} load {} → {}:{}",
                "→".bold(),
                file.dimmed(),
                server_name.dimmed(),
                table.dimmed()
            );
        }
        if !env.dry {
            let confirmed = Confirmed::require(spec, "db_load", &task.name)?;
            let output = exec_db_load(&confirmed, vars, env)?;
            if !env.quiet {
                let summary = if output.is_empty() {
                    "loaded"
                } else {
                    output.as_str()
                };
                println!("  {} {}", "✓ ok".green().bold(), summary.dimmed());
            }
            if let Some(reg) = &task.register {
                vars.insert(reg.clone(), output);
            }
        }
        return Ok(());
    }

    if let Some(spec) = &task.secret_set {
        let profile = render(&spec.profile, vars);
        let key = render(&spec.key, vars);
        if !env.quiet {
            println!(
                "  {} secret.{}.{}",
                "→ set".bold(),
                profile.dimmed(),
                key.dimmed()
            );
        }
        if !env.dry {
            let confirmed = Confirmed::require(spec, "secret_set", &task.name)?;
            exec_secret_set(&confirmed, &profile, &key, vars)?;
            if !env.quiet {
                println!("  {} secret set", "✓ ok".green().bold());
            }
            if let Some(reg) = &task.register {
                vars.insert(reg.clone(), "true".to_string());
            }
        }
        return Ok(());
    }

    if let Some(spec) = &task.deploy {
        let server_name = render(&spec.server, vars);
        let path = render(&spec.path, vars);
        if !env.quiet {
            println!(
                "  {} deploy {} on {}",
                "→".bold(),
                path.dimmed(),
                server_name.dimmed()
            );
        }
        if !env.dry {
            let confirmed = Confirmed::require(spec, "deploy", &task.name)?;
            exec_deploy(&confirmed, vars, env)?;
            if !env.quiet {
                println!("  {} deployed", "✓ ok".green().bold());
            }
            if let Some(reg) = &task.register {
                vars.insert(reg.clone(), "true".to_string());
            }
        }
        return Ok(());
    }

    if let Some(spec) = &task.upload {
        let local = render(&spec.local, vars);
        let server_name = render(&spec.server, vars);
        let remote = render(&spec.remote, vars);
        if !env.quiet {
            println!(
                "  {} upload {} → {}:{}",
                "→".bold(),
                local.dimmed(),
                server_name.dimmed(),
                remote.dimmed()
            );
        }
        if !env.dry {
            let confirmed = Confirmed::require(spec, "upload", &task.name)?;
            let bytes = exec_upload(&confirmed, vars, env)?;
            if !env.quiet {
                println!("  {} uploaded ({bytes} bytes)", "✓ ok".green().bold());
            }
            if let Some(reg) = &task.register {
                vars.insert(reg.clone(), bytes.to_string());
            }
        }
        return Ok(());
    }

    if let Some(spec) = &task.cron {
        // exec_cron does its own per-operation preview/echo (like ssh:'s command line),
        // and validates exactly-one-of add/remove/list before touching the server.
        if !env.dry {
            let captured = exec_cron(task, spec, vars, env)?;
            if let Some(reg) = &task.register {
                vars.insert(reg.clone(), captured);
            }
        } else if !env.quiet {
            println!(
                "  {} cron on {}",
                "→".bold(),
                render(&spec.server, vars).dimmed()
            );
        }
        return Ok(());
    }

    if let Some(spec) = &task.mail {
        let to = render(&spec.to, vars);
        let subject = render(&spec.subject, vars);
        if !env.quiet {
            println!(
                "  {} mail to {} — {}",
                "→".bold(),
                render_for_display(&spec.to, vars).dimmed(),
                render_for_display(&spec.subject, vars).dimmed()
            );
        }
        if !env.dry {
            let cc = spec.cc.as_deref().map(|s| render(s, vars));
            let bcc = spec.bcc.as_deref().map(|s| render(s, vars));
            let body = render(&spec.body, vars);
            let from = spec.from.as_deref().map(|s| render(s, vars));
            let host = spec.host.as_deref().map(|s| render(s, vars));
            let user = spec.user.as_deref().map(|s| render(s, vars));
            let password = spec.password.as_deref().map(|s| render(s, vars));
            let server = spec.server.as_deref().map(|s| render(s, vars));
            let creds = resolve_mail_creds(
                env.ctx,
                server.as_deref(),
                host.as_deref(),
                spec.port,
                user.as_deref(),
                password.as_deref(),
                from.as_deref(),
                spec.tls.as_deref(),
            )?;
            let attachments = spec
                .attachments
                .iter()
                .map(|p| {
                    join_confined(&env.playbook_dir, &render(p, vars))
                        .map(ConfinedPath::into_path_buf)
                })
                .collect::<Result<Vec<_>>>()?;
            let count = send_mail(
                &creds,
                &to,
                cc.as_deref(),
                bcc.as_deref(),
                &subject,
                &body,
                spec.html,
                &attachments,
            )?;
            if !env.quiet {
                println!("  {} sent to {} recipient(s)", "✓ ok".green().bold(), count);
            }
            if let Some(reg) = &task.register {
                vars.insert(reg.clone(), "true".to_string());
            }
        }
        return Ok(());
    }

    if let Some(spec) = &task.mail_check {
        let server_name = render(&spec.server, vars);
        let folder = render(&spec.folder, vars);
        if !env.quiet {
            println!(
                "  {} checking {} on {} ({})",
                "→".bold(),
                folder.dimmed(),
                server_name.dimmed(),
                if spec.unseen_only { "unseen" } else { "all" }
            );
        }
        if !env.dry {
            let creds = resolve_imap_creds(env.ctx, &server_name)?;
            let messages = crate::commands::mail::fetch_mail(
                &creds,
                &folder,
                spec.unseen_only,
                spec.limit,
                spec.include_body,
                spec.mark_seen,
            )?;
            if !env.quiet {
                println!("  {} {} message(s)", "✓ ok".green().bold(), messages.len());
            }
            if let Some(reg) = &task.register {
                vars.insert(
                    reg.clone(),
                    serde_json::to_string(&messages).unwrap_or_default(),
                );
            }
        }
        return Ok(());
    }

    if let Some(spec) = &task.include {
        let rendered = render(spec.file(), vars);
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
            let sub_playbook_dir = include_path
                .parent()
                .unwrap_or(Path::new("."))
                .to_path_buf();

            // Render this include:'s vars: overrides against the *parent's* vars, before
            // any mutation below, and remember each overridden key's prior value (or that
            // it was absent) so it can be restored once the sub-playbook returns —
            // otherwise calling the same include: repeatedly with different vars: (e.g.
            // once per service in a per-service deploy) would leak the last call's values
            // into sibling tasks afterward.
            let rendered_overrides: Vec<(String, String)> = spec
                .vars()
                .iter()
                .map(|(k, v)| (k.clone(), render(v, vars)))
                .collect();
            let saved: Vec<(String, Option<String>)> = rendered_overrides
                .iter()
                .map(|(k, _)| (k.clone(), vars.get(k).cloned()))
                .collect();

            let sub_own_vars = load_playbook_vars(&sub_playbook, &sub_playbook_dir)?;
            for (k, v) in &sub_own_vars {
                vars.entry(k.clone()).or_insert_with(|| v.clone());
            }
            for (k, v) in &rendered_overrides {
                vars.insert(k.clone(), v.clone());
            }

            let sub_notes = read_notes(&include_path);
            let sub_env = RunEnv {
                playbook_dir: sub_playbook_dir,
                playbook_name: sub_playbook.name.clone(),
                audit_log: env.audit_log.clone(),
                project_root: env.project_root.clone(),
                dry: env.dry,
                diff: env.diff,
                quiet: env.quiet,
                auto_yes: env.auto_yes,
                start_at: None,
                state_path: None,
                data_path: None,
                keep_checkpoint: false,
                lock_path: None,
                ctx: env.ctx,
            };
            include_stack.push(include_path);
            let result = execute_playbook(
                &sub_playbook,
                &None,
                &None,
                &sub_notes,
                vars,
                include_stack,
                false,
                &sub_env,
            );
            include_stack.pop();

            // Restore each overridden key, regardless of outcome, so this include: call's
            // overrides don't leak into sibling tasks that follow it.
            for (k, prior) in saved {
                match prior {
                    Some(v) => {
                        vars.insert(k, v);
                    }
                    None => {
                        vars.remove(&k);
                    }
                }
            }

            result?;
        }
        return Ok(());
    }

    bail!(
        "task '{}' has no action (run, check_url, check_port, http, scrape, wait_for, \
         report, env_check, ssh, fleet, fs_cat, fs_write, systemd_restart, systemd_status, \
         logs_tail, logs_grep, ps_list, ps_kill, stat, include, assert, block, parallel, \
         debug, confirm, set_fact, include_vars, state_set, sync_db, sync_files, write_file, \
         template, read_csv, write_csv, db_query, db_exec, db_load, secret_set, deploy, upload, cron, \
         mail, mail_check, git_summary, git_changelog, gh_prs)",
        task.name
    );
}

/// Best-effort action label for a task, used only by `--audit-log` entries (see
/// `write_audit_entry`) — not for dispatch, that's `run_task_once`'s own if-chain above.
/// Same field list its "no action" error enumerates, checked in the same order; a task
/// with no action field set never reaches here in practice (`run_task_once` rejects it
/// first), so `"unknown"` is just a safe fallback, not an expected case.
/// Whether `task` is one of the confirm:-gated destructive actions (fs_write:/
/// systemd_restart:/ps_kill:/db_exec:/secret_set:/deploy:/upload:, and cron: when it
/// adds or removes a line -- see `Confirmed<T>`/`RequiresConfirm` above), and if so,
/// whether its own YAML already sets that gate. `None` for every other action -- most
/// tasks aren't destructive at all, so --list-tasks's output only carries this where it
/// means something.
pub(crate) fn confirm_gate(task: &Task) -> Option<bool> {
    if let Some(s) = &task.fs_write {
        return Some(s.is_confirmed());
    }
    if let Some(s) = &task.systemd_restart {
        return Some(s.is_confirmed());
    }
    if let Some(s) = &task.ps_kill {
        return Some(s.is_confirmed());
    }
    if let Some(s) = &task.db_exec {
        return Some(s.is_confirmed());
    }
    if let Some(s) = &task.db_load {
        return Some(s.is_confirmed());
    }
    if let Some(s) = &task.secret_set {
        return Some(s.is_confirmed());
    }
    if let Some(s) = &task.deploy {
        return Some(s.is_confirmed());
    }
    if let Some(s) = &task.upload {
        return Some(s.is_confirmed());
    }
    if let Some(s) = &task.cron {
        // list-only is read-only -- not a destructive action, so no gate to report.
        if s.add.is_some() || s.remove.is_some() {
            return Some(s.is_confirmed());
        }
    }
    if let Some(s) = &task.template {
        // A local render isn't destructive; only a push to a server is gated.
        if s.server.is_some() {
            return Some(s.is_confirmed());
        }
    }
    None
}

pub(crate) fn task_action_label(task: &Task) -> &'static str {
    // A plain bool, not an Option<T>, so it doesn't fit the check! macro below.
    if task.flush_handlers {
        return "flush_handlers";
    }
    macro_rules! check {
        ($($field:ident),+ $(,)?) => {
            $(if task.$field.is_some() { return stringify!($field); })+
        };
    }
    check!(
        run,
        check_url,
        check_port,
        http,
        scrape,
        wait_for,
        report,
        env_check,
        ssh,
        fleet,
        fs_cat,
        fs_write,
        systemd_restart,
        systemd_status,
        logs_tail,
        logs_grep,
        ps_list,
        ps_kill,
        stat,
        include,
        assert,
        block,
        parallel,
        debug,
        confirm,
        set_fact,
        include_vars,
        state_set,
        sync_db,
        sync_files,
        write_file,
        template,
        read_csv,
        write_csv,
        db_query,
        db_exec,
        db_load,
        secret_set,
        deploy,
        upload,
        cron,
        mail,
        mail_check,
        git_summary,
        git_changelog,
        gh_prs,
    );
    "unknown"
}

/// Appends one JSON line per task attempt to `env.audit_log`, if set — see
/// `RunEnv::audit_log`/`PlayArgs::audit_log`. Called from `run_task_once_with_retries`,
/// the single funnel every concrete attempt passes through, so this covers top-level
/// tasks, `block:`/`rescue:`/`always:`, `include:`, and each `loop:` iteration
/// uniformly — but not a task skipped via `when:` (that check happens one level up,
/// before `run_task_once_with_retries` is ever called), since a skipped task never
/// touched anything. A plain blocking append is intentional, same reasoning as
/// `commands::mcp::ToolerMcp::write_audit`: one line per task attempt isn't a hot path,
/// and a write failure here must never fail the task itself — it's only reported to
/// stderr.
/// Every value a task's own fields resolve `{{secret.<profile>.<key>}}` to — scanned by
/// Debug-formatting the whole task (covers every field on every action, present or
/// future, with no per-action-type enumeration to keep in sync) and pulling out each
/// `{{secret.<profile>.<key>}}` token, then resolving it for real via the OS keychain.
/// Used to scrub a captured error message before it's recorded anywhere (`--audit-log`,
/// `--output json`'s `error` field) — a `run:`/`ssh:` task's own subprocess can echo an
/// unmasked secret back in its stderr (its *argv* is real and unmasked, by necessity;
/// only tooler's own *printed preview* of it is masked, see `render_for_display`), so
/// redacting by known-value rather than by field name catches that too, not just
/// tooler's own interpolation. Best-effort: a keychain lookup failure is skipped
/// silently, same graceful-abstain `--lint`'s Check C already has.
pub(crate) fn task_secret_values(task: &Task) -> Vec<String> {
    let dump = format!("{task:?}");
    let mut out = Vec::new();
    let mut rest = dump.as_str();
    while let Some(start) = rest.find("{{secret.") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else { break };
        let token = &after[..end];
        rest = &after[end + 2..];
        let Some(profile_key) = token.strip_prefix("secret.") else {
            continue;
        };
        let Some((profile, key)) = profile_key.split_once('.') else {
            continue;
        };
        if let Ok(Some(value)) = crate::secrets::get_secret(profile, key)
            && !value.is_empty()
        {
            out.push(value);
        }
    }
    out
}

/// Replaces every occurrence of each (non-empty) secret value with `***`.
pub(crate) fn redact_secrets(text: &str, secrets: &[String]) -> String {
    let mut out = text.to_string();
    for s in secrets {
        if !s.is_empty() {
            out = out.replace(s.as_str(), "***");
        }
    }
    out
}

pub(crate) fn write_audit_entry(
    env: &RunEnv,
    task: &Task,
    status: &str,
    error: Option<&str>,
    duration: Duration,
) {
    let Some(path) = &env.audit_log else {
        return;
    };
    let error_kind = error.map(classify_error);
    let redacted;
    let error = match error {
        Some(e) => {
            redacted = redact_secrets(e, &task_secret_values(task));
            Some(redacted.as_str())
        }
        None => None,
    };
    let line = serde_json::json!({
        "ts": chrono::Utc::now().to_rfc3339(),
        "playbook": env.playbook_name,
        "task": task.name,
        "action": task_action_label(task),
        "status": status,
        "duration_ms": duration.as_millis(),
        "error": error,
        "error_kind": error_kind,
    });
    use std::io::Write;
    let result = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| writeln!(f, "{line}"));
    if let Err(e) = result {
        eprintln!(
            "tooler play: failed to write audit log {}: {e}",
            path.display()
        );
    }
}

/// Builds `Credentials` for one side of a `sync_db:` task, rendering each field through
/// `render()` first, then delegating to `commands::db::resolve_credentials` — the exact
/// same engine/host/port/database/user/password resolution `tooler db backup`/`restore`
/// already use, so `sync_db:` inherits the same validation and `--env`-file support.
pub(crate) fn resolve_db_sync_creds(
    server: &crate::config::Server,
    side: &DbSyncSide,
    vars: &HashMap<String, String>,
) -> Result<crate::db::Credentials> {
    resolve_conn_creds(
        server,
        side.env.as_deref(),
        side.engine.as_deref(),
        side.host.as_deref(),
        side.port,
        side.database.as_deref(),
        side.user.as_deref(),
        side.password.as_deref(),
        vars,
    )
}

/// Renders each (possibly-`{{var}}`-templated) connection field against `vars` and
/// delegates to `commands::db::resolve_credentials` — the exact engine/host/port/
/// database/user/password resolution `tooler db backup`/`restore`/`query` already use, so
/// both `sync_db:` (via `resolve_db_sync_creds`) and `db_query:` inherit the same
/// validation and `--env`-file support from one place.
#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve_conn_creds(
    server: &crate::config::Server,
    env: Option<&str>,
    engine: Option<&str>,
    host: Option<&str>,
    port: Option<u16>,
    database: Option<&str>,
    user: Option<&str>,
    password: Option<&str>,
    vars: &HashMap<String, String>,
) -> Result<crate::db::Credentials> {
    let env = env.map(|s| render(s, vars));
    let engine = engine.map(|s| render(s, vars));
    let host = host.map(|s| render(s, vars));
    let database = database.map(|s| render(s, vars));
    let user = user.map(|s| render(s, vars));
    let password = password.map(|s| render(s, vars));
    crate::commands::db::resolve_credentials(
        server,
        &crate::commands::db::ConnOpts {
            env: env.as_deref(),
            engine: engine.as_deref(),
            host: host.as_deref(),
            port,
            database: database.as_deref(),
            user: user.as_deref(),
            password: password.as_deref(),
        },
    )
}
