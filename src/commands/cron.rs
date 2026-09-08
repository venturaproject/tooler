use crate::{commands::ssh::resolve_server, context::Context, db, output::OutputFormat};
use anyhow::{Result, bail};
use clap::{Args, Subcommand};
use colored::Colorize;
use serde::Serialize;

#[derive(Args)]
pub struct CronArgs {
    #[command(subcommand)]
    pub subcommand: CronSubcommand,
}

#[derive(Subcommand)]
pub enum CronSubcommand {
    /// List a server's crontab entries (crontab -l)
    List {
        /// Server profile (see: tooler server list)
        server: String,
    },
    /// Append a line to a server's crontab
    Add {
        server: String,
        /// Full crontab line, e.g. "0 3 * * * /path/to/backup.sh"
        line: String,
    },
    /// Remove crontab lines containing an exact substring
    Remove {
        server: String,
        /// Fixed substring to match (not a regex) -- matching lines are dropped
        pattern: String,
    },
    /// Manage this machine's own crontab directly (no SSH) -- e.g. to schedule a
    /// recurring `tooler play` run locally
    Local(LocalCronArgs),
}

#[derive(Args)]
pub struct LocalCronArgs {
    #[command(subcommand)]
    pub subcommand: LocalCronSubcommand,
}

#[derive(Subcommand)]
pub enum LocalCronSubcommand {
    /// List this machine's own crontab entries (crontab -l)
    List,
    /// Append a line to this machine's own crontab
    Add {
        /// Full crontab line, e.g. "0 8 * * * /usr/local/bin/tooler play ~/playbooks/x.yml"
        line: String,
    },
    /// Remove crontab lines containing an exact substring
    Remove {
        /// Fixed substring to match (not a regex) -- matching lines are dropped
        pattern: String,
    },
}

pub fn run(args: CronArgs, ctx: &Context) -> Result<()> {
    match args.subcommand {
        CronSubcommand::List { server } => list(&server, ctx),
        CronSubcommand::Add { server, line } => add(&server, &line, ctx),
        CronSubcommand::Remove { server, pattern } => remove(&server, &pattern, ctx),
        CronSubcommand::Local(local) => match local.subcommand {
            LocalCronSubcommand::List => list_local(ctx),
            LocalCronSubcommand::Add { line } => add_local(&line, ctx),
            LocalCronSubcommand::Remove { pattern } => remove_local(&pattern, ctx),
        },
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
struct CronLine {
    line: String,
    schedule: Option<String>,
    command: Option<String>,
}

/// Splits `crontab -l` output into lines, pulling a `schedule`/`command` pair out of
/// entries that look like standard 5-field cron lines. Comments, blank lines, and
/// env-var assignments (e.g. `MAILTO=root`) keep `schedule`/`command` as `None` --
/// they're still returned as raw lines so nothing silently disappears.
fn parse_crontab(output: &str) -> Vec<CronLine> {
    output
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let trimmed = l.trim();
            if trimmed.starts_with('#') {
                return CronLine {
                    line: l.to_string(),
                    schedule: None,
                    command: None,
                };
            }
            let fields: Vec<&str> = trimmed.split_whitespace().collect();
            if fields.len() >= 6 {
                CronLine {
                    line: l.to_string(),
                    schedule: Some(fields[..5].join(" ")),
                    command: Some(fields[5..].join(" ")),
                }
            } else {
                CronLine {
                    line: l.to_string(),
                    schedule: None,
                    command: None,
                }
            }
        })
        .collect()
}

/// `crontab -l` exits 1 with "no crontab for <user>" on stderr when the user has none
/// set up yet -- that's an empty list, not an error worth surfacing.
fn is_no_crontab_error(stderr: &str) -> bool {
    stderr.to_lowercase().contains("no crontab")
}

/// Runs `crontab -l`/`crontab -` directly on this machine, no SSH involved -- the local
/// counterpart to `fetch_crontab`/its `write` sibling below. There's no `crontab` binary
/// on Windows (and no cron daemon to act on it), so both bail there with a clear pointer
/// to the remote path instead of a confusing "program not found".
#[cfg(windows)]
fn fetch_crontab_local() -> Result<String> {
    bail!(
        "local cron is not supported on Windows (no crontab). Use `tooler cron <server>` \
         to schedule on a remote Linux target over SSH instead."
    );
}

#[cfg(not(windows))]
fn fetch_crontab_local() -> Result<String> {
    let output = std::process::Command::new("crontab").arg("-l").output()?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if is_no_crontab_error(&stderr) {
            Ok(String::new())
        } else {
            bail!("{}", stderr.trim())
        }
    }
}

#[cfg(windows)]
fn write_crontab_local(_new_crontab: &str) -> Result<()> {
    bail!(
        "local cron is not supported on Windows (no crontab). Use `tooler cron <server>` \
         to schedule on a remote Linux target over SSH instead."
    );
}

/// Unlike the remote path's `printf %s <shell_quote> | crontab -` (an indirection that
/// exists *because* SSH needs one command string), a local subprocess can just take the
/// new crontab on its own stdin directly -- no shell-quoting footgun to think about here
/// at all.
#[cfg(not(windows))]
fn write_crontab_local(new_crontab: &str) -> Result<()> {
    use std::io::Write;
    use std::process::Stdio;

    let mut child = std::process::Command::new("crontab")
        .arg("-")
        .stdin(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(new_crontab.as_bytes())?;
    let status = child.wait()?;
    if !status.success() {
        bail!("crontab - exited with {status}");
    }
    Ok(())
}

fn list_local(ctx: &Context) -> Result<()> {
    let json = ctx.output == OutputFormat::Json;

    let output = match fetch_crontab_local() {
        Ok(o) => o,
        Err(e) => return fail(json, format!("{e:#}")),
    };
    let entries = parse_crontab(&output);

    if json {
        println!("{}", serde_json::json!({"entries": entries}));
        return Ok(());
    }

    if entries.is_empty() {
        println!("{}", "(no crontab entries)".dimmed());
        return Ok(());
    }
    println!("{}", "local crontab".bold());
    println!("{}", "─".repeat(50).dimmed());
    for entry in &entries {
        match (&entry.schedule, &entry.command) {
            (Some(schedule), Some(command)) => {
                println!("  {}  {}", schedule.yellow(), command);
            }
            _ => println!("  {}", entry.line.dimmed()),
        }
    }
    Ok(())
}

fn add_local(line: &str, ctx: &Context) -> Result<()> {
    let json = ctx.output == OutputFormat::Json;

    let existing = match fetch_crontab_local() {
        Ok(o) => o,
        Err(e) => return fail(json, format!("{e:#}")),
    };
    let mut new_crontab = existing;
    if !new_crontab.is_empty() && !new_crontab.ends_with('\n') {
        new_crontab.push('\n');
    }
    new_crontab.push_str(line.trim_end());
    new_crontab.push('\n');

    if let Err(e) = write_crontab_local(&new_crontab) {
        return fail(json, format!("{e:#}"));
    }

    if json {
        println!("{}", serde_json::json!({"added": line}));
        return Ok(());
    }
    println!(
        "{} added to local crontab: {}",
        "✓".green().bold(),
        line.dimmed()
    );
    Ok(())
}

fn remove_local(pattern: &str, ctx: &Context) -> Result<()> {
    let json = ctx.output == OutputFormat::Json;

    let existing = match fetch_crontab_local() {
        Ok(o) => o,
        Err(e) => return fail(json, format!("{e:#}")),
    };

    let (kept, removed): (Vec<&str>, Vec<&str>) =
        existing.lines().partition(|l| !l.contains(pattern));
    let mut new_crontab = kept.join("\n");
    if !new_crontab.is_empty() {
        new_crontab.push('\n');
    }

    if removed.is_empty() {
        if json {
            println!("{}", serde_json::json!({"removed": Vec::<String>::new()}));
            return Ok(());
        }
        println!("{}", "No crontab lines matched.".dimmed());
        return Ok(());
    }

    if let Err(e) = write_crontab_local(&new_crontab) {
        return fail(json, format!("{e:#}"));
    }

    if json {
        println!("{}", serde_json::json!({"removed": removed}));
        return Ok(());
    }
    println!(
        "{} removed {} line(s) from local crontab:",
        "✓".green().bold(),
        removed.len()
    );
    for line in &removed {
        println!("  {}", line.dimmed());
    }
    Ok(())
}

fn fetch_crontab(server: &crate::config::Server) -> Result<String> {
    let (stdout, stderr, success, _) = db::ssh_exec_capture_lenient(server, "crontab -l")?;
    if success {
        Ok(stdout)
    } else if is_no_crontab_error(&stderr) {
        Ok(String::new())
    } else {
        bail!("{}", stderr.trim())
    }
}

fn list(server_name: &str, ctx: &Context) -> Result<()> {
    let json = ctx.output == OutputFormat::Json;

    let server = match resolve_server(ctx, server_name) {
        Ok(s) => s,
        Err(e) => return fail(json, format!("{e:#}")),
    };
    let output = match fetch_crontab(&server) {
        Ok(o) => o,
        Err(e) => return fail(json, format!("{e:#}")),
    };
    let entries = parse_crontab(&output);

    if json {
        println!(
            "{}",
            serde_json::json!({"server": server_name, "entries": entries})
        );
        return Ok(());
    }

    if entries.is_empty() {
        println!("{}", "(no crontab entries)".dimmed());
        return Ok(());
    }
    println!("{} {}", "crontab on".bold(), server_name.cyan());
    println!("{}", "─".repeat(50).dimmed());
    for entry in &entries {
        match (&entry.schedule, &entry.command) {
            (Some(schedule), Some(command)) => {
                println!("  {}  {}", schedule.yellow(), command);
            }
            _ => println!("  {}", entry.line.dimmed()),
        }
    }
    Ok(())
}

fn add(server_name: &str, line: &str, ctx: &Context) -> Result<()> {
    let json = ctx.output == OutputFormat::Json;

    let server = match resolve_server(ctx, server_name) {
        Ok(s) => s,
        Err(e) => return fail(json, format!("{e:#}")),
    };
    let existing = match fetch_crontab(&server) {
        Ok(o) => o,
        Err(e) => return fail(json, format!("{e:#}")),
    };
    let mut new_crontab = existing;
    if !new_crontab.is_empty() && !new_crontab.ends_with('\n') {
        new_crontab.push('\n');
    }
    new_crontab.push_str(line.trim_end());
    new_crontab.push('\n');

    let command = format!("printf %s {} | crontab -", db::shell_quote(&new_crontab));
    if let Err(e) = db::ssh_exec_capture(&server, &command) {
        return fail(json, format!("{e:#}"));
    }

    if json {
        println!(
            "{}",
            serde_json::json!({"server": server_name, "added": line})
        );
        return Ok(());
    }
    println!(
        "{} added to crontab on {}: {}",
        "✓".green().bold(),
        server_name.cyan(),
        line.dimmed()
    );
    Ok(())
}

fn remove(server_name: &str, pattern: &str, ctx: &Context) -> Result<()> {
    let json = ctx.output == OutputFormat::Json;

    let server = match resolve_server(ctx, server_name) {
        Ok(s) => s,
        Err(e) => return fail(json, format!("{e:#}")),
    };
    let existing = match fetch_crontab(&server) {
        Ok(o) => o,
        Err(e) => return fail(json, format!("{e:#}")),
    };

    let (kept, removed): (Vec<&str>, Vec<&str>) =
        existing.lines().partition(|l| !l.contains(pattern));
    let mut new_crontab = kept.join("\n");
    if !new_crontab.is_empty() {
        new_crontab.push('\n');
    }

    if removed.is_empty() {
        if json {
            println!(
                "{}",
                serde_json::json!({"server": server_name, "removed": Vec::<String>::new()})
            );
            return Ok(());
        }
        println!("{}", "No crontab lines matched.".dimmed());
        return Ok(());
    }

    let command = format!("printf %s {} | crontab -", db::shell_quote(&new_crontab));
    if let Err(e) = db::ssh_exec_capture(&server, &command) {
        return fail(json, format!("{e:#}"));
    }

    if json {
        println!(
            "{}",
            serde_json::json!({"server": server_name, "removed": removed})
        );
        return Ok(());
    }
    println!(
        "{} removed {} line(s) from crontab on {}:",
        "✓".green().bold(),
        removed.len(),
        server_name.cyan()
    );
    for line in &removed {
        println!("  {}", line.dimmed());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_crontab_extracts_schedule_and_command() {
        let out = "0 3 * * * /path/to/backup.sh --flag\n";
        let entries = parse_crontab(out);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].schedule.as_deref(), Some("0 3 * * *"));
        assert_eq!(
            entries[0].command.as_deref(),
            Some("/path/to/backup.sh --flag")
        );
    }

    #[test]
    fn parse_crontab_keeps_comments_and_env_lines_as_raw() {
        let out = "# a comment\nMAILTO=root\n0 3 * * * echo hi\n";
        let entries = parse_crontab(out);
        assert_eq!(entries.len(), 3);
        assert!(entries[0].schedule.is_none());
        assert_eq!(entries[0].line, "# a comment");
        assert!(entries[1].schedule.is_none());
        assert_eq!(entries[1].line, "MAILTO=root");
        assert!(entries[2].schedule.is_some());
    }

    #[test]
    fn parse_crontab_skips_blank_lines() {
        let out = "\n\n0 3 * * * echo hi\n\n";
        assert_eq!(parse_crontab(out).len(), 1);
    }

    #[test]
    fn is_no_crontab_error_matches_common_message() {
        assert!(is_no_crontab_error("no crontab for bob"));
        assert!(is_no_crontab_error("No crontab for bob"));
        assert!(!is_no_crontab_error("permission denied"));
    }

    #[cfg(windows)]
    #[test]
    fn local_crontab_fns_bail_clearly_on_windows() {
        let err = fetch_crontab_local().unwrap_err();
        assert!(err.to_string().contains("not supported on Windows"));
        let err = write_crontab_local("0 3 * * * echo hi\n").unwrap_err();
        assert!(err.to_string().contains("not supported on Windows"));
    }
}
