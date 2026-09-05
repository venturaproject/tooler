use crate::{context::Context, output::OutputFormat};
use anyhow::{Context as _, Result, bail};
use chrono::{DateTime, NaiveDate, Utc};
use clap::{Args, Subcommand};
use colored::Colorize;
use serde_json::Value;
use std::path::Path;

#[derive(Args)]
pub struct GhArgs {
    #[command(subcommand)]
    pub subcommand: GhSubcommand,
}

#[derive(Subcommand)]
pub enum GhSubcommand {
    /// List pull requests (title, labels, dates) via the `gh` CLI, optionally
    /// filtered to a created-date range. JSON output feeds straight into
    /// `tooler report pdf`/`excel --in prs=-`
    Prs {
        /// Repository as owner/name (defaults to the repo in the current directory)
        #[arg(long)]
        repo: Option<String>,
        /// Only PRs created on/after this date (YYYY-MM-DD)
        #[arg(long)]
        after: Option<String>,
        /// Only PRs created on/before this date (YYYY-MM-DD)
        #[arg(long)]
        before: Option<String>,
        /// PR state to include
        #[arg(long, default_value = "all")]
        state: String,
        /// Max PRs to fetch from GitHub before date filtering
        #[arg(long, default_value_t = 500)]
        limit: u32,
    },
}

pub fn run(args: GhArgs, ctx: &Context) -> Result<()> {
    match args.subcommand {
        GhSubcommand::Prs {
            repo,
            after,
            before,
            state,
            limit,
        } => prs(repo, after, before, state, limit, ctx),
    }
}

fn fail(json: bool, message: String) -> Result<()> {
    if json {
        println!("{}", serde_json::json!({ "error": message }));
        std::process::exit(1);
    }
    bail!(message);
}

pub(crate) fn parse_date(s: &str) -> Result<NaiveDate> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .map_err(|_| anyhow::anyhow!("expected a date in YYYY-MM-DD format, got {s:?}"))
}

fn created_date(pr: &Value) -> Option<NaiveDate> {
    let s = pr.get("createdAt")?.as_str()?;
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc).date_naive())
}

/// GitHub calls them "labels"; flattened to a single comma-separated string
/// so they render as one table/spreadsheet cell instead of nested JSON.
fn label_names(pr: &Value) -> String {
    pr.get("labels")
        .and_then(Value::as_array)
        .map(|labels| {
            labels
                .iter()
                .filter_map(|l| l.get("name").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}

fn author_login(pr: &Value) -> String {
    pr.get("author")
        .and_then(|a| a.get("login"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Flattens one `gh pr list --json ...` record into scalar fields only, so
/// it matches what `tooler report`'s table extraction expects.
fn flatten(pr: &Value) -> Value {
    serde_json::json!({
        "number": pr.get("number").cloned().unwrap_or(Value::Null),
        "title": pr.get("title").and_then(Value::as_str).unwrap_or_default(),
        "state": pr.get("state").and_then(Value::as_str).unwrap_or_default(),
        "author": author_login(pr),
        "labels": label_names(pr),
        "created_at": pr.get("createdAt").and_then(Value::as_str).unwrap_or_default(),
        "merged_at": pr.get("mergedAt").and_then(Value::as_str),
        "url": pr.get("url").and_then(Value::as_str).unwrap_or_default(),
    })
}

/// Pure fetch+filter+flatten core behind `tooler gh prs` / the `gh_prs:` playbook task.
/// `dir` is `None` for the CLI (process's own cwd, so `gh` infers the repo the same way
/// it always has) or `Some(playbook_dir)` for `gh_prs:` (same convention `git_in` uses).
pub(crate) fn fetch_prs(
    dir: Option<&Path>,
    repo: Option<&str>,
    after: Option<NaiveDate>,
    before: Option<NaiveDate>,
    state: &str,
    limit: u32,
) -> Result<Vec<Value>> {
    let mut version_cmd = std::process::Command::new("gh");
    version_cmd.arg("--version");
    if let Some(d) = dir {
        version_cmd.current_dir(d);
    }
    if version_cmd.output().is_err() {
        bail!("gh CLI not found -- install from https://cli.github.com and run `gh auth login`");
    }

    let mut cmd_args = vec![
        "pr".to_string(),
        "list".to_string(),
        "--state".to_string(),
        state.to_string(),
        "--json".to_string(),
        "number,title,state,author,labels,createdAt,mergedAt,url".to_string(),
        "--limit".to_string(),
        limit.to_string(),
    ];
    if let Some(r) = repo {
        cmd_args.push("--repo".to_string());
        cmd_args.push(r.to_string());
    }

    let mut cmd = std::process::Command::new("gh");
    cmd.args(&cmd_args);
    if let Some(d) = dir {
        cmd.current_dir(d);
    }
    let out = cmd.output().context("running gh pr list")?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let all: Vec<Value> =
        serde_json::from_slice(&out.stdout).context("parsing gh pr list output")?;

    Ok(all
        .iter()
        .filter(|pr| match created_date(pr) {
            Some(d) => after.is_none_or(|a| d >= a) && before.is_none_or(|b| d <= b),
            None => after.is_none() && before.is_none(),
        })
        .map(flatten)
        .collect())
}

fn prs(
    repo: Option<String>,
    after: Option<String>,
    before: Option<String>,
    state: String,
    limit: u32,
    ctx: &Context,
) -> Result<()> {
    let json = ctx.output == OutputFormat::Json;

    let after = match after.as_deref().map(parse_date).transpose() {
        Ok(d) => d,
        Err(e) => return fail(json, format!("{e:#}")),
    };
    let before = match before.as_deref().map(parse_date).transpose() {
        Ok(d) => d,
        Err(e) => return fail(json, format!("{e:#}")),
    };

    let prs = match fetch_prs(None, repo.as_deref(), after, before, &state, limit) {
        Ok(p) => p,
        Err(e) => return fail(json, format!("{e:#}")),
    };

    if json {
        println!("{}", serde_json::json!({"pull_requests": prs}));
        return Ok(());
    }

    if prs.is_empty() {
        println!("{}", "No pull requests match.".dimmed());
        return Ok(());
    }

    println!(
        "{}",
        format!("{} pull request(s):", prs.len()).bold().cyan()
    );
    println!("{}", "─".repeat(60).dimmed());
    for pr in &prs {
        let number = pr.get("number").map(|n| n.to_string()).unwrap_or_default();
        let title = pr.get("title").and_then(Value::as_str).unwrap_or_default();
        let labels = pr.get("labels").and_then(Value::as_str).unwrap_or_default();
        let created = pr
            .get("created_at")
            .and_then(Value::as_str)
            .unwrap_or_default();
        println!("  {} {}", format!("#{number}").yellow(), title.bold());
        if !labels.is_empty() {
            println!("      {} {}", "labels:".dimmed(), labels);
        }
        println!("      {} {}", "created:".dimmed(), created);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_date_accepts_iso_format() {
        assert_eq!(
            parse_date("2026-07-08").unwrap(),
            NaiveDate::from_ymd_opt(2026, 7, 8).unwrap()
        );
    }

    #[test]
    fn parse_date_rejects_other_formats() {
        assert!(parse_date("08/07/2026").is_err());
        assert!(parse_date("not-a-date").is_err());
    }

    #[test]
    fn created_date_parses_rfc3339_timestamp() {
        let pr = json!({"createdAt": "2026-07-08T10:15:00Z"});
        assert_eq!(
            created_date(&pr),
            Some(NaiveDate::from_ymd_opt(2026, 7, 8).unwrap())
        );
    }

    #[test]
    fn created_date_none_when_missing_or_malformed() {
        assert_eq!(created_date(&json!({})), None);
        assert_eq!(created_date(&json!({"createdAt": "not-a-date"})), None);
    }

    #[test]
    fn label_names_joins_multiple_labels() {
        let pr = json!({"labels": [{"name": "bug"}, {"name": "priority-high"}]});
        assert_eq!(label_names(&pr), "bug, priority-high");
    }

    #[test]
    fn label_names_empty_when_no_labels() {
        assert_eq!(label_names(&json!({})), "");
        assert_eq!(label_names(&json!({"labels": []})), "");
    }

    #[test]
    fn author_login_extracts_login_field() {
        let pr = json!({"author": {"login": "ventura", "id": "123"}});
        assert_eq!(author_login(&pr), "ventura");
    }

    #[test]
    fn flatten_produces_scalar_fields_only() {
        let pr = json!({
            "number": 42,
            "title": "Add feature",
            "state": "MERGED",
            "author": {"login": "ventura"},
            "labels": [{"name": "feature"}],
            "createdAt": "2026-07-08T10:15:00Z",
            "mergedAt": "2026-07-09T10:15:00Z",
            "url": "https://github.com/x/y/pull/42",
        });
        let flat = flatten(&pr);
        assert_eq!(flat["number"], 42);
        assert_eq!(flat["title"], "Add feature");
        assert_eq!(flat["labels"], "feature");
        assert_eq!(flat["author"], "ventura");
        assert!(flat["labels"].is_string());
    }
}
