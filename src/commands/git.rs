use crate::{context::Context, output::OutputFormat};
use anyhow::{Result, bail};
use clap::{Args, Subcommand};
use colored::Colorize;

#[derive(Args)]
pub struct GitArgs {
    #[command(subcommand)]
    pub subcommand: GitSubcommand,
}

#[derive(Subcommand)]
pub enum GitSubcommand {
    /// Compact repo summary: branch, tag, status, recent commits
    Summary,

    /// Delete branches already merged into the current branch
    Clean {
        /// Also delete from remote
        #[arg(long)]
        remote: bool,
        /// Actually delete (default is preview)
        #[arg(long)]
        confirm: bool,
    },

    /// Generate a changelog from commits since the last tag
    Changelog {
        /// Starting tag or commit (defaults to latest tag)
        #[arg(long)]
        from: Option<String>,
    },
}

fn git(args: &[&str]) -> Result<String> {
    let out = std::process::Command::new("git").args(args).output()?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub fn run(args: GitArgs, ctx: &Context) -> Result<()> {
    match args.subcommand {
        GitSubcommand::Summary => summary(ctx),
        GitSubcommand::Clean { remote, confirm } => clean(remote, confirm, ctx),
        GitSubcommand::Changelog { from } => changelog(from, ctx),
    }
}

fn summary(ctx: &Context) -> Result<()> {
    let branch = git(&["branch", "--show-current"])?;
    let status = git(&["status", "--short"])?;
    let last_tag = git(&["describe", "--tags", "--abbrev=0"]).ok();
    let log = git(&["log", "--oneline", "-5"])?;

    let ahead_behind: Option<(u32, u32)> =
        git(&["rev-list", "--left-right", "--count", "HEAD...@{u}"])
            .ok()
            .and_then(|s| {
                let parts: Vec<&str> = s.split_whitespace().collect();
                if parts.len() == 2 {
                    Some((parts[0].parse().unwrap_or(0), parts[1].parse().unwrap_or(0)))
                } else {
                    None
                }
            });

    if ctx.output == OutputFormat::Json {
        let (ahead, behind) = ahead_behind.unwrap_or((0, 0));
        println!(
            "{}",
            serde_json::json!({
                "branch": branch,
                "tag": last_tag,
                "ahead": ahead,
                "behind": behind,
                "clean": status.is_empty(),
                "status": status.lines().collect::<Vec<_>>(),
                "recent": log.lines().collect::<Vec<_>>(),
            })
        );
        return Ok(());
    }

    let last_tag = last_tag.unwrap_or_else(|| "—".to_string());
    let ahead_behind = ahead_behind.map(|(a, b)| format!("↑{a} ↓{b}"));

    println!("{}", "git summary".bold().cyan());
    println!("{}", "─".repeat(40).dimmed());
    print!("{} {}", "branch:".bold(), branch.green());
    if let Some(ab) = ahead_behind {
        print!("  {}", ab.dimmed());
    }
    println!();
    println!("{} {}", "tag:   ".bold(), last_tag.yellow());

    if status.is_empty() {
        println!("{} {}", "status:".bold(), "clean".green());
    } else {
        println!("{}", "status:".bold());
        for line in status.lines() {
            println!("  {}", line);
        }
    }

    if !log.is_empty() {
        println!("{}", "recent:".bold());
        for line in log.lines() {
            println!("  {}", line.dimmed());
        }
    }

    Ok(())
}

/// Outcome of deleting a single merged branch, shared by both the JSON and
/// plain-text renderers below so they can never diverge on what actually happened.
struct DeleteOutcome<'a> {
    branch: &'a str,
    local_error: Option<String>,
    /// `None` if `--remote` wasn't requested or the local delete failed first.
    remote_deleted: Option<bool>,
}

/// Deletes each branch locally (and from `origin` if `remote`), continuing past
/// failures so every branch gets a reported outcome instead of aborting partway
/// through and silently dropping the branches after the first failure.
fn delete_branches<'a>(to_delete: &[&'a str], remote: bool) -> Vec<DeleteOutcome<'a>> {
    to_delete
        .iter()
        .map(|&branch| {
            let local = git(&["branch", "-d", branch]);
            let remote_deleted = if remote && local.is_ok() {
                Some(git(&["push", "origin", "--delete", branch]).is_ok())
            } else {
                None
            };
            DeleteOutcome {
                branch,
                local_error: local.err().map(|e| e.to_string()),
                remote_deleted,
            }
        })
        .collect()
}

fn clean(remote: bool, confirm: bool, ctx: &Context) -> Result<()> {
    let current = git(&["branch", "--show-current"])?;
    let protected = ["main", "master", "develop", "dev", current.as_str()];

    let merged = git(&["branch", "--merged"])?;
    let to_delete: Vec<&str> = merged
        .lines()
        .map(|l| l.trim().trim_start_matches("* "))
        .filter(|b| !b.is_empty() && !protected.contains(b))
        .collect();

    if ctx.output == OutputFormat::Json {
        if to_delete.is_empty() {
            println!("{}", serde_json::json!({"branches": [], "deleted": false}));
            return Ok(());
        }
        if !confirm {
            println!(
                "{}",
                serde_json::json!({"branches": to_delete, "deleted": false})
            );
            return Ok(());
        }
        let results: Vec<_> = delete_branches(&to_delete, remote)
            .into_iter()
            .map(|o| {
                serde_json::json!({
                    "branch": o.branch,
                    "deleted": o.local_error.is_none(),
                    "error": o.local_error,
                    "remote_deleted": o.remote_deleted,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::json!({"branches": to_delete, "deleted": true, "results": results})
        );
        return Ok(());
    }

    if to_delete.is_empty() {
        println!("{}", "No merged branches to delete.".green());
        return Ok(());
    }

    println!("{}", "Merged branches to delete:".bold());
    for b in &to_delete {
        println!("  {} {}", "−".red(), b);
    }

    if !confirm {
        println!("\n{}", "Run with --confirm to actually delete.".dimmed());
        return Ok(());
    }

    for outcome in delete_branches(&to_delete, remote) {
        match outcome.local_error {
            None => println!("{} deleted {}", "✓".green().bold(), outcome.branch),
            Some(e) => {
                println!("{} {}: {}", "!".red().bold(), outcome.branch, e);
                continue;
            }
        }
        match outcome.remote_deleted {
            Some(true) => println!("{} deleted origin/{}", "✓".green().bold(), outcome.branch),
            Some(false) => println!(
                "{} origin/{}: failed to delete",
                "!".yellow().bold(),
                outcome.branch
            ),
            None => {}
        }
    }
    Ok(())
}

fn changelog(from: Option<String>, ctx: &Context) -> Result<()> {
    let from_ref = match from {
        Some(f) => f,
        None => git(&["describe", "--tags", "--abbrev=0"])
            .unwrap_or_else(|_| git(&["rev-list", "--max-parents=0", "HEAD"]).unwrap_or_default()),
    };

    let range = if from_ref.is_empty() {
        "HEAD".to_string()
    } else {
        format!("{from_ref}..HEAD")
    };

    let log = git(&["log", &range, "--oneline", "--no-merges"])?;

    if log.is_empty() {
        if ctx.output == OutputFormat::Json {
            println!(
                "{}",
                serde_json::json!({"features": [], "fixes": [], "other": []})
            );
            return Ok(());
        }
        println!("{}", "No commits since last tag.".dimmed());
        return Ok(());
    }

    let mut feat: Vec<&str> = vec![];
    let mut fix: Vec<&str> = vec![];
    let mut other: Vec<&str> = vec![];

    for line in log.lines() {
        let msg = line.split_once(' ').map(|x| x.1).unwrap_or(line);
        if msg.starts_with("feat") {
            feat.push(msg);
        } else if msg.starts_with("fix") {
            fix.push(msg);
        } else {
            other.push(msg);
        }
    }

    if ctx.output == OutputFormat::Json {
        println!(
            "{}",
            serde_json::json!({"features": feat, "fixes": fix, "other": other})
        );
        return Ok(());
    }

    println!("## Changelog\n");
    if !feat.is_empty() {
        println!("### Features");
        for m in &feat {
            println!("- {m}");
        }
        println!();
    }
    if !fix.is_empty() {
        println!("### Bug Fixes");
        for m in &fix {
            println!("- {m}");
        }
        println!();
    }
    if !other.is_empty() {
        println!("### Other");
        for m in &other {
            println!("- {m}");
        }
    }
    Ok(())
}
