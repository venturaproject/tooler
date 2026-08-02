mod common;

use common::{tooler, tooler_in};
use std::net::TcpListener;
use tempfile::tempdir;

fn stdout_of(assert: assert_cmd::assert::Assert) -> String {
    String::from_utf8_lossy(&assert.get_output().stdout).to_string()
}

/// `--output json` still prints whatever a `run:` task's own subprocess writes to
/// stdout (that stream is inherited, not captured, so plain-mode's live-output
/// behavior works — see `run_task_once`'s `run:` branch) — only the final summary line
/// is guaranteed pure JSON. Parsing just the last line is robust to that.
fn last_line_json(out: &str) -> serde_json::Value {
    serde_json::from_str(out.lines().next_back().expect("non-empty output"))
        .expect("last line was not valid JSON")
}

#[test]
fn init_creates_a_playbook_in_the_playbooks_dir() {
    let (mut cmd, dir) = tooler();
    cmd.args(["play", "--init"]).assert().success();
    assert!(dir.path().join("playbooks/playbook.yml").exists());

    tooler_in(dir.path())
        .args(["play", "--init", "smoke"])
        .assert()
        .success();
    assert!(dir.path().join("playbooks/smoke.yml").exists());
}

#[test]
fn bare_play_lists_the_playbooks_dir() {
    let (mut cmd, dir) = tooler();
    cmd.args(["play", "--init", "smoke"]).assert().success();

    let out = stdout_of(tooler_in(dir.path()).args(["play"]).assert().success());
    assert!(out.contains("smoke"));
}

#[test]
fn runs_a_playbook_by_name_after_init() {
    // Writing directly into playbooks/ (rather than using --init's sample content,
    // whose build/test/deploy tasks all assume a real project directory) keeps this
    // test focused on name resolution, not on the sample's own task content.
    let (_cmd, dir) = tooler();
    std::fs::create_dir_all(dir.path().join("playbooks")).unwrap();
    std::fs::write(
        dir.path().join("playbooks/smoke.yml"),
        "name: Smoke\ntasks:\n  - name: say hi\n    run: echo hi\n",
    )
    .unwrap();
    tooler_in(dir.path())
        .args(["play", "smoke"])
        .assert()
        .success();
}

#[test]
fn runs_a_playbook_given_a_literal_path() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("custom.yml"),
        "name: Custom\ntasks:\n  - name: say hi\n    run: echo hi\n",
    )
    .unwrap();
    cmd.args(["play", "./custom.yml"]).assert().success();
}

#[test]
fn local_task_actions_all_succeed() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let _ = listener.accept();
    });

    let (mut cmd, dir) = tooler();
    std::fs::write(dir.path().join(".env.example"), "A=\n").unwrap();
    std::fs::write(dir.path().join(".env"), "A=1\n").unwrap();
    std::fs::write(
        dir.path().join("playbook.yml"),
        format!(
            "name: Local actions\n\
             tasks:\n\
             \x20\x20- name: run a command\n\
             \x20\x20\x20\x20run: echo hi\n\
             \x20\x20- name: check the port\n\
             \x20\x20\x20\x20check_port:\n\
             \x20\x20\x20\x20\x20\x20host: 127.0.0.1\n\
             \x20\x20\x20\x20\x20\x20port: {port}\n\
             \x20\x20- name: check the env file\n\
             \x20\x20\x20\x20env_check:\n\
             \x20\x20\x20\x20\x20\x20reference: .env.example\n\
             \x20\x20\x20\x20\x20\x20target: .env\n"
        ),
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["--output", "json", "play", "playbook.yml"])
            .assert()
            .success(),
    );
    let value = last_line_json(&out);
    assert_eq!(value["success"], true);
    assert_eq!(value["ok"], 3);
    assert_eq!(value["failed"], 0);
}

#[test]
fn when_skips_or_runs_based_on_var() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: When test\n\
         vars:\n  env: staging\n\
         tasks:\n\
         \x20\x20- name: prod only\n\
         \x20\x20\x20\x20when: \"{{env}} == prod\"\n\
         \x20\x20\x20\x20run: echo prod-task-ran\n",
    )
    .unwrap();

    let skipped_out = stdout_of(
        cmd.args(["--output", "json", "play", "playbook.yml"])
            .assert()
            .success(),
    );
    let skipped_value = last_line_json(&skipped_out);
    assert_eq!(skipped_value["tasks"][0]["status"], "skipped");

    let ran_out = stdout_of(
        tooler_in(dir.path())
            .args([
                "--output",
                "json",
                "play",
                "playbook.yml",
                "--var",
                "env=prod",
            ])
            .assert()
            .success(),
    );
    let ran_value = last_line_json(&ran_out);
    assert_eq!(ran_value["tasks"][0]["status"], "ok");
}

#[test]
fn loop_runs_once_per_item_in_order() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Loop test\n\
         tasks:\n\
         \x20\x20- name: append items\n\
         \x20\x20\x20\x20loop: [a, b, c]\n\
         \x20\x20\x20\x20run: echo \"{{item}}\" >> log.txt\n",
    )
    .unwrap();

    cmd.args(["play", "playbook.yml"]).assert().success();

    let log = std::fs::read_to_string(dir.path().join("log.txt")).unwrap();
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(lines, vec!["a", "b", "c"]);
}

#[test]
fn loop_failure_aborts_remaining_items_and_playbook() {
    let dir = tempdir().unwrap();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Loop failure test\n\
         tasks:\n\
         \x20\x20- name: fails on b\n\
         \x20\x20\x20\x20loop: [a, b, c]\n\
         \x20\x20\x20\x20run: test \"{{item}}\" != \"b\"\n\
         \x20\x20- name: never runs\n\
         \x20\x20\x20\x20run: echo should-not-run\n",
    )
    .unwrap();

    let out = stdout_of(
        tooler_in(dir.path())
            .args(["--output", "json", "play", "playbook.yml"])
            .assert()
            .failure(),
    );
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(value["success"], false);
    assert_eq!(value["failed"], 1);
    // Only the failed task's outcome is recorded — the second task is never reached.
    assert_eq!(value["tasks"].as_array().unwrap().len(), 1);
}

#[test]
fn dry_run_reports_skipped_without_executing() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Dry test\ntasks:\n  - name: t\n    run: echo hi >> log.txt\n",
    )
    .unwrap();

    cmd.args(["play", "playbook.yml", "--dry"])
        .assert()
        .success();

    assert!(!dir.path().join("log.txt").exists());
}
