use crate::{commands::ssh::resolve_server, context::Context, db, output::OutputFormat};
use anyhow::{Result, bail};
use clap::{Args, Subcommand};
use colored::Colorize;

#[derive(Args)]
pub struct LogsArgs {
    #[command(subcommand)]
    pub subcommand: LogsSubcommand,
}

#[derive(Subcommand)]
pub enum LogsSubcommand {
    /// Show the last N lines of a remote file (tail -n)
    Tail {
        /// Server profile (see: tooler server list)
        server: String,
        /// Remote file path
        path: String,
        /// Number of lines
        #[arg(long, default_value_t = 100)]
        lines: u32,
    },
    /// Search a remote file for a fixed substring (grep -F)
    Grep {
        server: String,
        path: String,
        /// Fixed substring to match (not a regex)
        pattern: String,
        /// Cap the number of matching lines returned
        #[arg(long, default_value_t = 200)]
        max_lines: usize,
    },
}

pub fn run(args: LogsArgs, ctx: &Context) -> Result<()> {
    match args.subcommand {
        LogsSubcommand::Tail {
            server,
            path,
            lines,
        } => tail(&server, &path, lines, ctx),
        LogsSubcommand::Grep {
            server,
            path,
            pattern,
            max_lines,
        } => grep(&server, &path, &pattern, max_lines, ctx),
    }
}

fn fail(json: bool, message: String) -> Result<()> {
    if json {
        println!("{}", serde_json::json!({ "error": message }));
        std::process::exit(1);
    }
    bail!(message);
}

pub(crate) fn tail_cmd(path: &str, lines: u32) -> String {
    format!("tail -n {lines} {}", db::shell_quote(path))
}

pub(crate) fn grep_cmd(path: &str, pattern: &str) -> String {
    format!(
        "grep -F -- {} {}",
        db::shell_quote(pattern),
        db::shell_quote(path)
    )
}

pub(crate) fn cap_lines(output: String, max_lines: usize) -> (Vec<String>, bool) {
    let all: Vec<String> = output.lines().map(str::to_string).collect();
    let truncated = all.len() > max_lines;
    (all.into_iter().take(max_lines).collect(), truncated)
}

fn print_lines(lines: &[String]) {
    if lines.is_empty() {
        println!("{}", "(no lines)".dimmed());
        return;
    }
    for line in lines {
        println!("{line}");
    }
}

fn tail(server_name: &str, path: &str, lines_n: u32, ctx: &Context) -> Result<()> {
    let json = ctx.output == OutputFormat::Json;

    let server = match resolve_server(ctx, server_name) {
        Ok(s) => s,
        Err(e) => return fail(json, format!("{e:#}")),
    };
    let output = match db::ssh_exec_capture(&server, &tail_cmd(path, lines_n)) {
        Ok(o) => o,
        Err(e) => return fail(json, format!("{e:#}")),
    };
    let lines: Vec<String> = output.lines().map(str::to_string).collect();

    if json {
        println!(
            "{}",
            serde_json::json!({"server": server_name, "path": path, "lines": lines})
        );
        return Ok(());
    }
    println!(
        "{} {} on {}",
        "tail:".bold().cyan(),
        path.bold(),
        server_name.cyan()
    );
    println!("{}", "─".repeat(50).dimmed());
    print_lines(&lines);
    Ok(())
}

fn grep(
    server_name: &str,
    path: &str,
    pattern: &str,
    max_lines: usize,
    ctx: &Context,
) -> Result<()> {
    let json = ctx.output == OutputFormat::Json;

    let server = match resolve_server(ctx, server_name) {
        Ok(s) => s,
        Err(e) => return fail(json, format!("{e:#}")),
    };
    // grep exits 1 (not an error) when nothing matches -- ssh_exec_capture_lenient lets
    // us tell that apart from a real failure (e.g. file not found, exit >1).
    let (stdout, stderr, success) =
        match db::ssh_exec_capture_lenient(&server, &grep_cmd(path, pattern)) {
            Ok(r) => r,
            Err(e) => return fail(json, format!("{e:#}")),
        };
    let output = if success {
        stdout
    } else if stderr.trim().is_empty() {
        String::new()
    } else {
        return fail(json, stderr.trim().to_string());
    };
    let (lines, truncated) = cap_lines(output, max_lines);

    if json {
        println!(
            "{}",
            serde_json::json!({
                "server": server_name,
                "path": path,
                "pattern": pattern,
                "lines": lines,
                "truncated": truncated,
            })
        );
        return Ok(());
    }
    let suffix = if truncated { ", truncated" } else { "" };
    println!(
        "{} '{}' in {} on {} ({} match(es){suffix})",
        "grep:".bold().cyan(),
        pattern,
        path.bold(),
        server_name.cyan(),
        lines.len()
    );
    println!("{}", "─".repeat(50).dimmed());
    print_lines(&lines);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_cmd_quotes_path() {
        assert_eq!(
            tail_cmd("/var/log/app.log", 50),
            "tail -n 50 '/var/log/app.log'"
        );
    }

    #[test]
    fn grep_cmd_quotes_pattern_and_path() {
        assert_eq!(
            grep_cmd("/var/log/app.log", "ERROR"),
            "grep -F -- 'ERROR' '/var/log/app.log'"
        );
    }

    #[test]
    fn grep_cmd_handles_pattern_with_single_quote() {
        assert_eq!(
            grep_cmd("/var/log/app.log", "it's broken"),
            "grep -F -- 'it'\\''s broken' '/var/log/app.log'"
        );
    }

    #[test]
    fn cap_lines_truncates_and_flags() {
        let output = "a\nb\nc\nd\n".to_string();
        let (lines, truncated) = cap_lines(output, 2);
        assert_eq!(lines, vec!["a", "b"]);
        assert!(truncated);
    }

    #[test]
    fn cap_lines_no_truncation_when_within_limit() {
        let output = "a\nb\n".to_string();
        let (lines, truncated) = cap_lines(output, 5);
        assert_eq!(lines, vec!["a", "b"]);
        assert!(!truncated);
    }
}
