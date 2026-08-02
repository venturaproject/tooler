mod common;

use common::tooler;

fn stdout_of(assert: assert_cmd::assert::Assert) -> String {
    String::from_utf8_lossy(&assert.get_output().stdout).to_string()
}

#[test]
fn show_masks_values_by_default() {
    let (mut cmd, dir) = tooler();
    std::fs::write(dir.path().join(".env"), "SECRET=abcdef123\n").unwrap();
    let out = stdout_of(cmd.args(["env", "show"]).assert().success());
    assert!(out.contains("SECRET"));
}

#[test]
fn list_returns_keys_as_json() {
    let (mut cmd, dir) = tooler();
    std::fs::write(dir.path().join(".env"), "A=1\nB=2\n").unwrap();
    let out = stdout_of(
        cmd.args(["--output", "json", "env", "list"])
            .assert()
            .success(),
    );
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    let keys: Vec<&str> = value["keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(keys, vec!["A", "B"]);
}

#[test]
fn get_returns_value() {
    let (mut cmd, dir) = tooler();
    std::fs::write(dir.path().join(".env"), "FOO=bar\n").unwrap();
    let out = stdout_of(cmd.args(["env", "get", "FOO"]).assert().success());
    assert!(out.contains("bar"));
}

#[test]
fn get_missing_key_fails() {
    let (mut cmd, dir) = tooler();
    std::fs::write(dir.path().join(".env"), "FOO=bar\n").unwrap();
    cmd.args(["env", "get", "MISSING"]).assert().failure();
}

#[test]
fn diff_reports_keys_only_in_one_file() {
    let (mut cmd, dir) = tooler();
    std::fs::write(dir.path().join("a.env"), "A=1\nSHARED=1\n").unwrap();
    std::fs::write(dir.path().join("b.env"), "B=1\nSHARED=1\n").unwrap();
    let out = stdout_of(
        cmd.args(["env", "diff", "a.env", "b.env"])
            .assert()
            .success(),
    );
    assert!(out.contains('A'));
    assert!(out.contains('B'));
}

#[test]
fn check_passes_when_all_reference_keys_present() {
    let (mut cmd, dir) = tooler();
    std::fs::write(dir.path().join(".env.example"), "A=\nB=\n").unwrap();
    std::fs::write(dir.path().join(".env"), "A=1\nB=2\n").unwrap();
    cmd.args(["env", "check", ".env.example"])
        .assert()
        .success();
}

#[test]
fn check_fails_when_a_key_is_missing() {
    let (mut cmd, dir) = tooler();
    std::fs::write(dir.path().join(".env.example"), "A=\nB=\n").unwrap();
    std::fs::write(dir.path().join(".env"), "A=1\n").unwrap();
    cmd.args(["env", "check", ".env.example"])
        .assert()
        .failure();
}
