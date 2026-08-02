use crate::{commands::ssh::resolve_server, context::Context, db, output::OutputFormat};
use anyhow::{Result, bail};
use clap::{Args, Subcommand};
use colored::Colorize;
use serde::Serialize;

#[derive(Args)]
pub struct FleetArgs {
    #[command(subcommand)]
    pub subcommand: FleetSubcommand,
}

#[derive(Subcommand)]
pub enum FleetSubcommand {
    /// Run a command on multiple servers over SSH
    Exec {
        /// Comma-separated server profile names (mutually exclusive with --all)
        #[arg(long, conflicts_with = "all")]
        servers: Option<String>,
        /// Target every configured server profile
        #[arg(long)]
        all: bool,
        /// Command to run
        command: String,
        /// Run command with sudo
        #[arg(long)]
        sudo: bool,
    },
    /// Check SSH connectivity to multiple servers
    Check {
        /// Comma-separated server profile names (mutually exclusive with --all)
        #[arg(long, conflicts_with = "all")]
        servers: Option<String>,
        /// Target every configured server profile
        #[arg(long)]
        all: bool,
    },
}

pub fn run(args: FleetArgs, ctx: &Context) -> Result<()> {
    match args.subcommand {
        FleetSubcommand::Exec {
            servers,
            all,
            command,
            sudo,
        } => exec(servers.as_deref(), all, &command, sudo, ctx),
        FleetSubcommand::Check { servers, all } => check(servers.as_deref(), all, ctx),
    }
}

fn fail(json: bool, message: String) -> Result<()> {
    if json {
        println!("{}", serde_json::json!({ "error": message }));
        std::process::exit(1);
    }
    bail!(message);
}

/// Resolves `--servers a,b,c` / `--all` into an ordered list of server profile names.
fn resolve_targets(ctx: &Context, servers: Option<&str>, all: bool) -> Result<Vec<String>> {
    if all {
        let mut names: Vec<String> = ctx.config.server.keys().cloned().collect();
        names.sort();
        return Ok(names);
    }
    match servers {
        Some(list) => Ok(list
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect()),
        None => bail!("pass --servers <a,b,c> or --all"),
    }
}

fn exec_command(command: &str, sudo: bool) -> String {
    if sudo {
        format!("sudo {command}")
    } else {
        command.to_string()
    }
}

#[derive(Serialize)]
struct ExecResult {
    server: String,
    success: bool,
    stdout: String,
    stderr: String,
}

fn exec(servers: Option<&str>, all: bool, command: &str, sudo: bool, ctx: &Context) -> Result<()> {
    let json = ctx.output == OutputFormat::Json;

    let names = match resolve_targets(ctx, servers, all) {
        Ok(n) => n,
        Err(e) => return fail(json, format!("{e:#}")),
    };
    if names.is_empty() {
        return fail(json, "no servers matched".to_string());
    }

    let full_cmd = exec_command(command, sudo);

    let results: Vec<ExecResult> = names
        .iter()
        .map(|name| match resolve_server(ctx, name) {
            Ok(server) => match db::ssh_exec_capture_lenient(&server, &full_cmd) {
                Ok((stdout, stderr, success)) => ExecResult {
                    server: name.clone(),
                    success,
                    stdout,
                    stderr,
                },
                Err(e) => ExecResult {
                    server: name.clone(),
                    success: false,
                    stdout: String::new(),
                    stderr: format!("{e:#}"),
                },
            },
            Err(e) => ExecResult {
                server: name.clone(),
                success: false,
                stdout: String::new(),
                stderr: format!("{e:#}"),
            },
        })
        .collect();

    let ok_count = results.iter().filter(|r| r.success).count();
    let all_succeeded = ok_count == results.len();

    if json {
        println!(
            "{}",
            serde_json::json!({
                "command": full_cmd,
                "results": results,
                "all_succeeded": all_succeeded,
            })
        );
        if !all_succeeded {
            std::process::exit(1);
        }
        return Ok(());
    }

    for r in &results {
        if r.success {
            println!("{} {}", "✓".green().bold(), r.server.cyan());
            let out = r.stdout.trim();
            if !out.is_empty() {
                println!("{out}");
            }
        } else {
            println!("{} {}", "✗".red().bold(), r.server.cyan());
            let err = r.stderr.trim();
            println!("{}", if err.is_empty() { "(no output)" } else { err });
        }
    }
    println!("{}/{} servers succeeded", ok_count, results.len());
    if !all_succeeded {
        std::process::exit(1);
    }
    Ok(())
}

#[derive(Serialize)]
struct CheckResult {
    server: String,
    host: String,
    success: bool,
    error: Option<String>,
}

fn check(servers: Option<&str>, all: bool, ctx: &Context) -> Result<()> {
    let json = ctx.output == OutputFormat::Json;

    let names = match resolve_targets(ctx, servers, all) {
        Ok(n) => n,
        Err(e) => return fail(json, format!("{e:#}")),
    };
    if names.is_empty() {
        return fail(json, "no servers matched".to_string());
    }

    let results: Vec<CheckResult> = names
        .iter()
        .map(|name| match resolve_server(ctx, name) {
            Ok(server) => {
                let host = server.host_target();
                match db::ssh_exec_capture_lenient(&server, "echo ok") {
                    Ok((_, stderr, success)) => CheckResult {
                        server: name.clone(),
                        host,
                        success,
                        error: if success {
                            None
                        } else {
                            Some(stderr.trim().to_string())
                        },
                    },
                    Err(e) => CheckResult {
                        server: name.clone(),
                        host,
                        success: false,
                        error: Some(format!("{e:#}")),
                    },
                }
            }
            Err(e) => CheckResult {
                server: name.clone(),
                host: String::new(),
                success: false,
                error: Some(format!("{e:#}")),
            },
        })
        .collect();

    let ok_count = results.iter().filter(|r| r.success).count();
    let all_succeeded = ok_count == results.len();

    if json {
        println!(
            "{}",
            serde_json::json!({
                "results": results,
                "all_succeeded": all_succeeded,
            })
        );
        if !all_succeeded {
            std::process::exit(1);
        }
        return Ok(());
    }

    for r in &results {
        if r.success {
            println!(
                "{} {} ({})",
                "✓".green().bold(),
                r.server.cyan(),
                r.host.dimmed()
            );
        } else {
            println!(
                "{} {} ({}) — {}",
                "✗".red().bold(),
                r.server.cyan(),
                r.host.dimmed(),
                r.error.as_deref().unwrap_or("unreachable")
            );
        }
    }
    println!("{}/{} servers reachable", ok_count, results.len());
    if !all_succeeded {
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Server};
    use crate::output::OutputFormat;
    use std::collections::HashMap;

    fn ctx_with_servers(names: &[&str]) -> Context {
        let mut server = HashMap::new();
        for name in names {
            server.insert((*name).to_string(), Server::default());
        }
        let config = Config {
            server,
            ..Default::default()
        };
        Context::new(OutputFormat::Json, "default".to_string(), config)
    }

    #[test]
    fn resolve_targets_splits_comma_list_and_trims_whitespace() {
        let ctx = ctx_with_servers(&[]);
        let names = resolve_targets(&ctx, Some("a, b ,c"), false).unwrap();
        assert_eq!(names, vec!["a", "b", "c"]);
    }

    #[test]
    fn resolve_targets_all_returns_sorted_names() {
        let ctx = ctx_with_servers(&["zebra", "alpha", "mid"]);
        let names = resolve_targets(&ctx, None, true).unwrap();
        assert_eq!(names, vec!["alpha", "mid", "zebra"]);
    }

    #[test]
    fn resolve_targets_errors_when_neither_given() {
        let ctx = ctx_with_servers(&[]);
        assert!(resolve_targets(&ctx, None, false).is_err());
    }

    #[test]
    fn exec_command_prefixes_sudo() {
        assert_eq!(exec_command("uptime", true), "sudo uptime");
    }

    #[test]
    fn exec_command_without_sudo() {
        assert_eq!(exec_command("uptime", false), "uptime");
    }
}
