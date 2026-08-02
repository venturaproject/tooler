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

#[test]
fn notes_are_shown_automatically_on_a_normal_run() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Notes test\ntasks:\n  - name: t\n    run: echo hi\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("playbook.md"),
        "# Heads up\n\nBe careful.\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["--output", "json", "play", "playbook.yml"])
            .assert()
            .success(),
    );
    let value = last_line_json(&out);
    assert_eq!(value["notes"], "# Heads up\n\nBe careful.\n");
}

#[test]
fn notes_flag_prints_without_running_tasks() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Notes test\ntasks:\n  - name: t\n    run: echo hi >> log.txt\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("playbook.md"), "Read this first.\n").unwrap();

    let out = stdout_of(
        cmd.args(["play", "playbook.yml", "--notes"])
            .assert()
            .success(),
    );
    assert!(out.contains("Read this first."));
    assert!(!dir.path().join("log.txt").exists());
}

#[test]
fn notes_flag_reports_null_when_no_md_file_exists() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: No notes\ntasks:\n  - name: t\n    run: echo hi\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["--output", "json", "play", "playbook.yml", "--notes"])
            .assert()
            .success(),
    );
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(value["notes"].is_null());
}

#[test]
fn list_marks_entries_that_have_notes() {
    let (mut cmd, dir) = tooler();
    std::fs::create_dir_all(dir.path().join("playbooks")).unwrap();
    std::fs::write(
        dir.path().join("playbooks/with-notes.yml"),
        "name: A\ntasks:\n  - name: t\n    run: echo hi\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("playbooks/with-notes.md"), "notes\n").unwrap();
    std::fs::write(
        dir.path().join("playbooks/without-notes.yml"),
        "name: B\ntasks:\n  - name: t\n    run: echo hi\n",
    )
    .unwrap();

    let out = stdout_of(cmd.args(["play"]).assert().success());
    let with_notes_line = out.lines().find(|l| l.contains("with-notes")).unwrap();
    let without_notes_line = out.lines().find(|l| l.contains("without-notes")).unwrap();
    assert!(with_notes_line.contains("[notes]"));
    assert!(!without_notes_line.contains("[notes]"));
}

#[test]
fn register_captures_run_output_and_when_uses_it() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Register test\n\
         tasks:\n\
         \x20\x20- name: capture\n\
         \x20\x20\x20\x20run: echo captured-value\n\
         \x20\x20\x20\x20register: result\n\
         \x20\x20- name: use it\n\
         \x20\x20\x20\x20when: \"{{result}} == captured-value\"\n\
         \x20\x20\x20\x20run: echo used-it\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["--output", "json", "play", "playbook.yml"])
            .assert()
            .success(),
    );
    let value = last_line_json(&out);
    assert_eq!(value["tasks"][0]["status"], "ok");
    assert_eq!(value["tasks"][1]["status"], "ok");
    assert_eq!(value["ok"], 2);
}

#[test]
fn retries_eventually_succeeds() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Retry test\n\
         tasks:\n\
         \x20\x20- name: flaky\n\
         \x20\x20\x20\x20run: test -f marker && exit 0 || { touch marker; exit 1; }\n\
         \x20\x20\x20\x20retries: 1\n\
         \x20\x20\x20\x20delay: 0\n",
    )
    .unwrap();

    cmd.args(["play", "playbook.yml"]).assert().success();
}

#[test]
fn retries_exhausted_fails_the_task_with_a_retry_log_line() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Retry exhaustion test\n\
         tasks:\n\
         \x20\x20- name: always fails\n\
         \x20\x20\x20\x20run: exit 1\n\
         \x20\x20\x20\x20retries: 1\n\
         \x20\x20\x20\x20delay: 0\n",
    )
    .unwrap();

    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().failure());
    assert!(out.contains("attempt 1/2 failed"));
}

#[test]
fn include_runs_a_sub_playbook_and_shares_vars() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("sub.yml"),
        "name: Sub\ntasks:\n  - name: register something\n    run: echo from-sub\n    register: sub_result\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Parent\n\
         tasks:\n\
         \x20\x20- name: run sub\n\
         \x20\x20\x20\x20include: sub.yml\n\
         \x20\x20- name: use sub var\n\
         \x20\x20\x20\x20when: \"{{sub_result}} == from-sub\"\n\
         \x20\x20\x20\x20run: echo parent-used-it\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["--output", "json", "play", "playbook.yml"])
            .assert()
            .success(),
    );
    let value = last_line_json(&out);
    assert_eq!(value["tasks"][0]["status"], "ok");
    assert_eq!(value["tasks"][1]["status"], "ok");
}

#[test]
fn include_cycle_is_rejected() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("a.yml"),
        "name: A\ntasks:\n  - name: include b\n    include: b.yml\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("b.yml"),
        "name: B\ntasks:\n  - name: include a\n    include: a.yml\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["play", "a.yml"])
            .timeout(std::time::Duration::from_secs(10))
            .assert()
            .failure(),
    );
    assert!(out.to_lowercase().contains("cycle"));
}

#[test]
fn fleet_task_parallel_flag_runs_without_hanging_or_panicking() {
    let (mut cmd, dir) = tooler();
    tooler_in(dir.path())
        .args(["server", "add", "s1", "--host", "127.0.0.1", "--port", "1"])
        .assert()
        .success();
    tooler_in(dir.path())
        .args(["server", "add", "s2", "--host", "127.0.0.1", "--port", "2"])
        .assert()
        .success();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Parallel fleet test\n\
         tasks:\n\
         \x20\x20- name: fan out\n\
         \x20\x20\x20\x20fleet:\n\
         \x20\x20\x20\x20\x20\x20servers: s1,s2\n\
         \x20\x20\x20\x20\x20\x20command: echo hi\n\
         \x20\x20\x20\x20\x20\x20parallel: true\n\
         \x20\x20\x20\x20ignore_errors: true\n",
    )
    .unwrap();

    // Unreachable servers, so the fleet: task itself fails — but ignore_errors keeps the
    // playbook going. The point is proving the parallel path completes (doesn't hang or
    // panic across threads), not that SSH actually succeeds.
    cmd.args(["--output", "json", "play", "playbook.yml"])
        .timeout(std::time::Duration::from_secs(15))
        .assert()
        .success();
}
