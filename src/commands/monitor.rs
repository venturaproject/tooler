use crate::{commands::check, context::Context, output::OutputFormat};
use anyhow::{Context as _, Result, bail};
use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

#[derive(Args)]
pub struct MonitorArgs {
    #[command(subcommand)]
    pub subcommand: MonitorSubcommand,
}

#[derive(Subcommand)]
pub enum MonitorSubcommand {
    /// Run a monitor definition once. Schedule this command with cron for continuous checks.
    Run {
        /// YAML monitor definition
        file: PathBuf,
        /// Override the durable state file (default: <file>.monitor.json)
        #[arg(long)]
        state: Option<PathBuf>,
        /// Parse and show the checks without probing or updating state
        #[arg(long)]
        dry: bool,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MonitorFile {
    name: String,
    #[serde(default)]
    webhook: Option<Webhook>,
    checks: Vec<CheckSpec>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Webhook {
    url: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckSpec {
    id: String,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    host: Option<String>,
    #[serde(default)]
    port: Option<u16>,
    #[serde(default)]
    timeout: Option<u64>,
    #[serde(default)]
    expected_status: Option<u16>,
}

#[derive(Default, Serialize, Deserialize)]
struct MonitorState {
    version: u8,
    checks: HashMap<String, CheckState>,
    #[serde(default)]
    pending_notifications: Vec<PendingNotification>,
}

#[derive(Serialize, Deserialize)]
struct PendingNotification {
    id: String,
    event: String,
    status: Status,
    error: Option<String>,
    at: String,
}

#[derive(Serialize, Deserialize)]
struct CheckState {
    status: Status,
    since: String,
    last_run_at: String,
    last_error: Option<String>,
}

#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Status {
    Ok,
    Failing,
}

#[derive(Serialize)]
struct CheckResult {
    id: String,
    status: Status,
    error: Option<String>,
    transition: Option<&'static str>,
}

fn state_path_for(file: &Path) -> PathBuf {
    let mut path = file.as_os_str().to_os_string();
    path.push(".monitor.json");
    PathBuf::from(path)
}

fn load_state(path: &Path) -> Result<MonitorState> {
    if !path.exists() {
        return Ok(MonitorState {
            version: 1,
            checks: HashMap::new(),
            pending_notifications: Vec::new(),
        });
    }
    serde_json::from_str(&std::fs::read_to_string(path)?)
        .with_context(|| format!("invalid monitor state {}", path.display()))
}

fn save_state(path: &Path, state: &MonitorState) -> Result<()> {
    crate::atomic_write::write(path, serde_json::to_string_pretty(state)?.as_bytes())
        .with_context(|| format!("writing monitor state {}", path.display()))
}

fn validate_check(check: &CheckSpec) -> Result<()> {
    match (
        check.url.is_some(),
        check.host.is_some(),
        check.port.is_some(),
    ) {
        (true, false, false) => Ok(()),
        (false, true, true) => Ok(()),
        _ => bail!(
            "check '{}' needs exactly one of url: or host: plus port:",
            check.id
        ),
    }
}

fn probe(check: &CheckSpec) -> Result<()> {
    validate_check(check)?;
    if let Some(url) = &check.url {
        let status = check::probe_url(url, check.timeout.unwrap_or(10))?;
        let expected = check.expected_status.unwrap_or(200);
        if status != expected {
            bail!("HTTP {status}, expected {expected}");
        }
        return Ok(());
    }
    check::probe_port(
        check.host.as_deref().expect("validated host"),
        check.port.expect("validated port"),
        check.timeout.unwrap_or(5),
    )
}

fn send_webhook(url: &str, monitor: &str, notification: &PendingNotification) -> Result<()> {
    let response = reqwest::blocking::Client::new()
        .post(url)
        .json(&serde_json::json!({
            "monitor": monitor,
            "check": notification.id,
            "event": notification.event,
            "status": notification.status,
            "error": notification.error,
            "at": notification.at,
        }))
        .send()
        .context("sending monitor webhook")?;
    if !response.status().is_success() {
        bail!("monitor webhook returned HTTP {}", response.status());
    }
    Ok(())
}

pub fn run(args: MonitorArgs, ctx: &Context) -> Result<()> {
    match args.subcommand {
        MonitorSubcommand::Run { file, state, dry } => run_file(&file, state.as_deref(), dry, ctx),
    }
}

fn run_file(file: &Path, state_override: Option<&Path>, dry: bool, ctx: &Context) -> Result<()> {
    let monitor: MonitorFile = serde_yaml::from_str(&std::fs::read_to_string(file)?)
        .with_context(|| format!("parsing monitor {}", file.display()))?;
    if monitor.checks.is_empty() {
        bail!("monitor '{}' has no checks", monitor.name);
    }
    let mut seen = std::collections::HashSet::new();
    for check in &monitor.checks {
        validate_check(check)?;
        if !seen.insert(&check.id) {
            bail!(
                "monitor '{}' has duplicate check id '{}'",
                monitor.name,
                check.id
            );
        }
    }
    if dry {
        let ids: Vec<&str> = monitor.checks.iter().map(|c| c.id.as_str()).collect();
        println!(
            "{}",
            serde_json::json!({"monitor": monitor.name, "dry": true, "checks": ids})
        );
        return Ok(());
    }

    let state_path = state_override
        .map(PathBuf::from)
        .unwrap_or_else(|| state_path_for(file));
    let mut state = load_state(&state_path)?;
    let now = chrono::Utc::now().to_rfc3339();
    let mut results = Vec::new();
    for check in &monitor.checks {
        let (status, error) = match probe(check) {
            Ok(()) => (Status::Ok, None),
            Err(error) => (Status::Failing, Some(error.to_string())),
        };
        let prior = state.checks.get(&check.id).map(|s| s.status);
        let transition = match (prior, status) {
            (Some(Status::Ok), Status::Failing) | (None, Status::Failing) => Some("failing"),
            (Some(Status::Failing), Status::Ok) => Some("recovered"),
            _ => None,
        };
        state.checks.insert(
            check.id.clone(),
            CheckState {
                status,
                since: if prior == Some(status) {
                    state
                        .checks
                        .get(&check.id)
                        .map(|s| s.since.clone())
                        .unwrap_or_else(|| now.clone())
                } else {
                    now.clone()
                },
                last_run_at: now.clone(),
                last_error: error.clone(),
            },
        );
        results.push(CheckResult {
            id: check.id.clone(),
            status,
            error,
            transition,
        });
        if let Some(event) = transition {
            state.pending_notifications.push(PendingNotification {
                id: check.id.clone(),
                event: event.to_string(),
                status,
                error: results.last().and_then(|result| result.error.clone()),
                at: now.clone(),
            });
        }
    }
    save_state(&state_path, &state)?;
    if let Some(webhook) = &monitor.webhook {
        let pending = std::mem::take(&mut state.pending_notifications);
        for notification in pending {
            if let Err(error) = send_webhook(&webhook.url, &monitor.name, &notification) {
                eprintln!("monitor webhook delivery deferred: {error}");
                state.pending_notifications.push(notification);
            }
        }
        save_state(&state_path, &state)?;
    }
    let success = results.iter().all(|r| r.status == Status::Ok);
    let output = serde_json::json!({
        "monitor": monitor.name,
        "state": state_path,
        "checks": results,
        "success": success,
    });
    println!("{output}");
    if !success {
        if ctx.output != OutputFormat::Json {
            eprintln!("monitor has failing checks");
        }
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_validation_requires_one_supported_target() {
        let valid = CheckSpec {
            id: "web".into(),
            url: Some("https://example.test".into()),
            host: None,
            port: None,
            timeout: None,
            expected_status: None,
        };
        assert!(validate_check(&valid).is_ok());
        let invalid = CheckSpec {
            id: "bad".into(),
            url: None,
            host: Some("localhost".into()),
            port: None,
            timeout: None,
            expected_status: None,
        };
        assert!(validate_check(&invalid).is_err());
    }

    #[test]
    fn pending_notifications_survive_state_round_trip() {
        let state = MonitorState {
            version: 1,
            checks: HashMap::new(),
            pending_notifications: vec![PendingNotification {
                id: "api".into(),
                event: "failing".into(),
                status: Status::Failing,
                error: Some("connection refused".into()),
                at: "2026-01-01T00:00:00Z".into(),
            }],
        };
        let restored: MonitorState =
            serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
        assert_eq!(restored.pending_notifications.len(), 1);
        assert_eq!(restored.pending_notifications[0].event, "failing");
    }
}
