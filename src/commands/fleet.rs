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
        /// Comma-separated server profile names (mutually exclusive with --all/--group)
        #[arg(long, conflicts_with_all = ["all", "group"])]
        servers: Option<String>,
        /// Target every configured server profile
        #[arg(long, conflicts_with_all = ["servers", "group"])]
        all: bool,
        /// Target a named server group (see: tooler group list)
        #[arg(long, conflicts_with_all = ["servers", "all"])]
        group: Option<String>,
        /// Command to run
        command: String,
        /// Run command with sudo
        #[arg(long)]
        sudo: bool,
        /// Run on all targeted servers concurrently instead of one at a time
        #[arg(long)]
        parallel: bool,
    },
    /// Check SSH connectivity to multiple servers
    Check {
        /// Comma-separated server profile names (mutually exclusive with --all/--group)
        #[arg(long, conflicts_with_all = ["all", "group"])]
        servers: Option<String>,
        /// Target every configured server profile
        #[arg(long, conflicts_with_all = ["servers", "group"])]
        all: bool,
        /// Target a named server group (see: tooler group list)
        #[arg(long, conflicts_with_all = ["servers", "all"])]
        group: Option<String>,
        /// Check all targeted servers concurrently instead of one at a time
        #[arg(long)]
        parallel: bool,
    },
}

pub fn run(args: FleetArgs, ctx: &Context) -> Result<()> {
    match args.subcommand {
        FleetSubcommand::Exec {
            servers,
            all,
            group,
            command,
            sudo,
            parallel,
        } => exec(
            servers.as_deref(),
            all,
            group.as_deref(),
            &command,
            sudo,
            parallel,
            ctx,
        ),
        FleetSubcommand::Check {
            servers,
            all,
            group,
            parallel,
        } => check(servers.as_deref(), all, group.as_deref(), parallel, ctx),
    }
}

fn fail(json: bool, message: String) -> Result<()> {
    if json {
        println!("{}", serde_json::json!({ "error": message }));
        std::process::exit(1);
    }
    bail!(message);
}

/// Resolves `--servers a,b,c` / `--all` / `--group <name>` into an ordered list of
/// server profile names.
pub(crate) fn resolve_targets(
    ctx: &Context,
    servers: Option<&str>,
    all: bool,
    group: Option<&str>,
) -> Result<Vec<String>> {
    if all {
        let mut names: Vec<String> = ctx.config.server.keys().cloned().collect();
        names.sort();
        return Ok(names);
    }
    if let Some(g) = group {
        let grp = ctx
            .config
            .group
            .get(g)
            .ok_or_else(|| anyhow::anyhow!("Group '{g}' not found"))?;
        let mut members = grp.members.clone();
        members.sort();
        return Ok(members);
    }
    match servers {
        Some(list) => Ok(list
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect()),
        None => bail!("pass --servers <a,b,c>, --group <name>, or --all"),
    }
}

pub(crate) fn exec_command(command: &str, sudo: bool) -> String {
    if sudo {
        format!("sudo {command}")
    } else {
        command.to_string()
    }
}

#[derive(Serialize)]
pub(crate) struct ExecResult {
    pub(crate) server: String,
    pub(crate) success: bool,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

fn exec_on_server(ctx: &Context, name: &str, full_cmd: &str) -> ExecResult {
    match resolve_server(ctx, name) {
        Ok(server) => match db::ssh_exec_capture_lenient(&server, full_cmd) {
            Ok((stdout, stderr, success)) => ExecResult {
                server: name.to_string(),
                success,
                stdout,
                stderr,
            },
            Err(e) => ExecResult {
                server: name.to_string(),
                success: false,
                stdout: String::new(),
                stderr: format!("{e:#}"),
            },
        },
        Err(e) => ExecResult {
            server: name.to_string(),
            success: false,
            stdout: String::new(),
            stderr: format!("{e:#}"),
        },
    }
}

/// Resolves targets (`--servers`/`--all`/`--group`) and runs `command` on each over SSH,
/// continuing past a failing/unresolvable server rather than aborting the batch. Shared
/// by `tooler fleet exec` and `tooler play`'s native `fleet:` task type. `parallel: true`
/// runs all targets concurrently (`std::thread::scope` — `Context` is plain owned data,
/// safe to share by reference across threads); output order matches `resolve_targets`'
/// (already sorted) either way.
pub(crate) fn run_on_targets(
    ctx: &Context,
    servers: Option<&str>,
    all: bool,
    group: Option<&str>,
    command: &str,
    sudo: bool,
    parallel: bool,
) -> Result<Vec<ExecResult>> {
    let names = resolve_targets(ctx, servers, all, group)?;
    if names.is_empty() {
        bail!("no servers matched");
    }

    let full_cmd = exec_command(command, sudo);

    if parallel {
        Ok(std::thread::scope(|scope| {
            let handles: Vec<_> = names
                .iter()
                .map(|name| scope.spawn(|| exec_on_server(ctx, name, &full_cmd)))
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        }))
    } else {
        Ok(names
            .iter()
            .map(|name| exec_on_server(ctx, name, &full_cmd))
            .collect())
    }
}

fn exec(
    servers: Option<&str>,
    all: bool,
    group: Option<&str>,
    command: &str,
    sudo: bool,
    parallel: bool,
    ctx: &Context,
) -> Result<()> {
    let json = ctx.output == OutputFormat::Json;

    let full_cmd = exec_command(command, sudo);
    let results = match run_on_targets(ctx, servers, all, group, command, sudo, parallel) {
        Ok(r) => r,
        Err(e) => return fail(json, format!("{e:#}")),
    };

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

fn check_on_server(ctx: &Context, name: &str) -> CheckResult {
    match resolve_server(ctx, name) {
        Ok(server) => {
            let host = server.host_target();
            match db::ssh_exec_capture_lenient(&server, "echo ok") {
                Ok((_, stderr, success)) => CheckResult {
                    server: name.to_string(),
                    host,
                    success,
                    error: if success {
                        None
                    } else {
                        Some(stderr.trim().to_string())
                    },
                },
                Err(e) => CheckResult {
                    server: name.to_string(),
                    host,
                    success: false,
                    error: Some(format!("{e:#}")),
                },
            }
        }
        Err(e) => CheckResult {
            server: name.to_string(),
            host: String::new(),
            success: false,
            error: Some(format!("{e:#}")),
        },
    }
}

fn check(
    servers: Option<&str>,
    all: bool,
    group: Option<&str>,
    parallel: bool,
    ctx: &Context,
) -> Result<()> {
    let json = ctx.output == OutputFormat::Json;

    let names = match resolve_targets(ctx, servers, all, group) {
        Ok(n) => n,
        Err(e) => return fail(json, format!("{e:#}")),
    };
    if names.is_empty() {
        return fail(json, "no servers matched".to_string());
    }

    let results: Vec<CheckResult> = if parallel {
        std::thread::scope(|scope| {
            let handles: Vec<_> = names
                .iter()
                .map(|name| scope.spawn(|| check_on_server(ctx, name)))
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        })
    } else {
        names
            .iter()
            .map(|name| check_on_server(ctx, name))
            .collect()
    };

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
    use crate::config::{Config, Group, Server};
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

    fn ctx_with_group(group_name: &str, members: &[&str]) -> Context {
        let mut group = HashMap::new();
        group.insert(
            group_name.to_string(),
            Group {
                members: members.iter().map(|m| (*m).to_string()).collect(),
            },
        );
        let config = Config {
            group,
            ..Default::default()
        };
        Context::new(OutputFormat::Json, "default".to_string(), config)
    }

    #[test]
    fn resolve_targets_splits_comma_list_and_trims_whitespace() {
        let ctx = ctx_with_servers(&[]);
        let names = resolve_targets(&ctx, Some("a, b ,c"), false, None).unwrap();
        assert_eq!(names, vec!["a", "b", "c"]);
    }

    #[test]
    fn resolve_targets_all_returns_sorted_names() {
        let ctx = ctx_with_servers(&["zebra", "alpha", "mid"]);
        let names = resolve_targets(&ctx, None, true, None).unwrap();
        assert_eq!(names, vec!["alpha", "mid", "zebra"]);
    }

    #[test]
    fn resolve_targets_errors_when_neither_given() {
        let ctx = ctx_with_servers(&[]);
        assert!(resolve_targets(&ctx, None, false, None).is_err());
    }

    #[test]
    fn resolve_targets_group_returns_sorted_members() {
        let ctx = ctx_with_group("web", &["zebra", "alpha"]);
        let names = resolve_targets(&ctx, None, false, Some("web")).unwrap();
        assert_eq!(names, vec!["alpha", "zebra"]);
    }

    #[test]
    fn resolve_targets_errors_on_unknown_group() {
        let ctx = ctx_with_group("web", &["alpha"]);
        assert!(resolve_targets(&ctx, None, false, Some("bogus")).is_err());
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
