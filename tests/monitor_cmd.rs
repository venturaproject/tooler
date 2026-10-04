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
