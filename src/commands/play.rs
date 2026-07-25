use crate::{context::Context, output::OutputFormat};
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

    // Actions — only one should be set per task
    run: Option<String>,
    check_url: Option<String>,
    check_port: Option<CheckPortSpec>,
    env_check: Option<EnvCheckSpec>,
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

pub fn run(args: PlayArgs, ctx: &Context) -> Result<()> {
    if args.init {
        let path = args.file.as_deref().unwrap_or("playbook.yml");
        return write_sample(path, ctx);
    }

    let file = args.file.as_deref().ok_or_else(|| {
        anyhow::anyhow!("Specify a playbook file, or use --init to generate one.")
    })?;

    let file_path = PathBuf::from(file);
    let playbook_dir = file_path
        .canonicalize()
        .with_context(|| format!("Cannot resolve path: {file}"))?
        .parent()
        .unwrap_or(Path::new("."))
        .to_path_buf();

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

    execute_playbook(&playbook, &playbook_dir, &tag_filter, args.dry, ctx)
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

        let result = run_task(task, &playbook.vars, playbook_dir, dry, json);

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

fn run_task(
    task: &Task,
    vars: &HashMap<String, String>,
    playbook_dir: &Path,
    dry: bool,
    quiet: bool,
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

    bail!(
        "task '{}' has no action (run, check_url, check_port, env_check)",
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

fn write_sample(path: &str, ctx: &Context) -> Result<()> {
    let json = ctx.output == OutputFormat::Json;
    if std::path::Path::new(path).exists() {
        let message = format!("'{}' already exists.", path);
        if json {
            println!("{}", serde_json::json!({"path": path, "error": message}));
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
        println!("{}", serde_json::json!({"created": path}));
        return Ok(());
    }
    println!("{} {}", "created".green().bold(), path.cyan());
    println!("{}", "Run it with: tooler play playbook.yml".dimmed());
    Ok(())
}
