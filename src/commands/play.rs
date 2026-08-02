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
}

#[derive(Debug, Deserialize)]
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

    // Actions — only one should be set per task
    run: Option<String>,
    check_url: Option<String>,
    check_port: Option<CheckPortSpec>,
    env_check: Option<EnvCheckSpec>,
    ssh: Option<SshSpec>,
    fleet: Option<FleetSpec>,
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

    execute_playbook(&playbook, &playbook_dir, &tag_filter, args.dry, &notes, ctx)
}

// ── Runner ────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct TaskOutcome {
    name: String,
    status: &'static str,
    error: Option<String>,
}

fn execute_playbook(
    playbook: &Playbook,
    playbook_dir: &Path,
    tag_filter: &Option<Vec<&str>>,
    dry: bool,
    notes: &Option<String>,
    ctx: &Context,
) -> Result<()> {
    let json = ctx.output == OutputFormat::Json;
    let sep = "─".repeat(56);

    if !json {
        println!(
            "\n{} {} {}",
            "PLAY".bold().cyan(),
            format!("[{}]", playbook.name).bold(),
            playbook_dir.display().to_string().dimmed()
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
            && !eval_when(w, &playbook.vars)
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

        let result = run_task(task, &playbook.vars, playbook_dir, dry, json, ctx);

        match result {
            Ok(_) => {
                if dry {
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
                                "dry": dry,
                                "notes": notes,
                                "tasks": outcomes,
                                "ok": ok,
                                "failed": failed,
                                "skipped": skipped,
                                "success": false,
                            })
                        );
                        std::process::exit(1);
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

    if json {
        println!(
            "{}",
            serde_json::json!({
                "playbook": playbook.name,
                "dry": dry,
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

/// Expands `loop:` (if present) into one `run_task_once` call per item, with `{{item}}`
/// added to that iteration's vars. The first failing iteration fails the whole task —
/// remaining items are not attempted, same as any other task failure.
fn run_task(
    task: &Task,
    vars: &HashMap<String, String>,
    playbook_dir: &Path,
    dry: bool,
    quiet: bool,
    ctx: &Context,
) -> Result<()> {
    let Some(items) = &task.loop_items else {
        return run_task_once(task, vars, playbook_dir, dry, quiet, ctx);
    };
    for item in items {
        let mut loop_vars = vars.clone();
        loop_vars.insert("item".to_string(), item.clone());
        if !quiet {
            println!("  {} item={}", "→".dimmed(), item.dimmed());
        }
        run_task_once(task, &loop_vars, playbook_dir, dry, quiet, ctx)?;
    }
    Ok(())
}

fn run_task_once(
    task: &Task,
    vars: &HashMap<String, String>,
    playbook_dir: &Path,
    dry: bool,
    quiet: bool,
    ctx: &Context,
) -> Result<()> {
    if let Some(cmd) = &task.run {
        let cmd = render(cmd, vars);
        if !quiet {
            println!("  {} {}", "$".bold().green(), cmd.dimmed());
        }
        if !dry {
            let start = Instant::now();
            let status = std::process::Command::new("sh")
                .arg("-c")
                .arg(&cmd)
                .current_dir(playbook_dir)
                .status()?;
            let elapsed = start.elapsed();
            if status.success() {
                if !quiet {
                    println!(
                        "  {} {}",
                        "✓ ok".green().bold(),
                        format!("({:.1}s)", elapsed.as_secs_f32()).dimmed()
                    );
                }
            } else {
                bail!("command exited with code {}", status.code().unwrap_or(1));
            }
        }
        return Ok(());
    }

    if let Some(url) = &task.check_url {
        let url = render(url, vars);
        if !quiet {
            println!("  {} {}", "→".bold(), url.dimmed());
        }
        if !dry {
            check_url(&url, quiet)?;
        }
        return Ok(());
    }

    if let Some(spec) = &task.check_port {
        let host = render(&spec.host, vars);
        if !quiet {
            println!("  {} {}:{}", "→".bold(), host.dimmed(), spec.port);
        }
        if !dry {
            check_port(&host, spec.port, spec.timeout, quiet)?;
        }
        return Ok(());
    }

    if let Some(spec) = &task.env_check {
        let reference = playbook_dir.join(render(&spec.reference, vars));
        let target = playbook_dir.join(render(&spec.target, vars));
        if !quiet {
            println!(
                "  {} {} → {}",
                "→".bold(),
                reference.display().to_string().dimmed(),
                target.display().to_string().dimmed()
            );
        }
        if !dry {
            env_check(
                &reference.to_string_lossy(),
                &target.to_string_lossy(),
                quiet,
            )?;
        }
        return Ok(());
    }

    if let Some(spec) = &task.ssh {
        let server_name = render(&spec.server, vars);
        let cmd = render(&spec.command, vars);
        let full_cmd = crate::commands::fleet::exec_command(&cmd, spec.sudo);
        if !quiet {
            println!(
                "  {} {} {} {}",
                "→".bold(),
                full_cmd.dimmed(),
                "on".dimmed(),
                server_name.dimmed()
            );
        }
        if !dry {
            let server = crate::commands::ssh::resolve_server(ctx, &server_name)?;
            let (stdout, stderr, success) =
                crate::db::ssh_exec_capture_lenient(&server, &full_cmd)?;
            if success {
                let out = stdout.trim();
                if !quiet && !out.is_empty() {
                    println!("{out}");
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
        if !quiet {
            println!("  {} {}", "→".bold(), command.dimmed());
        }
        if !dry {
            let results = crate::commands::fleet::run_on_targets(
                ctx,
                spec.servers.as_deref(),
                spec.all,
                spec.group.as_deref(),
                &command,
                spec.sudo,
            )?;
            let ok_count = results.iter().filter(|r| r.success).count();
            let all_succeeded = ok_count == results.len();
            if !quiet {
                for r in &results {
                    if r.success {
                        println!("    {} {}", "✓".green().bold(), r.server.cyan());
                    } else {
                        println!("    {} {}", "✗".red().bold(), r.server.cyan());
                    }
                }
            }
            if !all_succeeded {
                bail!("{}/{} servers succeeded", ok_count, results.len());
            }
        }
        return Ok(());
    }

    bail!(
        "task '{}' has no action (run, check_url, check_port, env_check, ssh, fleet)",
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

fn render(s: &str, vars: &HashMap<String, String>) -> String {
    let mut out = s.to_string();
    for (k, v) in vars {
        out = out.replace(&format!("{{{{{k}}}}}"), v);
    }
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
}
