use crate::{commands::ssh::resolve_server, context::Context, db, output::OutputFormat};
use anyhow::{Result, bail};
use clap::{Args, Subcommand};
use colored::Colorize;

#[derive(Args)]
pub struct SystemdArgs {
    #[command(subcommand)]
    pub subcommand: SystemdSubcommand,
}

#[derive(Subcommand)]
pub enum SystemdSubcommand {
    /// Show a systemd unit's status on a remote server (systemctl status)
    Status {
        /// Server profile (see: tooler server list)
        server: String,
        /// Unit name, e.g. nginx or myapp.service
        unit: String,
    },
    /// Restart a systemd unit on a remote server (systemctl restart)
    Restart {
        server: String,
        unit: String,
        /// Run via sudo
        #[arg(long)]
        sudo: bool,
        /// Sudo password [env: TOOLER_SUDO_PASS] (only used with --sudo; omit to rely on NOPASSWD)
        #[arg(long, env = "TOOLER_SUDO_PASS")]
        sudo_pass: Option<String>,
    },
    /// Show recent journal entries for a unit (journalctl -u)
    Logs {
        server: String,
        unit: String,
        /// Number of lines
        #[arg(long, default_value_t = 100)]
        lines: u32,
        /// Run via sudo (some systems restrict journal access to root)
        #[arg(long)]
        sudo: bool,
    },
}

pub fn run(args: SystemdArgs, ctx: &Context) -> Result<()> {
    match args.subcommand {
        SystemdSubcommand::Status { server, unit } => status(&server, &unit, ctx),
        SystemdSubcommand::Restart {
            server,
            unit,
            sudo,
            sudo_pass,
        } => restart(&server, &unit, sudo, sudo_pass.as_deref(), ctx),
        SystemdSubcommand::Logs {
            server,
            unit,
            lines,
            sudo,
        } => logs(&server, &unit, lines, sudo, ctx),
    }
}

fn fail(json: bool, message: String) -> Result<()> {
    if json {
        println!("{}", serde_json::json!({ "error": message }));
        std::process::exit(1);
    }
    bail!(message);
}

pub(crate) fn status_cmd(unit: &str) -> String {
    format!("systemctl status {} --no-pager", db::shell_quote(unit))
}

pub(crate) fn restart_cmd(unit: &str, sudo: bool, sudo_pass: Option<&str>) -> String {
    format!(
        "{}systemctl restart {}",
        db::sudo_prefix(sudo, sudo_pass),
        db::shell_quote(unit)
    )
}

fn logs_cmd(unit: &str, lines: u32, sudo: bool) -> String {
    format!(
        "{}journalctl -u {} -n {lines} --no-pager",
        db::sudo_prefix(sudo, None),
        db::shell_quote(unit)
    )
}

/// `systemctl status` normally writes to stdout, but a shell-level failure to even
/// launch it (e.g. `systemctl: command not found` on a non-systemd host like FreeBSD)
/// lands on stderr instead, with empty stdout -- fall back to stderr in that case so
/// the caller sees *why* the unit looks inactive rather than a silently empty output.
pub(crate) fn merge_output(stdout: String, stderr: String) -> String {
    if stdout.trim().is_empty() && !stderr.trim().is_empty() {
        stderr
    } else {
        stdout
    }
}

fn status(server_name: &str, unit: &str, ctx: &Context) -> Result<()> {
    let json = ctx.output == OutputFormat::Json;

    let server = match resolve_server(ctx, server_name) {
        Ok(s) => s,
        Err(e) => return fail(json, format!("{e:#}")),
    };
    let (output, active) = match db::ssh_exec_capture_lenient(&server, &status_cmd(unit)) {
        Ok((stdout, stderr, success)) => (merge_output(stdout, stderr), success),
        Err(e) => return fail(json, format!("{e:#}")),
    };

    if json {
        println!(
            "{}",
            serde_json::json!({
                "server": server_name,
                "unit": unit,
                "active": active,
                "output": output,
            })
        );
        return Ok(());
    }

    let marker = if active {
        "●".green().bold()
    } else {
        "●".red().bold()
    };
    println!("{marker} {} on {}", unit.bold(), server_name.cyan());
    println!("{}", "─".repeat(50).dimmed());
    print!("{output}");
    Ok(())
}

fn restart(
    server_name: &str,
    unit: &str,
    sudo: bool,
    sudo_pass: Option<&str>,
    ctx: &Context,
) -> Result<()> {
    let json = ctx.output == OutputFormat::Json;

    let server = match resolve_server(ctx, server_name) {
        Ok(s) => s,
        Err(e) => return fail(json, format!("{e:#}")),
    };
    if let Err(e) = db::ssh_exec_capture(&server, &restart_cmd(unit, sudo, sudo_pass)) {
        return fail(json, format!("{e:#}"));
    }

    if json {
        println!(
            "{}",
            serde_json::json!({"server": server_name, "unit": unit, "restarted": true})
        );
        return Ok(());
    }
    println!(
        "{} {} restarted on {}",
        "✓".green().bold(),
        unit.bold(),
        server_name.cyan()
    );
    Ok(())
}

fn logs(server_name: &str, unit: &str, lines: u32, sudo: bool, ctx: &Context) -> Result<()> {
    let json = ctx.output == OutputFormat::Json;

    let server = match resolve_server(ctx, server_name) {
        Ok(s) => s,
        Err(e) => return fail(json, format!("{e:#}")),
    };
    let output = match db::ssh_exec_capture(&server, &logs_cmd(unit, lines, sudo)) {
        Ok(o) => o,
        Err(e) => return fail(json, format!("{e:#}")),
    };

    if json {
        let lines: Vec<&str> = output.lines().collect();
        println!(
            "{}",
            serde_json::json!({"server": server_name, "unit": unit, "lines": lines})
        );
        return Ok(());
    }

    println!(
        "{} {} on {}",
        "journal:".bold().cyan(),
        unit.bold(),
        server_name.cyan()
    );
    println!("{}", "─".repeat(50).dimmed());
    print!("{output}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_output_prefers_stdout_when_present() {
        assert_eq!(
            merge_output("active (running)".to_string(), String::new()),
            "active (running)"
        );
    }

    #[test]
    fn merge_output_falls_back_to_stderr_when_stdout_empty() {
        assert_eq!(
            merge_output(String::new(), "systemctl: command not found".to_string()),
            "systemctl: command not found"
        );
    }

    #[test]
    fn merge_output_empty_when_both_empty() {
        assert_eq!(merge_output(String::new(), String::new()), "");
    }

    #[test]
    fn status_cmd_quotes_unit() {
        assert_eq!(
            status_cmd("nginx.service"),
            "systemctl status 'nginx.service' --no-pager"
        );
    }

    #[test]
    fn restart_cmd_without_sudo() {
        assert_eq!(
            restart_cmd("nginx", false, None),
            "systemctl restart 'nginx'"
        );
    }

    #[test]
    fn restart_cmd_with_sudo_and_password() {
        assert_eq!(
            restart_cmd("nginx", true, Some("pw")),
            "echo 'pw' | sudo -S systemctl restart 'nginx'"
        );
    }

    #[test]
    fn logs_cmd_builds_journalctl_invocation() {
        assert_eq!(
            logs_cmd("nginx", 50, false),
            "journalctl -u 'nginx' -n 50 --no-pager"
        );
    }

    #[test]
    fn logs_cmd_with_sudo() {
        assert_eq!(
            logs_cmd("nginx", 50, true),
            "sudo journalctl -u 'nginx' -n 50 --no-pager"
        );
    }
}
