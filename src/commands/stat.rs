use crate::{commands::ssh::resolve_server, context::Context, db, output::OutputFormat};
use anyhow::{Result, bail};
use clap::Args;
use colored::Colorize;

#[derive(Args)]
pub struct StatArgs {
    /// Server profile (see: tooler server list)
    pub server: String,
}

pub fn run(args: StatArgs, ctx: &Context) -> Result<()> {
    stat(&args.server, ctx)
}

fn fail(json: bool, message: String) -> Result<()> {
    if json {
        println!("{}", serde_json::json!({ "error": message }));
        std::process::exit(1);
    }
    bail!(message);
}

const MARK_UPTIME: &str = "__TOOLER_STAT_UPTIME__";
const MARK_MEM: &str = "__TOOLER_STAT_MEM__";
const MARK_DISK: &str = "__TOOLER_STAT_DISK__";

fn stat_cmd() -> String {
    format!(
        "echo {MARK_UPTIME}; uptime; echo {MARK_MEM}; \
         (free -h 2>/dev/null || vm_stat 2>/dev/null || echo 'unavailable'); \
         echo {MARK_DISK}; df -h"
    )
}

/// Splits the combined `stat_cmd` output into (uptime, memory, disk) blocks using the
/// echo'd markers. Missing markers degrade to empty sections rather than panicking.
fn parse_sections(output: &str) -> (String, String, String) {
    let after_uptime = output.split(MARK_UPTIME).nth(1).unwrap_or("");
    let (uptime, rest) = after_uptime
        .split_once(MARK_MEM)
        .unwrap_or((after_uptime, ""));
    let (mem, disk) = rest.split_once(MARK_DISK).unwrap_or((rest, ""));
    (
        uptime.trim().to_string(),
        mem.trim().to_string(),
        disk.trim().to_string(),
    )
}

fn stat(server_name: &str, ctx: &Context) -> Result<()> {
    let json = ctx.output == OutputFormat::Json;

    let server = match resolve_server(ctx, server_name) {
        Ok(s) => s,
        Err(e) => return fail(json, format!("{e:#}")),
    };

    let output = match db::ssh_exec_capture(&server, &stat_cmd()) {
        Ok(o) => o,
        Err(e) => return fail(json, format!("{e:#}")),
    };

    let (uptime, memory, disk) = parse_sections(&output);

    if json {
        println!(
            "{}",
            serde_json::json!({
                "server": server_name,
                "uptime": uptime,
                "memory": memory,
                "disk": disk,
            })
        );
        return Ok(());
    }

    println!("{} {}", "stat on".bold(), server_name.cyan());
    println!("{}", "─".repeat(50).dimmed());
    println!("{}", "uptime / load".yellow().bold());
    println!("{uptime}\n");
    println!("{}", "memory".yellow().bold());
    println!("{memory}\n");
    println!("{}", "disk".yellow().bold());
    println!("{disk}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_cmd_contains_all_three_markers() {
        let cmd = stat_cmd();
        assert!(cmd.contains(MARK_UPTIME));
        assert!(cmd.contains(MARK_MEM));
        assert!(cmd.contains(MARK_DISK));
    }

    #[test]
    fn parse_sections_splits_three_blocks() {
        let output = format!(
            "{MARK_UPTIME}\n load average: 0.10 \n{MARK_MEM}\n Mem: 512M used \n{MARK_DISK}\n /dev/sda1 50%\n"
        );
        let (uptime, mem, disk) = parse_sections(&output);
        assert_eq!(uptime, "load average: 0.10");
        assert_eq!(mem, "Mem: 512M used");
        assert_eq!(disk, "/dev/sda1 50%");
    }

    #[test]
    fn parse_sections_handles_missing_markers_gracefully() {
        let (uptime, mem, disk) = parse_sections("");
        assert_eq!(uptime, "");
        assert_eq!(mem, "");
        assert_eq!(disk, "");
    }
}
