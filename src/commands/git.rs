use crate::{context::Context, output::OutputFormat};
use anyhow::{Result, bail};
use chrono::NaiveDate;
use clap::{Args, Subcommand};
use colored::Colorize;
use serde::Serialize;
use std::path::Path;

#[derive(Args)]
pub struct GitArgs {
    #[command(subcommand)]
    pub subcommand: GitSubcommand,
}

#[derive(Subcommand)]
pub enum GitSubcommand {
    /// Compact repo summary: branch, tag, status, recent commits
    Summary,

    /// Delete branches already merged into the current branch, or (with
    /// --after/--before) any local branch whose name ends in a DDMMYY date
    /// suffix falling in the given range, regardless of merge status
    Clean {
        /// Also delete from remote
        #[arg(long)]
        remote: bool,
        /// Actually delete (default is preview)
        #[arg(long)]
        confirm: bool,
        /// Only branches with a trailing DDMMYY date suffix on/after this date (DDMMYY)
        #[arg(long)]
        after: Option<String>,
        /// Only branches with a trailing DDMMYY date suffix on/before this date (DDMMYY)
        #[arg(long)]
        before: Option<String>,
    },

    /// Generate a changelog from commits since the last tag
    Changelog {
        /// Starting tag or commit (defaults to latest tag)
        #[arg(long)]
        from: Option<String>,
    },
}

fn git(args: &[&str]) -> Result<String> {
    git_in(None, args)
}

/// Like `git()`, but runs in `dir` when given instead of the process's own cwd — used
/// by the `git_summary:`/`git_changelog:` playbook tasks so they operate on the
/// playbook's own repo (same convention `run:` already has via `current_dir`), not
/// wherever `tooler` itself happened to be invoked from.
pub(crate) fn git_in(dir: Option<&Path>, args: &[&str]) -> Result<String> {
    let mut cmd = std::process::Command::new("git");
    cmd.args(args);
    if let Some(d) = dir {
        cmd.current_dir(d);
    }
    let out = cmd.output()?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub fn run(args: GitArgs, ctx: &Context) -> Result<()> {
    match args.subcommand {
        GitSubcommand::Summary => summary(ctx),
        GitSubcommand::Clean {
            remote,
            confirm,
            after,
            before,
        } => clean(remote, confirm, after, before, ctx),
        GitSubcommand::Changelog { from } => changelog(from, ctx),
    }
}

/// Parses a `DDMMYY` string into a date, for the `--after`/`--before` flags.
fn parse_ddmmyy(s: &str) -> Result<NaiveDate> {
    NaiveDate::parse_from_str(s, "%d%m%y")
        .map_err(|_| anyhow::anyhow!("expected a date in DDMMYY format, got {s:?}"))
}

/// If `branch` ends in exactly 6 digits, parses them as a `DDMMYY` date suffix.
/// Works regardless of what separator (`-`, `_`, none) precedes the digits.
fn date_suffix(branch: &str) -> Option<NaiveDate> {
    if branch.len() < 6 {
        return None;
    }
    let tail = &branch[branch.len() - 6..];
    if !tail.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    NaiveDate::parse_from_str(tail, "%d%m%y").ok()
}

/// Pure data behind `tooler git summary` / the `git_summary:` playbook task — a plain
/// struct rather than a print, so both callers can render/serialize it however they
/// need. `dir` is `None` for the CLI (process's own cwd) or `Some(playbook_dir)` for
/// `git_summary:`.
#[derive(Serialize)]
pub(crate) struct GitSummary {
    pub(crate) branch: String,
    pub(crate) tag: Option<String>,
    pub(crate) ahead: u32,
    pub(crate) behind: u32,
    /// Whether `HEAD` has a configured upstream to compare against at all -- distinct
    /// from `ahead`/`behind` both being 0, which also happens with an upstream that's
    /// perfectly in sync. Text-mode only shows the "↑/↓" line when this is true, same
    /// as the original inline implementation this was extracted from.
    #[serde(skip)]
    pub(crate) has_upstream: bool,
    pub(crate) clean: bool,
    pub(crate) status: Vec<String>,
    pub(crate) recent: Vec<String>,
}

pub(crate) fn compute_summary(dir: Option<&Path>) -> Result<GitSummary> {
    let branch = git_in(dir, &["branch", "--show-current"])?;
    let status = git_in(dir, &["status", "--short"])?;
    let tag = git_in(dir, &["describe", "--tags", "--abbrev=0"]).ok();
    let log = git_in(dir, &["log", "--oneline", "-5"])?;

    let ahead_behind: Option<(u32, u32)> =
        git_in(dir, &["rev-list", "--left-right", "--count", "HEAD...@{u}"])
            .ok()
            .and_then(|s| {
                let parts: Vec<&str> = s.split_whitespace().collect();
                if parts.len() == 2 {
                    Some((parts[0].parse().unwrap_or(0), parts[1].parse().unwrap_or(0)))
                } else {
                    None
                }
            });
    let has_upstream = ahead_behind.is_some();
    let (ahead, behind) = ahead_behind.unwrap_or((0, 0));

    Ok(GitSummary {
        branch,
        tag,
        ahead,
        behind,
        has_upstream,
        clean: status.is_empty(),
        status: status.lines().map(String::from).collect(),
        recent: log.lines().map(String::from).collect(),
    })
}

fn summary(ctx: &Context) -> Result<()> {
    let s = compute_summary(None)?;

    if ctx.output == OutputFormat::Json {
        println!("{}", serde_json::to_string(&s)?);
        return Ok(());
    }

    let tag = s.tag.unwrap_or_else(|| "—".to_string());
    let ahead_behind = s
        .has_upstream
        .then(|| format!("↑{} ↓{}", s.ahead, s.behind));

    println!("{}", "git summary".bold().cyan());
    println!("{}", "─".repeat(40).dimmed());
    print!("{} {}", "branch:".bold(), s.branch.green());
    if let Some(ab) = ahead_behind {
        print!("  {}", ab.dimmed());
    }
    println!();
    println!("{} {}", "tag:   ".bold(), tag.yellow());

    if s.clean {
        println!("{} {}", "status:".bold(), "clean".green());
    } else {
        println!("{}", "status:".bold());
        for line in &s.status {
            println!("  {}", line);
        }
    }

    if !s.recent.is_empty() {
        println!("{}", "recent:".bold());
        for line in &s.recent {
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

fn clean(
    remote: bool,
    confirm: bool,
    after: Option<String>,
    before: Option<String>,
    ctx: &Context,
) -> Result<()> {
    let current = git(&["branch", "--show-current"])?;
    let protected = ["main", "master", "develop", "dev", current.as_str()];

    let by_date = after.is_some() || before.is_some();
    let after = after.as_deref().map(parse_ddmmyy).transpose()?;
    let before = before.as_deref().map(parse_ddmmyy).transpose()?;

    let owned_names: Vec<String>;
    let to_delete: Vec<&str> = if by_date {
        // Date-suffix cleanup targets any local branch in range, not just
        // merged ones — the date suffix is the user's own retention signal.
        let all = git(&["branch", "--list"])?;
        owned_names = all
            .lines()
            .map(|l| l.trim().trim_start_matches("* ").to_string())
            .filter(|b| {
                !b.is_empty()
                    && !protected.contains(&b.as_str())
                    && date_suffix(b).is_some_and(|d| {
                        after.is_none_or(|a| d >= a) && before.is_none_or(|bf| d <= bf)
                    })
            })
            .collect();
        owned_names.iter().map(String::as_str).collect()
    } else {
        let merged = git(&["branch", "--merged"])?;
        owned_names = merged
            .lines()
            .map(|l| l.trim().trim_start_matches("* ").to_string())
            .filter(|b| !b.is_empty() && !protected.contains(&b.as_str()))
            .collect();
        owned_names.iter().map(String::as_str).collect()
    };

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
        let msg = if by_date {
            "No branches with a date suffix in range."
        } else {
            "No merged branches to delete."
        };
        println!("{}", msg.green());
        return Ok(());
    }

    let heading = if by_date {
        "Branches in date range to delete:"
    } else {
        "Merged branches to delete:"
    };
    println!("{}", heading.bold());
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

/// Pure data behind `tooler git changelog` / the `git_changelog:` playbook task.
#[derive(Serialize)]
pub(crate) struct Changelog {
    pub(crate) features: Vec<String>,
    pub(crate) fixes: Vec<String>,
    pub(crate) other: Vec<String>,
}

pub(crate) fn compute_changelog(dir: Option<&Path>, from: Option<&str>) -> Result<Changelog> {
    let from_ref = match from {
        Some(f) => f.to_string(),
        None => git_in(dir, &["describe", "--tags", "--abbrev=0"]).unwrap_or_else(|_| {
            git_in(dir, &["rev-list", "--max-parents=0", "HEAD"]).unwrap_or_default()
        }),
    };

    let range = if from_ref.is_empty() {
        "HEAD".to_string()
    } else {
        format!("{from_ref}..HEAD")
    };

    let log = git_in(dir, &["log", &range, "--oneline", "--no-merges"])?;

    let mut features = vec![];
    let mut fixes = vec![];
    let mut other = vec![];

    for line in log.lines() {
        let msg = line
            .split_once(' ')
            .map(|x| x.1)
            .unwrap_or(line)
            .to_string();
        if msg.starts_with("feat") {
            features.push(msg);
        } else if msg.starts_with("fix") {
            fixes.push(msg);
        } else {
            other.push(msg);
        }
    }

    Ok(Changelog {
        features,
        fixes,
        other,
    })
}

fn changelog(from: Option<String>, ctx: &Context) -> Result<()> {
    let c = compute_changelog(None, from.as_deref())?;

    if c.features.is_empty() && c.fixes.is_empty() && c.other.is_empty() {
        if ctx.output == OutputFormat::Json {
            println!("{}", serde_json::to_string(&c)?);
            return Ok(());
        }
        println!("{}", "No commits since last tag.".dimmed());
        return Ok(());
    }

    if ctx.output == OutputFormat::Json {
        println!("{}", serde_json::to_string(&c)?);
        return Ok(());
    }

    println!("## Changelog\n");
    if !c.features.is_empty() {
        println!("### Features");
        for m in &c.features {
            println!("- {m}");
        }
        println!();
    }
    if !c.fixes.is_empty() {
        println!("### Bug Fixes");
        for m in &c.fixes {
            println!("- {m}");
        }
        println!();
    }
    if !c.other.is_empty() {
        println!("### Other");
        for m in &c.other {
            println!("- {m}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_suffix_parses_ddmmyy_with_various_separators() {
        let expected = NaiveDate::from_ymd_opt(2026, 7, 8).unwrap();
        assert_eq!(date_suffix("feature-080726"), Some(expected));
        assert_eq!(date_suffix("feature_080726"), Some(expected));
        assert_eq!(date_suffix("feature080726"), Some(expected));
    }

    #[test]
    fn date_suffix_none_when_no_trailing_digits_or_invalid_date() {
        assert_eq!(date_suffix("feature-login"), None);
        assert_eq!(date_suffix("main"), None);
        // 99 is not a valid month
        assert_eq!(date_suffix("feature-089999"), None);
    }

    #[test]
    fn date_suffix_none_when_branch_shorter_than_six_chars() {
        assert_eq!(date_suffix("dev"), None);
    }

    #[test]
    fn parse_ddmmyy_round_trips_valid_dates() {
        let d = parse_ddmmyy("080726").unwrap();
        assert_eq!(d, NaiveDate::from_ymd_opt(2026, 7, 8).unwrap());
    }

    #[test]
    fn parse_ddmmyy_rejects_malformed_input() {
        assert!(parse_ddmmyy("not-a-date").is_err());
        assert!(parse_ddmmyy("999999").is_err());
    }
}
