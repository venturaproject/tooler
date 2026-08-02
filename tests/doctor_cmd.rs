mod common;

use common::tooler;

/// `doctor` touches machine-dependent state (git identity, OS keychain, SSH agent), so
/// this is a shape/smoke test only — it must not crash, and its JSON must have the
/// expected top-level structure, regardless of whether individual checks pass on the
/// machine running the test. Exit code isn't asserted either way: `doctor` exits 1 if
/// any check fails, which is expected and machine-dependent, not a test failure.
#[test]
fn reports_a_checks_array_and_a_healthy_bool() {
    let (mut cmd, _dir) = tooler();
    let output = cmd
        .args(["--output", "json", "doctor"])
        .output()
        .expect("run tooler doctor");

    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("doctor did not print valid JSON");
    assert!(value["checks"].is_array());
    assert!(!value["checks"].as_array().unwrap().is_empty());
    assert!(value["healthy"].is_boolean());
}
