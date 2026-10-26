mod common;

use common::{tooler, tooler_in};

#[test]
fn dry_run_validates_and_lists_checks_without_creating_state() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("monitor.yml"),
        "name: Smoke\nchecks:\n  - id: api\n    url: https://example.test/health\n",
    )
    .unwrap();

    let output = cmd
        .args(["monitor", "run", "monitor.yml", "--dry"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(value["monitor"], "Smoke");
    assert_eq!(value["checks"][0], "api");
    assert!(!dir.path().join("monitor.yml.monitor.json").exists());
}

#[test]
fn a_failed_check_writes_durable_failure_state() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("monitor.yml"),
        "name: Smoke\nchecks:\n  - id: unavailable\n    host: 127.0.0.1\n    port: 1\n    timeout: 1\n",
    )
    .unwrap();

    cmd.args(["--output", "json", "monitor", "run", "monitor.yml"])
        .assert()
        .failure();
    let state: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("monitor.yml.monitor.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(state["checks"]["unavailable"]["status"], "failing");

    tooler_in(dir.path())
        .args(["monitor", "run", "monitor.yml", "--dry"])
        .assert()
        .success();
}

#[test]
fn watch_rejects_zero_interval_without_running_checks() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("monitor.yml"),
        "name: Smoke\nchecks:\n  - id: unavailable\n    host: 127.0.0.1\n    port: 1\n",
    )
    .unwrap();

    cmd.args(["monitor", "watch", "monitor.yml", "--interval", "0"])
        .assert()
        .failure();
    assert!(!dir.path().join("monitor.yml.monitor.json").exists());
}

#[test]
fn failure_threshold_delays_confirmed_failure_until_consecutive_probes() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("monitor.yml"),
        "name: Smoke\nchecks:\n  - id: unavailable\n    host: 127.0.0.1\n    port: 1\n    timeout: 1\n    failure_threshold: 2\n",
    )
    .unwrap();

    let first = cmd
        .args(["--output", "json", "monitor", "run", "monitor.yml"])
        .assert()
        .failure()
        .get_output()
        .stdout
        .clone();
    let first: serde_json::Value = serde_json::from_slice(&first).unwrap();
    assert_eq!(first["checks"][0]["failure_count"], 1);
    assert_eq!(first["checks"][0]["status"], "ok");
    assert_eq!(first["checks"][0]["probe_status"], "failing");
    assert!(first["checks"][0]["duration_ms"].is_number());
    let state: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("monitor.yml.monitor.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(state["checks"]["unavailable"]["status"], "ok");

    let second = tooler_in(dir.path())
        .args(["--output", "json", "monitor", "run", "monitor.yml"])
        .assert()
        .failure()
        .get_output()
        .stdout
        .clone();
    let second: serde_json::Value = serde_json::from_slice(&second).unwrap();
    assert_eq!(second["checks"][0]["failure_count"], 2);
    assert_eq!(second["checks"][0]["transition"], "failing");
}
