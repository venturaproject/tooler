mod common;

use common::tooler;

fn stdout_of(assert: assert_cmd::assert::Assert) -> String {
    String::from_utf8_lossy(&assert.get_output().stdout).to_string()
}

#[test]
fn info_json_has_expected_top_level_keys() {
    let (mut cmd, _dir) = tooler();
    let out = stdout_of(cmd.args(["--output", "json", "info"]).assert().success());
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(value.get("dir").is_some());
    assert!(value.get("env").is_some());
}

#[test]
fn echo_prints_the_given_text() {
    let (mut cmd, _dir) = tooler();
    let out = stdout_of(cmd.args(["echo", "hello", "world"]).assert().success());
    assert_eq!(out.trim(), "hello world");
}

#[test]
fn echo_repeats_and_uppercases() {
    let (mut cmd, _dir) = tooler();
    let out = stdout_of(
        cmd.args(["echo", "hi", "--upper", "--repeat", "2"])
            .assert()
            .success(),
    );
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines, vec!["HI", "HI"]);
}

#[test]
fn json_pretty_prints_a_file() {
    let (mut cmd, dir) = tooler();
    std::fs::write(dir.path().join("data.json"), r#"{"a": {"b": 1}}"#).unwrap();
    let out = stdout_of(cmd.args(["json", "data.json"]).assert().success());
    assert!(out.contains('1'));
}

#[test]
fn json_extracts_a_dotted_key() {
    let (mut cmd, dir) = tooler();
    std::fs::write(dir.path().join("data.json"), r#"{"a": {"b": 42}}"#).unwrap();
    let out = stdout_of(
        cmd.args(["json", "data.json", "--key", "a.b"])
            .assert()
            .success(),
    );
    assert_eq!(out.trim(), "42");
}
