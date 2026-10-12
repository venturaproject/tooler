mod common;

use common::{tooler, tooler_in};

fn write_taskfile(dir: &std::path::Path) {
    std::fs::write(
        dir.join("Taskfile.yml"),
        r#"
version: 1
name: integration-test
single_instance: true
targets:
  default:
    deps: [build]
    phony: true
    command: "echo default"
  build:
    inputs: [source.txt]
    outputs: [build.txt]
    command: "echo built > build.txt"
"#,
    )
    .unwrap();
    std::fs::write(dir.join("source.txt"), "source").unwrap();
}

fn stdout_of(assert: assert_cmd::assert::Assert) -> String {
    String::from_utf8_lossy(&assert.get_output().stdout).to_string()
}

#[test]
fn executes_dependencies_and_skips_current_outputs() {
    let (mut cmd, dir) = tooler();
    write_taskfile(dir.path());
    let first = stdout_of(cmd.args(["task"]).assert().success());
    assert!(first.contains("build"));
    assert!(dir.path().join("build.txt").is_file());

    let mut cmd = tooler_in(dir.path());
    let second = stdout_of(cmd.args(["task"]).assert().success());
    assert!(second.contains("build            up_to_date"));
    assert!(second.contains("default          ok"));
}

#[test]
fn plan_graph_and_history_are_inspectable() {
    let (mut cmd, dir) = tooler();
    write_taskfile(dir.path());
    let plan = stdout_of(cmd.args(["task", "--plan"]).assert().success());
    assert!(plan.contains("would_run"));
    assert!(!dir.path().join("build.txt").exists());

    let mut cmd = tooler_in(dir.path());
    let graph = stdout_of(cmd.args(["task", "--graph"]).assert().success());
    assert!(graph.contains("default <- build"));

    let mut cmd = tooler_in(dir.path());
    cmd.args(["task"]).assert().success();
    let mut cmd = tooler_in(dir.path());
    let history = stdout_of(cmd.args(["task", "--history"]).assert().success());
    assert!(history.contains("default"));
}

#[test]
fn invalid_target_dependency_is_rejected() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("Taskfile.yml"),
        "version: 1\ntargets:\n  build:\n    deps: [missing]\n    outputs: [out]\n    command: echo ok\n",
    )
    .unwrap();
    cmd.args(["task"]).assert().failure();
}

#[test]
fn json_output_stays_a_single_document() {
    let (mut cmd, dir) = tooler();
    write_taskfile(dir.path());
    let out = stdout_of(cmd.args(["task", "--output", "json"]).assert().success());
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(value["success"], true);
}
