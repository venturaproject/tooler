mod common;

use common::{tooler, tooler_in};

#[test]
fn records_operational_invocations_and_filters_failures_without_storing_arguments() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("monitor.yml"),
        "name: Test\nchecks:\n  - id: api\n    url: https://example.test/health\n",
    )
    .unwrap();

    cmd.args(["monitor", "run", "monitor.yml", "--dry"])
        .assert()
        .success();
    tooler_in(dir.path())
        .args(["check", "port", "127.0.0.1", "1"])
        .assert()
        .failure();

    let out = tooler_in(dir.path())
        .args(["--output", "json", "history", "--limit", "10"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let runs = value["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0]["command"], "check");
    assert_eq!(runs[0]["outcome"], "failed");
    assert_eq!(runs[1]["command"], "monitor");
    assert_eq!(runs[1]["outcome"], "success");
    assert!(runs.iter().all(|run| run.get("args").is_none()));

    let failed = tooler_in(dir.path())
        .args(["--output", "json", "history", "--failed"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let failed: serde_json::Value = serde_json::from_slice(&failed).unwrap();
    assert_eq!(failed["runs"].as_array().unwrap().len(), 1);
}
