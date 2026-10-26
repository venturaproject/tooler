use crate::{commands::check, context::Context};
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
    /// Repeatedly run a monitor definition until interrupted
    Watch {
        /// YAML monitor definition
        file: PathBuf,
        /// Seconds between checks (minimum: 1)
        #[arg(short, long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..))]
        interval: u64,
        /// Override the durable state file (default: <file>.monitor.json)
        #[arg(long)]
        state: Option<PathBuf>,
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
    #[serde(default = "one")]
    failure_threshold: u32,
    #[serde(default = "one")]
    recovery_threshold: u32,
}

fn one() -> u32 {
    1
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
    #[serde(default)]
    failure_count: u32,
    #[serde(default)]
    recovery_count: u32,
    #[serde(default)]
    last_duration_ms: u128,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Status {
    Ok,
    Failing,
}

#[derive(Serialize)]
struct CheckResult {
    id: String,
    status: Status,
    probe_status: Status,
    error: Option<String>,
    transition: Option<&'static str>,
    duration_ms: u128,
    failure_count: u32,
    recovery_count: u32,
}

fn status_after_probe(
    prior: Option<&CheckState>,
    probe_ok: bool,
    spec: &CheckSpec,
) -> (Status, u32, u32, Option<&'static str>) {
    let prior_status = prior.map(|state| state.status).unwrap_or(Status::Ok);
    let failure_count = if probe_ok {
        0
    } else {
        prior.map_or(1, |s| s.failure_count.saturating_add(1))
    };
    let recovery_count = if probe_ok && prior_status == Status::Failing {
        prior.map_or(1, |s| s.recovery_count.saturating_add(1))
    } else {
        0
    };
    let status = match prior_status {
        Status::Ok if !probe_ok && failure_count >= spec.failure_threshold => Status::Failing,
        Status::Failing if probe_ok && recovery_count >= spec.recovery_threshold => Status::Ok,
        value => value,
    };
    let transition = match (prior.map(|s| s.status), status) {
        (Some(Status::Ok), Status::Failing) | (None, Status::Failing) => Some("failing"),
        (Some(Status::Failing), Status::Ok) => Some("recovered"),
        _ => None,
    };
    (status, failure_count, recovery_count, transition)
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

pub fn run(args: MonitorArgs, _ctx: &Context) -> Result<()> {
    match args.subcommand {
        MonitorSubcommand::Run { file, state, dry } => {
            let success = run_file_once(&file, state.as_deref(), dry)?;
            if !success {
                bail!("monitor has failing checks");
            }
            Ok(())
        }
        MonitorSubcommand::Watch {
            file,
            interval,
            state,
        } => watch_file(&file, state.as_deref(), interval),
    }
}

fn watch_file(file: &Path, state: Option<&Path>, interval: u64) -> Result<()> {
    loop {
        // A failing probe is an expected monitor result, not a reason to stop watching.
        let _ = run_file_once(file, state, false)?;
        std::thread::sleep(std::time::Duration::from_secs(interval));
    }
}

fn run_file_once(file: &Path, state_override: Option<&Path>, dry: bool) -> Result<bool> {
    let monitor: MonitorFile = serde_yaml::from_str(&std::fs::read_to_string(file)?)
        .with_context(|| format!("parsing monitor {}", file.display()))?;
    if monitor.checks.is_empty() {
        bail!("monitor '{}' has no checks", monitor.name);
    }
    let mut seen = std::collections::HashSet::new();
    for check in &monitor.checks {
        validate_check(check)?;
        if check.failure_threshold == 0 || check.recovery_threshold == 0 {
            bail!("check '{}' thresholds must be at least 1", check.id);
        }
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
        return Ok(true);
    }

    let state_path = state_override
        .map(PathBuf::from)
        .unwrap_or_else(|| state_path_for(file));
    if let Some(parent) = state_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Hold the lock through probing, state update, delivery, and acknowledgement. This
    // prevents two monitor invocations from deriving duplicate transitions or dropping a
    // notification that the other invocation has just acknowledged.
    let _lock = crate::atomic_write::lock(&state_path)?;
    let mut state = load_state(&state_path)?;
    let now = chrono::Utc::now().to_rfc3339();
    let mut results = Vec::new();
    for check in &monitor.checks {
        let probe_started = std::time::Instant::now();
        let error = match probe(check) {
            Ok(()) => None,
            Err(error) => Some(error.to_string()),
        };
        let duration_ms = probe_started.elapsed().as_millis();
        let prior_state = state.checks.get(&check.id);
        let prior_status = prior_state.map(|s| s.status);
        let (confirmed_status, failure_count, recovery_count, transition) =
            status_after_probe(prior_state, error.is_none(), check);
        let since = if prior_status == Some(confirmed_status) {
            prior_state
                .map(|s| s.since.clone())
                .unwrap_or_else(|| now.clone())
        } else {
            now.clone()
        };
        state.checks.insert(
            check.id.clone(),
            CheckState {
                status: confirmed_status,
                since,
                last_run_at: now.clone(),
                last_error: error.clone(),
                failure_count,
                recovery_count,
                last_duration_ms: duration_ms,
            },
        );
        results.push(CheckResult {
            id: check.id.clone(),
            status: confirmed_status,
            probe_status: if error.is_none() {
                Status::Ok
            } else {
                Status::Failing
            },
            error,
            transition,
            duration_ms,
            failure_count,
            recovery_count,
        });
        if let Some(event) = transition {
            state.pending_notifications.push(PendingNotification {
                id: check.id.clone(),
                event: event.to_string(),
                status: confirmed_status,
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
    let success = results.iter().all(|r| r.error.is_none());
    let output = serde_json::json!({
        "monitor": monitor.name,
        "state": state_path,
        "checks": results,
        "success": success,
    });
    println!("{output}");
    Ok(success)
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
            failure_threshold: 1,
            recovery_threshold: 1,
        };
        assert!(validate_check(&valid).is_ok());
        let invalid = CheckSpec {
            id: "bad".into(),
            url: None,
            host: Some("localhost".into()),
            port: None,
            timeout: None,
            expected_status: None,
            failure_threshold: 1,
            recovery_threshold: 1,
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

    #[test]
    fn thresholds_require_consecutive_failures_and_recoveries() {
        let spec = CheckSpec {
            id: "api".into(),
            url: Some("https://example.test".into()),
            host: None,
            port: None,
            timeout: None,
            expected_status: None,
            failure_threshold: 2,
            recovery_threshold: 2,
        };
        let (status, failures, recoveries, transition) = status_after_probe(None, false, &spec);
        assert_eq!(status, Status::Ok);
        assert_eq!(failures, 1);
        assert_eq!(transition, None);
        let failing = CheckState {
            status,
            since: "now".into(),
            last_run_at: "now".into(),
            last_error: Some("down".into()),
            failure_count: failures,
            recovery_count: recoveries,
            last_duration_ms: 1,
        };
        let (status, failures, _, transition) = status_after_probe(Some(&failing), false, &spec);
        assert_eq!(status, Status::Failing);
        assert_eq!(failures, 2);
        assert_eq!(transition, Some("failing"));
        let recovered_once = CheckState {
            status,
            since: "then".into(),
            last_run_at: "then".into(),
            last_error: None,
            failure_count: failures,
            recovery_count: 0,
            last_duration_ms: 1,
        };
        let (status, _, recoveries, transition) =
            status_after_probe(Some(&recovered_once), true, &spec);
        assert_eq!(status, Status::Failing);
        assert_eq!(recoveries, 1);
        assert_eq!(transition, None);
        let recovered_twice = CheckState {
            recovery_count: recoveries,
            ..recovered_once
        };
        let (status, _, _, transition) = status_after_probe(Some(&recovered_twice), true, &spec);
        assert_eq!(status, Status::Ok);
        assert_eq!(transition, Some("recovered"));
    }
}
