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

#[test]
fn assert_failure_aborts_the_playbook_with_a_clear_message() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Assert test\n\
         vars:\n  env: staging\n\
         tasks:\n\
         \x20\x20- name: must be prod\n\
         \x20\x20\x20\x20assert: \"{{env}} == prod\"\n",
    )
    .unwrap();

    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().failure());
    assert!(out.to_lowercase().contains("assertion failed"));
}

#[test]
fn block_runs_tasks_in_order_and_counts_as_one_outcome() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Block test\n\
         tasks:\n\
         \x20\x20- name: my block\n\
         \x20\x20\x20\x20block:\n\
         \x20\x20\x20\x20\x20\x20- name: t1\n\
         \x20\x20\x20\x20\x20\x20\x20\x20run: echo one >> log.txt\n\
         \x20\x20\x20\x20\x20\x20- name: t2\n\
         \x20\x20\x20\x20\x20\x20\x20\x20run: echo two >> log.txt\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["--output", "json", "play", "playbook.yml"])
            .assert()
            .success(),
    );
    let value = last_line_json(&out);
    assert_eq!(value["tasks"].as_array().unwrap().len(), 1);
    assert_eq!(value["ok"], 1);

    let log = std::fs::read_to_string(dir.path().join("log.txt")).unwrap();
    assert_eq!(log.lines().collect::<Vec<_>>(), vec!["one", "two"]);
}

#[test]
fn block_failure_runs_rescue_and_recovers() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Rescue test\n\
         tasks:\n\
         \x20\x20- name: risky block\n\
         \x20\x20\x20\x20block:\n\
         \x20\x20\x20\x20\x20\x20- name: fails\n\
         \x20\x20\x20\x20\x20\x20\x20\x20run: exit 1\n\
         \x20\x20\x20\x20rescue:\n\
         \x20\x20\x20\x20\x20\x20- name: recover\n\
         \x20\x20\x20\x20\x20\x20\x20\x20run: echo recovered >> log.txt\n",
    )
    .unwrap();

    cmd.args(["play", "playbook.yml"]).assert().success();
    let log = std::fs::read_to_string(dir.path().join("log.txt")).unwrap();
    assert!(log.contains("recovered"));
}

#[test]
fn always_runs_even_after_a_successful_rescue_and_can_still_fail_the_block() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Always test\n\
         tasks:\n\
         \x20\x20- name: risky block\n\
         \x20\x20\x20\x20block:\n\
         \x20\x20\x20\x20\x20\x20- name: fails\n\
         \x20\x20\x20\x20\x20\x20\x20\x20run: exit 1\n\
         \x20\x20\x20\x20rescue:\n\
         \x20\x20\x20\x20\x20\x20- name: recover\n\
         \x20\x20\x20\x20\x20\x20\x20\x20run: echo recovered\n\
         \x20\x20\x20\x20always:\n\
         \x20\x20\x20\x20\x20\x20- name: cleanup fails\n\
         \x20\x20\x20\x20\x20\x20\x20\x20run: exit 1\n",
    )
    .unwrap();

    cmd.args(["play", "playbook.yml"]).assert().failure();
}

#[test]
fn handler_runs_once_when_notified_and_deduplicates() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Handler test\n\
         handlers:\n\
         \x20\x20- name: restart\n\
         \x20\x20\x20\x20run: echo restarted >> log.txt\n\
         tasks:\n\
         \x20\x20- name: t1\n\
         \x20\x20\x20\x20run: echo t1\n\
         \x20\x20\x20\x20notify: [restart]\n\
         \x20\x20- name: t2\n\
         \x20\x20\x20\x20run: echo t2\n\
         \x20\x20\x20\x20notify: [restart]\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["--output", "json", "play", "playbook.yml"])
            .assert()
            .success(),
    );
    let value = last_line_json(&out);
    let restart_count = value["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["name"] == "restart")
        .count();
    assert_eq!(restart_count, 1);

    let log = std::fs::read_to_string(dir.path().join("log.txt")).unwrap();
    assert_eq!(log.lines().count(), 1);
}

#[test]
fn changed_when_false_suppresses_notification() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Changed test\n\
         handlers:\n\
         \x20\x20- name: restart\n\
         \x20\x20\x20\x20run: echo restarted >> log.txt\n\
         tasks:\n\
         \x20\x20- name: t1\n\
         \x20\x20\x20\x20run: echo t1\n\
         \x20\x20\x20\x20changed_when: \"false\"\n\
         \x20\x20\x20\x20notify: [restart]\n",
    )
    .unwrap();

    cmd.args(["play", "playbook.yml"]).assert().success();
    assert!(!dir.path().join("log.txt").exists());
}

#[test]
fn unknown_notify_target_is_rejected_upfront() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Bad notify\n\
         tasks:\n\
         \x20\x20- name: t1\n\
         \x20\x20\x20\x20run: echo hi >> log.txt\n\
         \x20\x20\x20\x20notify: [nonexistent]\n",
    )
    .unwrap();

    cmd.args(["play", "playbook.yml"]).assert().failure();
    // The task never actually ran — validation happens before any task executes.
    assert!(!dir.path().join("log.txt").exists());
}

#[test]
fn env_templating_resolves_from_the_process_environment() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Env template test\n\
         tasks:\n\
         \x20\x20- name: use env var\n\
         \x20\x20\x20\x20run: echo \"{{env.TOOLER_TEST_VAR}}\" >> log.txt\n",
    )
    .unwrap();

    cmd.env("TOOLER_TEST_VAR", "hello-env")
        .args(["play", "playbook.yml"])
        .assert()
        .success();

    let log = std::fs::read_to_string(dir.path().join("log.txt")).unwrap();
    assert_eq!(log.trim(), "hello-env");
}

#[test]
fn secret_templating_leaves_token_literal_when_unresolvable() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Secret template test\n\
         tasks:\n\
         \x20\x20- name: use secret\n\
         \x20\x20\x20\x20run: echo \"{{secret.definitely_not_a_real_profile_xyz.token}}\" >> log.txt\n",
    )
    .unwrap();

    cmd.args(["play", "playbook.yml"]).assert().success();
    let log = std::fs::read_to_string(dir.path().join("log.txt")).unwrap();
    assert!(log.contains("{{secret.definitely_not_a_real_profile_xyz.token}}"));
}

#[test]
fn debug_prints_a_rendered_message() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Debug test\n\
         tasks:\n\
         \x20\x20- name: capture\n\
         \x20\x20\x20\x20run: echo captured-value\n\
         \x20\x20\x20\x20register: result\n\
         \x20\x20- name: show it\n\
         \x20\x20\x20\x20debug: \"result is {{result}}\"\n",
    )
    .unwrap();

    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().success());
    assert!(out.contains("result is captured-value"));
}

#[test]
fn fleet_servers_field_is_templated() {
    // Plain (non-JSON) output, since fleet:'s per-server ✓/✗ lines print the resolved
    // server name directly — the clearest observable proof that servers: was rendered
    // through {{var}} rather than treated as the literal string "{{target}}".
    let (mut cmd, dir) = tooler();
    tooler_in(dir.path())
        .args([
            "server",
            "add",
            "realserver",
            "--host",
            "127.0.0.1",
            "--port",
            "1",
        ])
        .assert()
        .success();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Templated fleet test\n\
         tasks:\n\
         \x20\x20- name: fan out\n\
         \x20\x20\x20\x20fleet:\n\
         \x20\x20\x20\x20\x20\x20servers: \"{{target}}\"\n\
         \x20\x20\x20\x20\x20\x20command: echo hi\n\
         \x20\x20\x20\x20ignore_errors: true\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["play", "playbook.yml", "--var", "target=realserver"])
            .timeout(std::time::Duration::from_secs(10))
            .assert()
            .success(),
    );
    assert!(out.contains("realserver"));
    assert!(!out.contains("{{target}}"));
}

#[test]
fn loop_over_map_items_exposes_item_fields() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Structured loop test\n\
         tasks:\n\
         \x20\x20- name: append pairs\n\
         \x20\x20\x20\x20loop:\n\
         \x20\x20\x20\x20\x20\x20- name: a\n\
         \x20\x20\x20\x20\x20\x20\x20\x20port: \"1\"\n\
         \x20\x20\x20\x20\x20\x20- name: b\n\
         \x20\x20\x20\x20\x20\x20\x20\x20port: \"2\"\n\
         \x20\x20\x20\x20run: echo \"{{item.name}}:{{item.port}}\" >> log.txt\n",
    )
    .unwrap();

    cmd.args(["play", "playbook.yml"]).assert().success();
    let log = std::fs::read_to_string(dir.path().join("log.txt")).unwrap();
    assert_eq!(log.lines().collect::<Vec<_>>(), vec!["a:1", "b:2"]);
}

#[test]
fn timeout_kills_a_hung_command() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Timeout test\n\
         tasks:\n\
         \x20\x20- name: hangs\n\
         \x20\x20\x20\x20run: sleep 5\n\
         \x20\x20\x20\x20timeout: 1\n",
    )
    .unwrap();

    let start = std::time::Instant::now();
    let out = stdout_of(
        cmd.args(["play", "playbook.yml"])
            .timeout(std::time::Duration::from_secs(10))
            .assert()
            .failure(),
    );
    let elapsed = start.elapsed();
    assert!(out.to_lowercase().contains("timed out"));
    assert!(
        elapsed < std::time::Duration::from_secs(3),
        "expected the timeout to cut this short, took {elapsed:?}"
    );
}

#[test]
fn timeout_without_run_is_rejected() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Bad timeout\n\
         tasks:\n\
         \x20\x20- name: t1\n\
         \x20\x20\x20\x20check_url: http://example.com\n\
         \x20\x20\x20\x20timeout: 5\n",
    )
    .unwrap();

    cmd.args(["play", "playbook.yml"]).assert().failure();
}

#[test]
fn sync_db_reports_a_structured_failure_against_an_unreachable_server() {
    // Same "point a throwaway server profile at an unreachable port" trick as
    // fleet_servers_field_is_templated — proves sync_db: actually reaches out over SSH
    // and fails cleanly rather than hanging, without needing real DB infra.
    let (mut cmd, dir) = tooler();
    tooler_in(dir.path())
        .args([
            "server",
            "add",
            "realserver",
            "--host",
            "127.0.0.1",
            "--port",
            "1",
        ])
        .assert()
        .success();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: DB sync test\n\
         tasks:\n\
         \x20\x20- name: sync\n\
         \x20\x20\x20\x20sync_db:\n\
         \x20\x20\x20\x20\x20\x20server: realserver\n\
         \x20\x20\x20\x20\x20\x20from:\n\
         \x20\x20\x20\x20\x20\x20\x20\x20engine: mysql\n\
         \x20\x20\x20\x20\x20\x20\x20\x20host: 127.0.0.1\n\
         \x20\x20\x20\x20\x20\x20\x20\x20database: a\n\
         \x20\x20\x20\x20\x20\x20\x20\x20user: u\n\
         \x20\x20\x20\x20\x20\x20to:\n\
         \x20\x20\x20\x20\x20\x20\x20\x20engine: mysql\n\
         \x20\x20\x20\x20\x20\x20\x20\x20host: 127.0.0.1\n\
         \x20\x20\x20\x20\x20\x20\x20\x20database: b\n\
         \x20\x20\x20\x20\x20\x20\x20\x20user: u\n",
    )
    .unwrap();

    cmd.args(["play", "playbook.yml"])
        .timeout(std::time::Duration::from_secs(10))
        .assert()
        .failure();
}

#[test]
fn vars_files_merge_precedence_file_then_inline_then_cli_var() {
    let (mut cmd, dir) = tooler();
    std::fs::write(dir.path().join("defaults.yml"), "a: file\nb: file\n").unwrap();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: VarsFiles\n\
         vars_files: [defaults.yml]\n\
         vars:\n\
         \x20\x20b: inline\n\
         \x20\x20c: inline\n\
         tasks:\n\
         \x20\x20- name: vars_files-only key survives\n\
         \x20\x20\x20\x20assert: \"{{a}} == file\"\n\
         \x20\x20- name: inline overrides vars_files\n\
         \x20\x20\x20\x20assert: \"{{b}} == inline\"\n\
         \x20\x20- name: cli --var overrides inline\n\
         \x20\x20\x20\x20assert: \"{{c}} == cli\"\n",
    )
    .unwrap();

    cmd.args(["play", "playbook.yml", "--var", "c=cli"])
        .assert()
        .success();
}

#[test]
fn start_at_task_skips_earlier_tasks() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: StartAt\n\
         tasks:\n\
         \x20\x20- name: first\n\
         \x20\x20\x20\x20run: exit 1\n\
         \x20\x20- name: second\n\
         \x20\x20\x20\x20run: echo second-ran\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args([
            "--output",
            "json",
            "play",
            "playbook.yml",
            "--start-at-task",
            "second",
        ])
        .assert()
        .success(),
    );
    let value = last_line_json(&out);
    assert_eq!(value["tasks"].as_array().unwrap().len(), 1);
    assert_eq!(value["tasks"][0]["name"], "second");
}

#[test]
fn include_with_vars_overrides_and_restores_between_calls() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("sub.yml"),
        "name: Sub\n\
         tasks:\n\
         \x20\x20- name: register the service seen inside\n\
         \x20\x20\x20\x20run: echo \"{{service}}\"\n\
         \x20\x20\x20\x20register: seen_service\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Parent\n\
         vars:\n\
         \x20\x20service: default-service\n\
         tasks:\n\
         \x20\x20- name: call sub for api\n\
         \x20\x20\x20\x20include:\n\
         \x20\x20\x20\x20\x20\x20file: sub.yml\n\
         \x20\x20\x20\x20\x20\x20vars:\n\
         \x20\x20\x20\x20\x20\x20\x20\x20service: api\n\
         \x20\x20- name: override took effect for api\n\
         \x20\x20\x20\x20assert: \"{{seen_service}} == api\"\n\
         \x20\x20- name: restored after first include\n\
         \x20\x20\x20\x20assert: \"{{service}} == default-service\"\n\
         \x20\x20- name: call sub for web\n\
         \x20\x20\x20\x20include:\n\
         \x20\x20\x20\x20\x20\x20file: sub.yml\n\
         \x20\x20\x20\x20\x20\x20vars:\n\
         \x20\x20\x20\x20\x20\x20\x20\x20service: web\n\
         \x20\x20- name: override took effect for web\n\
         \x20\x20\x20\x20assert: \"{{seen_service}} == web\"\n\
         \x20\x20- name: restored after second include\n\
         \x20\x20\x20\x20assert: \"{{service}} == default-service\"\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["--output", "json", "play", "playbook.yml"])
            .assert()
            .success(),
    );
    let value = last_line_json(&out);
    assert_eq!(value["success"], true);
}

#[test]
fn report_task_writes_a_report_from_inline_data_no_temp_file() {
    // set_fact: seeds a JSON-looking var directly (no scrape:/http: needed) — this test
    // exercises report:'s own Source-building/file-writing path, not the network.
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: ReportTask\n\
         tasks:\n\
         \x20\x20- name: seed data\n\
         \x20\x20\x20\x20set_fact:\n\
         \x20\x20\x20\x20\x20\x20data: '[{\"name\": \"a\", \"count\": 3}, {\"name\": \"b\", \"count\": 7}]'\n\
         \x20\x20- name: build html report\n\
         \x20\x20\x20\x20report:\n\
         \x20\x20\x20\x20\x20\x20format: html\n\
         \x20\x20\x20\x20\x20\x20sources:\n\
         \x20\x20\x20\x20\x20\x20\x20\x20items: \"{{data}}\"\n\
         \x20\x20\x20\x20\x20\x20out: out.html\n\
         \x20\x20\x20\x20register: report_size\n\
         \x20\x20- name: report size is a positive number\n\
         \x20\x20\x20\x20assert: \"{{report_size}} != 0\"\n",
    )
    .unwrap();

    cmd.args(["play", "playbook.yml"]).assert().success();

    let report_path = dir.path().join("out.html");
    assert!(report_path.exists());
    let content = std::fs::read_to_string(&report_path).unwrap();
    assert!(content.starts_with("<!doctype html>"));
    assert!(content.contains("<th>name</th>"));
}

fn stderr_of(assert: assert_cmd::assert::Assert) -> String {
    String::from_utf8_lossy(&assert.get_output().stderr).to_string()
}

#[test]
fn write_file_task_writes_rendered_content_and_appends() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: WriteFile\n\
         vars:\n\
         \x20\x20who: world\n\
         tasks:\n\
         \x20\x20- name: write initial content\n\
         \x20\x20\x20\x20write_file:\n\
         \x20\x20\x20\x20\x20\x20path: out.txt\n\
         \x20\x20\x20\x20\x20\x20content: |\n\
         \x20\x20\x20\x20\x20\x20\x20\x20hello {{who}}\n\
         \x20\x20\x20\x20register: bytes_written\n\
         \x20\x20- name: bytes captured\n\
         \x20\x20\x20\x20assert: \"{{bytes_written}} != 0\"\n\
         \x20\x20- name: append a line\n\
         \x20\x20\x20\x20write_file:\n\
         \x20\x20\x20\x20\x20\x20path: out.txt\n\
         \x20\x20\x20\x20\x20\x20content: \"goodbye\\n\"\n\
         \x20\x20\x20\x20\x20\x20append: true\n",
    )
    .unwrap();

    cmd.args(["play", "playbook.yml"]).assert().success();

    let content = std::fs::read_to_string(dir.path().join("out.txt")).unwrap();
    assert_eq!(content, "hello world\ngoodbye\n");
}

#[test]
fn max_parallel_loop_runs_all_items_and_keeps_last_registered_value_deterministic() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: ParallelLoop\n\
         tasks:\n\
         \x20\x20- name: fan out writes\n\
         \x20\x20\x20\x20loop: [a, b, c, d]\n\
         \x20\x20\x20\x20max_parallel: 2\n\
         \x20\x20\x20\x20write_file:\n\
         \x20\x20\x20\x20\x20\x20path: out.txt\n\
         \x20\x20\x20\x20\x20\x20content: \"{{item}}\\n\"\n\
         \x20\x20\x20\x20\x20\x20append: true\n\
         \x20\x20- name: all four items ran\n\
         \x20\x20\x20\x20run: test $(wc -l < out.txt) -eq 4\n\
         \x20\x20- name: fan out with register\n\
         \x20\x20\x20\x20loop: [a, b, c, d]\n\
         \x20\x20\x20\x20max_parallel: 2\n\
         \x20\x20\x20\x20run: echo {{item}}\n\
         \x20\x20\x20\x20register: last\n\
         \x20\x20- name: last item wins despite concurrency\n\
         \x20\x20\x20\x20assert: \"{{last}} == d\"\n",
    )
    .unwrap();

    cmd.args(["play", "playbook.yml"]).assert().success();
}

#[test]
fn resume_restores_vars_and_continues_after_the_last_completed_task() {
    let dir = tempdir().unwrap();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: ResumeTest\n\
         vars:\n\
         \x20\x20fail_flag: \"yes\"\n\
         tasks:\n\
         \x20\x20- name: seed\n\
         \x20\x20\x20\x20set_fact:\n\
         \x20\x20\x20\x20\x20\x20x: \"1\"\n\
         \x20\x20- name: gate\n\
         \x20\x20\x20\x20assert: \"{{fail_flag}} != yes\"\n\
         \x20\x20- name: finish\n\
         \x20\x20\x20\x20set_fact:\n\
         \x20\x20\x20\x20\x20\x20done: \"true\"\n",
    )
    .unwrap();

    // First run fails at "gate" (fail_flag defaults to "yes").
    let out = stdout_of(
        tooler_in(dir.path())
            .args(["--output", "json", "play", "playbook.yml"])
            .assert()
            .failure(),
    );
    let value = last_line_json(&out);
    assert_eq!(value["success"], false);

    let state_path = dir.path().join("playbook.yml.state.json");
    assert!(state_path.exists(), "checkpoint should be left behind");
    let checkpoint: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&state_path).unwrap()).unwrap();
    assert_eq!(checkpoint["last_completed_task"], "seed");
    assert_eq!(checkpoint["vars"]["x"], "1");

    // Resume, fixing fail_flag via --var — should continue right after "seed" and succeed.
    let out = stdout_of(
        tooler_in(dir.path())
            .args([
                "--output",
                "json",
                "play",
                "playbook.yml",
                "--resume",
                "--var",
                "fail_flag=no",
            ])
            .assert()
            .success(),
    );
    let value = last_line_json(&out);
    assert_eq!(value["success"], true);
    let tasks = value["tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), 2);
    assert_eq!(tasks[0]["name"], "gate");
    assert_eq!(tasks[1]["name"], "finish");

    // A fully-completed playbook has nothing left to resume.
    assert!(!state_path.exists());
}

#[test]
fn resume_without_a_checkpoint_fails_clearly() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: NoCheckpoint\ntasks:\n  - name: t1\n    debug: hi\n",
    )
    .unwrap();

    let err = stderr_of(
        cmd.args(["play", "playbook.yml", "--resume"])
            .assert()
            .failure(),
    );
    assert!(
        err.to_lowercase().contains("no checkpoint found"),
        "stderr was: {err}"
    );
}

#[test]
fn resume_and_start_at_task_are_mutually_exclusive() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Conflict\ntasks:\n  - name: t1\n    debug: hi\n",
    )
    .unwrap();

    let err = stderr_of(
        cmd.args(["play", "playbook.yml", "--resume", "--start-at-task", "t1"])
            .assert()
            .failure(),
    );
    assert!(
        err.to_lowercase().contains("mutually exclusive"),
        "stderr was: {err}"
    );
}

#[test]
fn repl_executes_lines_tracks_vars_and_saves_a_session() {
    let (mut cmd, dir) = tooler();
    let out = stdout_of(
        cmd.args(["play", "--repl"])
            .write_stdin(
                "run: echo hi\n\
                 set_fact: {x: \"1\"}\n\
                 .vars\n\
                 .save session.yml\n\
                 .exit\n",
            )
            .assert()
            .success(),
    );
    assert!(out.contains("x = 1"), "stdout was: {out}");
    assert!(out.contains("saved 2 task(s)"), "stdout was: {out}");

    let saved = std::fs::read_to_string(dir.path().join("session.yml")).unwrap();
    assert!(saved.contains("name: REPL session"));
    assert!(saved.contains("run: echo hi"));
    assert!(saved.contains("x: '1'") || saved.contains("x: \"1\""));
}

#[test]
fn repl_survives_an_invalid_line_and_keeps_taking_input() {
    let (mut cmd, _dir) = tooler();
    let out = stdout_of(
        cmd.args(["play", "--repl"])
            .write_stdin(
                "this is not: valid: yaml: at all\n\
                 run: echo still-alive\n\
                 .exit\n",
            )
            .assert()
            .success(),
    );
    assert!(out.contains("invalid YAML"), "stdout was: {out}");
    assert!(out.contains("still-alive"), "stdout was: {out}");
}

#[test]
fn repl_save_excludes_a_hard_failure_but_keeps_an_ignored_one() {
    let (mut cmd, dir) = tooler();
    cmd.args(["play", "--repl"])
        .write_stdin(
            "run: echo kept\n\
             notanaction: oops\n\
             {run: \"exit 1\", ignore_errors: true}\n\
             .save session.yml\n\
             .exit\n",
        )
        .assert()
        .success();

    let saved = std::fs::read_to_string(dir.path().join("session.yml")).unwrap();
    assert!(saved.contains("echo kept"));
    assert!(saved.contains("ignore_errors: true"));
    assert!(!saved.contains("notanaction"));
}

#[test]
fn repl_saved_session_is_a_real_playbook_that_tooler_play_can_run() {
    let dir = tempdir().unwrap();
    tooler_in(dir.path())
        .args(["play", "--repl"])
        .write_stdin("run: echo roundtrip\n.save out.yml\n.exit\n")
        .assert()
        .success();

    let saved_path = dir.path().join("out.yml");
    assert!(saved_path.exists());

    let out = stdout_of(
        tooler_in(dir.path())
            .args(["play", "out.yml"])
            .assert()
            .success(),
    );
    assert!(out.contains("roundtrip"), "stdout was: {out}");
}

#[test]
fn mail_task_against_an_unconfigured_profile_fails_clearly() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Mail\ntasks:\n  - name: notify\n    mail:\n      server: ghost\n      \
         to: a@example.com\n      subject: hi\n      body: hello\n",
    )
    .unwrap();

    // Task-level failures print their detail to stdout (execute_playbook's own recap),
    // then bail generically for the exit code -- see `resolve_mail_creds_errors_on_unknown_profile`
    // for the same message asserted directly against the function's `Result`.
    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().failure());
    assert!(
        out.contains("No mail profile 'ghost' configured"),
        "stdout was: {out}"
    );
}

#[test]
fn mail_task_dry_run_previews_without_connecting() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Mail\ntasks:\n  - name: notify\n    mail:\n      server: ghost\n      \
         to: a@example.com\n      subject: dry subject\n      body: hello\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["play", "playbook.yml", "--dry"])
            .assert()
            .success(),
    );
    // The preview line renders even though `ghost` isn't a configured profile — a dry
    // run never resolves credentials or connects, so this can't fail on the missing
    // profile the way a real run does (see `mail_task_against_an_unconfigured_profile_fails_clearly`).
    assert!(out.contains("mail to"), "stdout was: {out}");
    assert!(out.contains("dry subject"), "stdout was: {out}");
}

#[test]
fn write_file_task_rejects_a_path_that_escapes_the_playbook_directory() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Escape\ntasks:\n  - name: t\n    write_file:\n      \
         path: \"../outside.txt\"\n      content: \"pwned\"\n",
    )
    .unwrap();

    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().failure());
    assert!(
        out.contains("escapes the playbook directory"),
        "stdout was: {out}"
    );
    assert!(!dir.path().parent().unwrap().join("outside.txt").exists());
}

#[test]
fn db_exec_without_confirm_fails_clearly_and_makes_no_connection() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Exec\ntasks:\n  - name: mark processed\n    db_exec:\n      \
         server: ghost\n      sql: \"UPDATE t SET x=1\"\n",
    )
    .unwrap();

    // Fails on the missing `confirm: true` before ever resolving `server` (an
    // unconfigured profile that would also fail, just with a different message) --
    // proves the gate is checked first, not as an afterthought.
    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().failure());
    assert!(
        out.contains("refused to run without confirm: true"),
        "stdout was: {out}"
    );
}

#[test]
fn db_exec_dry_run_previews_without_connecting() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Exec\ntasks:\n  - name: mark processed\n    db_exec:\n      \
         server: ghost\n      sql: \"UPDATE t SET x=1\"\n      confirm: true\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["play", "playbook.yml", "--dry"])
            .assert()
            .success(),
    );
    assert!(out.contains("UPDATE t SET x=1"), "stdout was: {out}");
}

#[test]
fn db_exec_against_an_unconfigured_server_fails_clearly() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Exec\ntasks:\n  - name: mark processed\n    db_exec:\n      \
         server: ghost\n      sql: \"UPDATE t SET x=1\"\n      confirm: true\n",
    )
    .unwrap();

    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().failure());
    assert!(out.contains("ghost"), "stdout was: {out}");
}

#[test]
fn mail_check_against_an_unconfigured_profile_fails_clearly() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Check\ntasks:\n  - name: check inbox\n    mail_check:\n      server: ghost\n",
    )
    .unwrap();

    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().failure());
    assert!(
        out.contains("No mail profile 'ghost' configured"),
        "stdout was: {out}"
    );
}

#[test]
fn mail_check_dry_run_previews_without_connecting() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Check\ntasks:\n  - name: check inbox\n    mail_check:\n      server: ghost\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["play", "playbook.yml", "--dry"])
            .assert()
            .success(),
    );
    // The preview line renders even though `ghost` isn't a configured profile — a dry
    // run never resolves credentials or connects (mirrors mail_task_dry_run_previews_
    // without_connecting's same reasoning).
    assert!(out.contains("checking"), "stdout was: {out}");
}

#[test]
fn read_csv_task_parses_a_real_csv_and_registers_rows() {
    let (mut cmd, dir) = tooler();
    std::fs::write(dir.path().join("data.csv"), "name,age\nAlice,30\nBob,25\n").unwrap();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: CSV\ntasks:\n  - name: read it\n    read_csv:\n      path: data.csv\n    \
         register: rows\n  - name: show first row\n    debug: \"{{rows}}\"\n",
    )
    .unwrap();

    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().success());
    assert!(out.contains("2 row(s)"), "stdout was: {out}");
    assert!(out.contains("\"name\":\"Alice\""), "stdout was: {out}");
    assert!(out.contains("\"age\":\"30\""), "stdout was: {out}");
}

#[test]
fn read_csv_task_rejects_a_path_that_escapes_the_playbook_directory() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Escape\ntasks:\n  - name: t\n    read_csv:\n      path: \"../outside.csv\"\n",
    )
    .unwrap();

    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().failure());
    assert!(
        out.contains("escapes the playbook directory"),
        "stdout was: {out}"
    );
}

#[test]
fn state_set_persists_across_separate_tooler_play_invocations() {
    let dir = tempdir().unwrap();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: State\ntasks:\n  - name: set it\n    state_set:\n      last_uid: \"42\"\n",
    )
    .unwrap();

    tooler_in(dir.path())
        .args(["play", "playbook.yml"])
        .assert()
        .success();

    let data_path = dir.path().join("playbook.yml.data.json");
    assert!(
        data_path.exists(),
        "expected {} to exist",
        data_path.display()
    );
    let saved = std::fs::read_to_string(&data_path).unwrap();
    assert!(saved.contains("42"), "saved state was: {saved}");

    // A second, separate invocation against the same playbook file reads the value
    // back via {{state.*}} -- the actual point of the feature: a cross-process
    // round-trip, not just in-memory persistence within one run.
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: State\ntasks:\n  - name: read it back\n    debug: \"got {{state.last_uid}}\"\n",
    )
    .unwrap();
    let out = stdout_of(
        tooler_in(dir.path())
            .args(["play", "playbook.yml"])
            .assert()
            .success(),
    );
    assert!(out.contains("got 42"), "stdout was: {out}");
}

#[test]
fn state_set_does_not_persist_in_a_dry_run() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: State\ntasks:\n  - name: set it\n    state_set:\n      x: \"1\"\n",
    )
    .unwrap();

    cmd.args(["play", "playbook.yml", "--dry"])
        .assert()
        .success();

    assert!(!dir.path().join("playbook.yml.data.json").exists());
}

#[test]
fn wait_for_file_exists_unblocks_once_the_file_appears() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Wait\ntasks:\n  - name: wait for the flag\n    wait_for:\n      \
         file_exists: flag.txt\n      interval: 1\n      timeout: 10\n",
    )
    .unwrap();

    let flag_path = dir.path().join("flag.txt");
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(500));
        std::fs::write(flag_path, "go").unwrap();
    });

    cmd.args(["play", "playbook.yml"])
        .timeout(std::time::Duration::from_secs(15))
        .assert()
        .success();
}

#[test]
fn wait_for_file_absent_times_out_while_the_file_still_exists() {
    let (mut cmd, dir) = tooler();
    std::fs::write(dir.path().join("lock.txt"), "held").unwrap();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Wait\ntasks:\n  - name: wait for the lock to clear\n    wait_for:\n      \
         file_absent: lock.txt\n      interval: 1\n      timeout: 2\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["play", "playbook.yml"])
            .timeout(std::time::Duration::from_secs(10))
            .assert()
            .failure(),
    );
    assert!(out.contains("timed out"), "stdout was: {out}");
}
