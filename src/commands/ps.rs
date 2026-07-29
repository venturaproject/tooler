use crate::{commands::ssh::resolve_server, context::Context, db, output::OutputFormat};
use anyhow::{Result, bail};
use clap::{Args, Subcommand};
use colored::Colorize;
use serde::Serialize;

#[derive(Args)]
pub struct PsArgs {
    #[command(subcommand)]
    pub subcommand: PsSubcommand,
}

#[derive(Subcommand)]
pub enum PsSubcommand {
    /// List running processes on a remote server (ps aux)
    List {
        /// Server profile (see: tooler server list)
        server: String,
        /// Only show processes whose command line (or PID) matches this substring
        #[arg(long)]
        filter: Option<String>,
    },
    /// Send a signal to a process on a remote server. Preview-only unless --confirm is passed
    Kill {
        server: String,
        /// Process ID to signal
        pid: u32,
        /// Signal name or number
        #[arg(long, default_value = "TERM")]
        signal: String,
        /// Run via sudo
        #[arg(long)]
        sudo: bool,
        /// Sudo password [env: TOOLER_SUDO_PASS] (only used with --sudo; omit to rely on NOPASSWD)
        #[arg(long, env = "TOOLER_SUDO_PASS")]
        sudo_pass: Option<String>,
        /// Actually send the signal (default is preview-only)
        #[arg(long)]
        confirm: bool,
    },
}

pub fn run(args: PsArgs, ctx: &Context) -> Result<()> {
    match args.subcommand {
        PsSubcommand::List { server, filter } => list(&server, filter.as_deref(), ctx),
        PsSubcommand::Kill {
            server,
            pid,
            signal,
            sudo,
            sudo_pass,
            confirm,
        } => kill(
            &server,
            pid,
            &signal,
            sudo,
            sudo_pass.as_deref(),
            confirm,
            ctx,
        ),
    }
}

fn fail(json: bool, message: String) -> Result<()> {
    if json {
        println!("{}", serde_json::json!({ "error": message }));
        std::process::exit(1);
    }
    bail!(message);
}

#[derive(Serialize)]
struct ProcessRow {
    user: String,
    pid: u32,
    cpu: String,
    mem: String,
    stat: String,
    start: String,
    time: String,
    command: String,
}

/// Parses `ps aux` output (USER PID %CPU %MEM VSZ RSS TTY STAT START TIME COMMAND --
/// the same 11-column layout on both Linux/procps and BSD/FreeBSD's `ps aux`), skipping
/// the header line and anything too short to be a real process row.
fn parse_ps_aux(output: &str) -> Vec<ProcessRow> {
    output
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter(|l| !l.trim_start().starts_with("USER"))
        .filter_map(parse_ps_line)
        .collect()
}

fn parse_ps_line(line: &str) -> Option<ProcessRow> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.len() < 11 {
        return None;
    }
    let pid: u32 = fields[1].parse().ok()?;
    Some(ProcessRow {
        user: fields[0].to_string(),
        pid,
        cpu: fields[2].to_string(),
        mem: fields[3].to_string(),
        stat: fields[7].to_string(),
        start: fields[8].to_string(),
        time: fields[9].to_string(),
        command: fields[10..].join(" "),
    })
}

/// Keeps rows whose command line contains `filter` (case-insensitive) or whose PID
/// matches it exactly.
fn apply_filter(rows: Vec<ProcessRow>, filter: Option<&str>) -> Vec<ProcessRow> {
    let Some(f) = filter else {
        return rows;
    };
    let f_lower = f.to_lowercase();
    rows.into_iter()
        .filter(|r| r.command.to_lowercase().contains(&f_lower) || r.pid.to_string() == f)
        .collect()
}

fn kill_cmd(pid: u32, signal: &str, sudo: bool, sudo_pass: Option<&str>) -> String {
    format!(
        "{}kill {} {pid}",
        db::sudo_prefix(sudo, sudo_pass),
        db::shell_quote(&format!("-{signal}"))
    )
}

fn list(server_name: &str, filter: Option<&str>, ctx: &Context) -> Result<()> {
    let json = ctx.output == OutputFormat::Json;

    let server = match resolve_server(ctx, server_name) {
        Ok(s) => s,
        Err(e) => return fail(json, format!("{e:#}")),
    };
    let output = match db::ssh_exec_capture(&server, "ps aux") {
        Ok(o) => o,
        Err(e) => return fail(json, format!("{e:#}")),
    };
    let rows = apply_filter(parse_ps_aux(&output), filter);

    if json {
        println!(
            "{}",
            serde_json::json!({"server": server_name, "processes": rows})
        );
        return Ok(());
    }

    if rows.is_empty() {
        println!("{}", "(no matching processes)".dimmed());
        return Ok(());
    }
    println!("{} {}", "processes on".bold(), server_name.cyan());
    println!("{}", "─".repeat(70).dimmed());
    for r in &rows {
        println!(
            "{:>7}  {:>5}  {:>5}  {:<6} {}",
            r.pid,
            format!("{}%", r.cpu).yellow(),
            format!("{}%", r.mem).yellow(),
            r.stat,
            r.command
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn kill(
    server_name: &str,
    pid: u32,
    signal: &str,
    sudo: bool,
    sudo_pass: Option<&str>,
    confirm: bool,
    ctx: &Context,
) -> Result<()> {
    let json = ctx.output == OutputFormat::Json;

    let server = match resolve_server(ctx, server_name) {
        Ok(s) => s,
        Err(e) => return fail(json, format!("{e:#}")),
    };

    if !confirm {
        if json {
            println!(
                "{}",
                serde_json::json!({
                    "server": server_name,
                    "pid": pid,
                    "signal": signal,
                    "confirmed": false,
                })
            );
            return Ok(());
        }
        println!(
            "Would send SIG{} to pid {} on {}. Re-run with --confirm to apply.",
            signal.bold(),
            pid,
            server_name.cyan()
        );
        return Ok(());
    }

    let command = kill_cmd(pid, signal, sudo, sudo_pass);
    if let Err(e) = db::ssh_exec_capture(&server, &command) {
        return fail(json, format!("{e:#}"));
    }

    if json {
        println!(
            "{}",
            serde_json::json!({"server": server_name, "pid": pid, "signal": signal, "sent": true})
        );
        return Ok(());
    }
    println!(
        "{} sent SIG{} to pid {} on {}",
        "✓".green().bold(),
        signal.bold(),
        pid,
        server_name.cyan()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
USER       PID  %CPU %MEM      VSZ    RSS TTY      STAT START   TIME COMMAND
ventura942 501   0.1  0.3    12345   6789 ?        Ss   09:00   0:01 /usr/bin/ssh-agent
ventura942 812   0.0  0.1     4321   1234 ?        S    09:01   0:00 -bash keepalive_axum.sh
ventura942 913  12.5  4.2   987654 321000 ?        Sl   09:01   3:22 ./server --port 3010
";

    #[test]
    fn parse_ps_aux_skips_header_and_parses_rows() {
        let rows = parse_ps_aux(SAMPLE);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].pid, 501);
        assert_eq!(rows[0].user, "ventura942");
        assert_eq!(rows[0].command, "/usr/bin/ssh-agent");
        assert_eq!(rows[2].command, "./server --port 3010");
        assert_eq!(rows[2].cpu, "12.5");
    }

    #[test]
    fn parse_ps_aux_ignores_short_or_blank_lines() {
        let rows = parse_ps_aux("\nUSER PID\nnot enough fields\n");
        assert!(rows.is_empty());
    }

    #[test]
    fn apply_filter_matches_command_substring_case_insensitively() {
        let rows = parse_ps_aux(SAMPLE);
        let filtered = apply_filter(rows, Some("KEEPALIVE"));
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].pid, 812);
    }

    #[test]
    fn apply_filter_matches_exact_pid() {
        let rows = parse_ps_aux(SAMPLE);
        let filtered = apply_filter(rows, Some("913"));
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].pid, 913);
    }

    #[test]
    fn apply_filter_none_returns_all() {
        let rows = parse_ps_aux(SAMPLE);
        assert_eq!(apply_filter(rows, None).len(), 3);
    }

    #[test]
    fn kill_cmd_without_sudo() {
        assert_eq!(kill_cmd(1234, "TERM", false, None), "kill '-TERM' 1234");
    }

    #[test]
    fn kill_cmd_with_sudo_and_password() {
        assert_eq!(
            kill_cmd(1234, "9", true, Some("pw")),
            "echo 'pw' | sudo -S kill '-9' 1234"
        );
    }
}
