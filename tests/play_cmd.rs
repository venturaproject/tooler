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

/// Spawns a one-shot local HTTP server that replies with `body` verbatim (arbitrary
/// bytes, not necessarily valid UTF-8) and returns the port it's listening on. Used by
/// the `http: {download: ...}` tests to prove the response is written byte-for-byte,
/// unlike the string-capturing `http_request` path.
fn serve_once_with_bytes(body: Vec<u8>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        use std::io::{Read, Write};
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(header.as_bytes());
            let _ = stream.write_all(&body);
        }
    });
    port
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
fn when_with_a_numeric_comparison_correctly_skips_below_threshold() {
    // Regression test for a live-demonstrated bug: when: "{{count}} > 5" used to always
    // evaluate true (any non-empty rendered string fell through to the truthy branch),
    // running the task even though count is only 3.
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Threshold\ntasks:\n  - name: set a small count\n    set_fact:\n      \
         count: \"3\"\n  - name: only if over threshold\n    when: \"{{count}} > 5\"\n    \
         debug: \"should not print\"\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["--output", "json", "play", "playbook.yml"])
            .assert()
            .success(),
    );
    let value = last_line_json(&out);
    assert_eq!(value["tasks"][1]["status"], "skipped");
}

#[test]
fn assert_with_a_greater_or_equal_comparison_passes() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Assert\ntasks:\n  - name: set n\n    set_fact:\n      n: \"3\"\n  - \
         name: check threshold\n    assert: \"{{n}} >= 3\"\n",
    )
    .unwrap();

    cmd.args(["play", "playbook.yml"]).assert().success();
}

#[test]
fn json_length_filter_counts_a_registered_array() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Length\ntasks:\n  - name: seed an array\n    set_fact:\n      \
         arr: '[1,2,3,4]'\n  - name: show its length\n    debug: \"{{arr | json:length}}\"\n",
    )
    .unwrap();

    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().success());
    assert!(out.lines().any(|l| l.trim() == "ℹ 4"), "stdout was: {out}");
}

#[test]
fn quote_filter_prevents_shell_injection_from_an_untrusted_value() {
    // Regression test for the shell-injection gap `| quote` closes: run: shells out the
    // fully rendered command via `sh -c`, so a {{var}} sourced from untrusted data (here
    // simulated with set_fact:, standing in for scrape:/http:/db_query: output) could
    // inject a second command if interpolated unquoted. `| quote` wraps it as one safe
    // argument instead.
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Quote\ntasks:\n  - name: seed a hostile value\n    set_fact:\n      \
         item: \"; touch pwned.txt\"\n  - name: echo it safely\n    \
         run: \"echo {{item | quote}}\"\n    register: out\n  - name: show what ran\n    \
         debug: \"{{out}}\"\n",
    )
    .unwrap();

    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().success());

    // The injected command never ran...
    assert!(
        !dir.path().join("pwned.txt").exists(),
        "the quoted value was interpreted by the shell instead of staying one argument"
    );
    // ...and the literal string (semicolon included) reached echo intact.
    assert!(out.contains("; touch pwned.txt"), "stdout was: {out}");
}

/// Parses `path` as JSON-lines (one `serde_json::Value` per non-empty line) — the shape
/// `--audit-log` writes.
fn read_jsonl(path: &std::path::Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn audit_log_records_one_json_line_per_task_with_status_and_duration() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Audited\ntasks:\n  - name: say hi\n    run: echo hi\n",
    )
    .unwrap();
    let audit_path = dir.path().join("audit.jsonl");

    cmd.args([
        "play",
        "playbook.yml",
        "--audit-log",
        audit_path.to_str().unwrap(),
    ])
    .assert()
    .success();

    let entries = read_jsonl(&audit_path);
    assert_eq!(entries.len(), 1, "entries were: {entries:?}");
    assert_eq!(entries[0]["playbook"], "Audited");
    assert_eq!(entries[0]["task"], "say hi");
    assert_eq!(entries[0]["action"], "run");
    assert_eq!(entries[0]["status"], "ok");
    assert!(entries[0]["duration_ms"].is_number());
    assert!(entries[0]["error"].is_null());
    assert!(entries[0]["ts"].is_string());
}

#[test]
fn audit_log_records_a_failed_task_with_its_error_message() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Audited\ntasks:\n  - name: boom\n    run: exit 1\n",
    )
    .unwrap();
    let audit_path = dir.path().join("audit.jsonl");

    cmd.args([
        "play",
        "playbook.yml",
        "--audit-log",
        audit_path.to_str().unwrap(),
    ])
    .assert()
    .failure();

    let entries = read_jsonl(&audit_path);
    assert_eq!(entries.len(), 1, "entries were: {entries:?}");
    assert_eq!(entries[0]["status"], "failed");
    assert!(
        entries[0]["error"].as_str().is_some(),
        "entries were: {entries:?}"
    );
}

#[test]
fn audit_log_records_one_line_per_loop_iteration() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Audited\ntasks:\n  - name: greet each\n    loop: [a, b, c]\n    \
         run: \"echo {{item}}\"\n",
    )
    .unwrap();
    let audit_path = dir.path().join("audit.jsonl");

    cmd.args([
        "play",
        "playbook.yml",
        "--audit-log",
        audit_path.to_str().unwrap(),
    ])
    .assert()
    .success();

    let entries = read_jsonl(&audit_path);
    assert_eq!(entries.len(), 3, "entries were: {entries:?}");
    assert!(entries.iter().all(|e| e["status"] == "ok"));
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
fn until_retries_a_successful_task_until_its_registered_value_satisfies_the_condition() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Until test\ntasks:\n  - name: poll until three\n    \
         run: \"printf x >> counter && wc -c < counter\"\n    register: n\n    \
         until: \"{{n}} == 3\"\n    retries: 5\n    delay: 0\n",
    )
    .unwrap();

    cmd.args(["play", "playbook.yml"]).assert().success();
    let len = std::fs::metadata(dir.path().join("counter")).unwrap().len();
    assert_eq!(
        len, 3,
        "task should have stopped exactly at the 3rd attempt"
    );
}

#[test]
fn until_fails_the_task_once_retries_are_exhausted_without_satisfying_the_condition() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Until exhaustion\ntasks:\n  - name: never satisfied\n    \
         run: echo hi\n    register: out\n    until: \"{{out}} == impossible\"\n    \
         retries: 2\n    delay: 0\n",
    )
    .unwrap();

    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().failure());
    assert!(
        out.contains("was still false after 3 attempt(s)"),
        "stdout was: {out}"
    );
}

#[test]
fn until_is_ignored_in_a_dry_run() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Until dry\ntasks:\n  - name: poll\n    run: echo hi\n    register: n\n    \
         until: \"{{n}} == never\"\n    retries: 2\n    delay: 0\n",
    )
    .unwrap();

    cmd.args(["play", "playbook.yml", "--dry"])
        .assert()
        .success();
}

#[test]
fn skip_tags_excludes_matching_tasks() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: SkipTags\ntasks:\n  - name: build step\n    tags: [build]\n    \
         run: echo build\n  - name: deploy step\n    tags: [deploy]\n    \
         run: touch deployed\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["play", "playbook.yml", "--skip-tags", "deploy"])
            .assert()
            .success(),
    );
    assert!(out.contains("build step"), "stdout was: {out}");
    assert!(!out.contains("deploy step"), "stdout was: {out}");
    assert!(!dir.path().join("deployed").exists());
}

#[test]
fn tags_and_skip_tags_together_skip_wins_on_overlap() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: TagsAndSkip\ntasks:\n  - name: build step\n    tags: [build]\n    \
         run: echo build\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args([
            "play",
            "playbook.yml",
            "--tags",
            "build",
            "--skip-tags",
            "build",
        ])
        .assert()
        .success(),
    );
    assert!(!out.contains("build step"), "stdout was: {out}");
}

#[test]
fn list_tasks_shows_the_tree_without_running_or_connecting_anything() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: ListMe\ntasks:\n  - name: outer block\n    block:\n      - name: inner db\n        \
         db_exec:\n          server: ghost\n          sql: \"DELETE FROM x\"\n          \
         confirm: true\n    rescue:\n      - name: cleanup\n        debug: oops\n  - name: pull in sub\n    \
         include: sub.yml\n    tags: [deploy]\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["--output", "json", "play", "playbook.yml", "--list-tasks"])
            .assert()
            .success(),
    );
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(value["playbook"], "ListMe");
    assert_eq!(value["tasks"][0]["name"], "outer block");
    assert_eq!(value["tasks"][0]["block"][0]["name"], "inner db");
    assert_eq!(value["tasks"][0]["block"][0]["action"], "db_exec");
    assert_eq!(value["tasks"][0]["rescue"][0]["name"], "cleanup");
    assert_eq!(value["tasks"][1]["name"], "pull in sub");
    assert_eq!(value["tasks"][1]["include"], "sub.yml");
    assert_eq!(value["tasks"][1]["tags"][0], "deploy");
    // No sub.yml exists on disk and the db_exec: task targets an unconfigured server --
    // both would error immediately if --list-tasks executed anything.
}

#[test]
fn list_tasks_respects_skip_tags() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: ListFiltered\ntasks:\n  - name: build step\n    tags: [build]\n    \
         run: echo build\n  - name: deploy step\n    tags: [deploy]\n    run: echo deploy\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args([
            "--output",
            "json",
            "play",
            "playbook.yml",
            "--list-tasks",
            "--skip-tags",
            "deploy",
        ])
        .assert()
        .success(),
    );
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    let names: Vec<&str> = value["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["build step"]);
}

#[test]
fn list_tags_prints_a_sorted_deduplicated_tag_list() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Tags\ntasks:\n  - name: a\n    tags: [zeta, build]\n    run: echo a\n  - \
         name: b\n    block:\n      - name: c\n        tags: [build]\n        run: echo c\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["--output", "json", "play", "playbook.yml", "--list-tags"])
            .assert()
            .success(),
    );
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(value["tags"], serde_json::json!(["build", "zeta"]));
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
fn loop_register_results_captures_every_iteration_in_order() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: LoopResults\ntasks:\n  - name: echo each\n    loop: [a, b, c]\n    \
         run: echo {{item}}\n    register: r\n  - name: check length\n    \
         assert: \"{{r.results | json:length}} == 3\"\n  - name: check second item\n    \
         assert: \"{{r.results | json:[1]}} == b\"\n  - name: last value still wins on r itself\n    \
         assert: \"{{r}} == c\"\n",
    )
    .unwrap();

    cmd.args(["play", "playbook.yml"]).assert().success();
}

#[test]
fn max_parallel_loop_register_results_captures_every_iteration_in_order() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: ParallelLoopResults\ntasks:\n  - name: echo each\n    loop: [a, b, c, d]\n    \
         max_parallel: 2\n    run: echo {{item}}\n    register: r\n  - name: check length\n    \
         assert: \"{{r.results | json:length}} == 4\"\n  - name: order preserved despite concurrency\n    \
         assert: \"{{r.results | json:[2]}} == c\"\n",
    )
    .unwrap();

    cmd.args(["play", "playbook.yml"]).assert().success();
}

#[test]
fn vars_file_loads_a_flat_yaml_file_and_makes_vars_usable() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("extra.yml"),
        "host: prod.example.com\nport: \"8080\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: VarsFile\ntasks:\n  - name: check host\n    assert: \"{{host}} == prod.example.com\"\n",
    )
    .unwrap();

    cmd.args(["play", "playbook.yml", "--vars-file", "extra.yml"])
        .assert()
        .success();
}

#[test]
fn var_flag_overrides_a_vars_file_value_on_the_same_key() {
    let (mut cmd, dir) = tooler();
    std::fs::write(dir.path().join("extra.yml"), "host: from-file\n").unwrap();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: VarsFileOverride\ntasks:\n  - name: check host\n    assert: \"{{host}} == from-cli\"\n",
    )
    .unwrap();

    cmd.args([
        "play",
        "playbook.yml",
        "--vars-file",
        "extra.yml",
        "--var",
        "host=from-cli",
    ])
    .assert()
    .success();
}

#[test]
fn later_vars_file_overrides_an_earlier_one() {
    let (mut cmd, dir) = tooler();
    std::fs::write(dir.path().join("a.yml"), "host: from-a\n").unwrap();
    std::fs::write(dir.path().join("b.yml"), "host: from-b\n").unwrap();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: TwoVarsFiles\ntasks:\n  - name: check host\n    assert: \"{{host}} == from-b\"\n",
    )
    .unwrap();

    cmd.args([
        "play",
        "playbook.yml",
        "--vars-file",
        "a.yml",
        "--vars-file",
        "b.yml",
    ])
    .assert()
    .success();
}

#[test]
fn fleet_batch_size_still_attempts_every_target() {
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
        "name: BatchedFleet\ntasks:\n  - name: rolling restart\n    \
         fleet:\n      servers: s1,s2\n      command: echo hi\n      parallel: true\n      \
         batch_size: 1\n    ignore_errors: true\n",
    )
    .unwrap();

    // Same shape as fleet_task_parallel_flag_runs_without_hanging_or_panicking: unreachable
    // servers make the fleet: task itself fail, but ignore_errors keeps the playbook going.
    // The point is proving batch_size: 1 still attempts (and chunks through) every target
    // rather than stopping early, and completes without hanging.
    cmd.args(["--output", "json", "play", "playbook.yml"])
        .timeout(std::time::Duration::from_secs(15))
        .assert()
        .success();
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
fn mail_task_with_a_missing_attachment_fails_before_connecting() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Mail\ntasks:\n  - name: notify\n    mail:\n      \
         host: 127.0.0.1\n      port: 1\n      user: u\n      password: p\n      \
         to: a@example.com\n      subject: hi\n      body: hello\n      \
         attachments:\n        - missing.pdf\n",
    )
    .unwrap();

    // Attachments are read (and must exist) before send_mail ever opens the SMTP
    // transport -- port 1 would fail to connect too, but that's not what fails here.
    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().failure());
    assert!(out.contains("reading attachment"), "stdout was: {out}");
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
fn http_download_saves_binary_response_bytes_and_register_captures_the_path() {
    let (mut cmd, dir) = tooler();
    let body: Vec<u8> = vec![0x25, 0x50, 0x44, 0x46, 0x00, 0xFF, 0x10, 0x0A];
    let port = serve_once_with_bytes(body.clone());
    std::fs::write(
        dir.path().join("playbook.yml"),
        format!(
            "name: Download\ntasks:\n  - name: fetch\n    http:\n      \
             url: http://127.0.0.1:{port}/f.bin\n      download: out/f.bin\n    \
             register: saved\n  - name: show\n    debug: \"{{{{saved}}}}\"\n"
        ),
    )
    .unwrap();

    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().success());
    let saved_path = dir.path().join("out/f.bin");
    assert!(saved_path.exists());
    assert_eq!(std::fs::read(&saved_path).unwrap(), body);
    assert!(out.contains("out/f.bin"), "stdout was: {out}");
}

#[test]
fn http_download_rejects_a_path_that_escapes_the_playbook_directory() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Escape\ntasks:\n  - name: t\n    http:\n      \
         url: https://example.com\n      download: \"../outside.bin\"\n",
    )
    .unwrap();

    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().failure());
    assert!(
        out.contains("escapes the playbook directory"),
        "stdout was: {out}"
    );
    assert!(!dir.path().parent().unwrap().join("outside.bin").exists());
}

#[test]
fn http_download_path_chains_into_mail_attachments_without_absolute_path_rejection() {
    // `register:` on `download:` must capture the rendered *relative* path, not the
    // resolved absolute one -- `join_confined` (used by both `http: {download}` and
    // `mail: {attachments}`) rejects absolute paths outright, so an absolute registered
    // value would break exactly the "download then attach" chain this feature exists for.
    let (mut cmd, dir) = tooler();
    let port = serve_once_with_bytes(b"hello attachment".to_vec());
    std::fs::write(
        dir.path().join("playbook.yml"),
        format!(
            "name: Chain\ntasks:\n  - name: fetch\n    http:\n      \
             url: http://127.0.0.1:{port}/f.txt\n      download: out/f.txt\n    \
             register: saved\n  - name: mail it\n    mail:\n      \
             host: 127.0.0.1\n      port: 1\n      user: u\n      password: p\n      \
             to: a@example.com\n      subject: hi\n      body: hello\n      \
             attachments:\n        - \"{{{{saved}}}}\"\n"
        ),
    )
    .unwrap();

    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().failure());
    assert!(
        !out.contains("must be relative to the playbook directory"),
        "stdout was: {out}"
    );
    assert!(!out.contains("reading attachment"), "stdout was: {out}");
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
fn fs_cat_dry_run_previews_without_connecting() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Cat\ntasks:\n  - name: read it\n    fs_cat:\n      \
         server: ghost\n      path: /etc/app/.env\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["play", "playbook.yml", "--dry"])
            .assert()
            .success(),
    );
    assert!(out.contains("/etc/app/.env"), "stdout was: {out}");
}

#[test]
fn fs_cat_against_an_unconfigured_server_fails_clearly() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Cat\ntasks:\n  - name: read it\n    fs_cat:\n      \
         server: ghost\n      path: /etc/app/.env\n",
    )
    .unwrap();

    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().failure());
    assert!(out.contains("ghost"), "stdout was: {out}");
}

#[test]
fn fs_write_without_confirm_fails_clearly_and_makes_no_connection() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Write\ntasks:\n  - name: overwrite it\n    fs_write:\n      \
         server: ghost\n      path: /etc/app/.env\n      content: FOO=1\n",
    )
    .unwrap();

    // Fails on the missing `confirm: true` before ever resolving `server` -- same gate
    // db_exec: uses, proven the same way.
    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().failure());
    assert!(
        out.contains("refused to run without confirm: true"),
        "stdout was: {out}"
    );
}

#[test]
fn fs_write_dry_run_previews_without_connecting() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Write\ntasks:\n  - name: overwrite it\n    fs_write:\n      \
         server: ghost\n      path: /etc/app/.env\n      content: FOO=1\n      confirm: true\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["play", "playbook.yml", "--dry"])
            .assert()
            .success(),
    );
    assert!(out.contains("/etc/app/.env"), "stdout was: {out}");
}

#[test]
fn fs_write_against_an_unconfigured_server_fails_clearly() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Write\ntasks:\n  - name: overwrite it\n    fs_write:\n      \
         server: ghost\n      path: /etc/app/.env\n      content: FOO=1\n      confirm: true\n",
    )
    .unwrap();

    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().failure());
    assert!(out.contains("ghost"), "stdout was: {out}");
}

#[test]
fn systemd_restart_dry_run_previews_without_connecting() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Restart\ntasks:\n  - name: bounce it\n    systemd_restart:\n      \
         server: ghost\n      unit: nginx\n      confirm: true\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["play", "playbook.yml", "--dry"])
            .assert()
            .success(),
    );
    assert!(out.contains("nginx"), "stdout was: {out}");
}

#[test]
fn systemd_restart_without_confirm_fails_clearly_and_makes_no_connection() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Restart\ntasks:\n  - name: bounce it\n    systemd_restart:\n      \
         server: ghost\n      unit: nginx\n",
    )
    .unwrap();

    // Fails on the missing `confirm: true` before ever resolving `server` -- same gate
    // fs_write:/ps_kill:/db_exec: use, proven the same way.
    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().failure());
    assert!(
        out.contains("refused to run without confirm: true"),
        "stdout was: {out}"
    );
}

#[test]
fn systemd_restart_against_an_unconfigured_server_fails_clearly() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Restart\ntasks:\n  - name: bounce it\n    systemd_restart:\n      \
         server: ghost\n      unit: nginx\n      confirm: true\n",
    )
    .unwrap();

    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().failure());
    assert!(out.contains("ghost"), "stdout was: {out}");
}

#[test]
fn systemd_status_dry_run_previews_without_connecting() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Status\ntasks:\n  - name: check it\n    systemd_status:\n      \
         server: ghost\n      unit: nginx\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["play", "playbook.yml", "--dry"])
            .assert()
            .success(),
    );
    assert!(out.contains("nginx"), "stdout was: {out}");
}

#[test]
fn systemd_status_against_an_unconfigured_server_fails_clearly() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Status\ntasks:\n  - name: check it\n    systemd_status:\n      \
         server: ghost\n      unit: nginx\n",
    )
    .unwrap();

    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().failure());
    assert!(out.contains("ghost"), "stdout was: {out}");
}

#[test]
fn logs_tail_dry_run_previews_without_connecting() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Tail\ntasks:\n  - name: tail it\n    logs_tail:\n      \
         server: ghost\n      path: /var/log/app.log\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["play", "playbook.yml", "--dry"])
            .assert()
            .success(),
    );
    assert!(out.contains("/var/log/app.log"), "stdout was: {out}");
}

#[test]
fn logs_tail_against_an_unconfigured_server_fails_clearly() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Tail\ntasks:\n  - name: tail it\n    logs_tail:\n      \
         server: ghost\n      path: /var/log/app.log\n",
    )
    .unwrap();

    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().failure());
    assert!(out.contains("ghost"), "stdout was: {out}");
}

#[test]
fn logs_grep_dry_run_previews_without_connecting() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Grep\ntasks:\n  - name: search it\n    logs_grep:\n      \
         server: ghost\n      path: /var/log/app.log\n      pattern: ERROR\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["play", "playbook.yml", "--dry"])
            .assert()
            .success(),
    );
    assert!(out.contains("/var/log/app.log"), "stdout was: {out}");
}

#[test]
fn logs_grep_against_an_unconfigured_server_fails_clearly() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Grep\ntasks:\n  - name: search it\n    logs_grep:\n      \
         server: ghost\n      path: /var/log/app.log\n      pattern: ERROR\n",
    )
    .unwrap();

    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().failure());
    assert!(out.contains("ghost"), "stdout was: {out}");
}

#[test]
fn ps_list_dry_run_previews_without_connecting() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: PsList\ntasks:\n  - name: list them\n    ps_list:\n      server: ghost\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["play", "playbook.yml", "--dry"])
            .assert()
            .success(),
    );
    assert!(out.contains("processes"), "stdout was: {out}");
}

#[test]
fn ps_list_against_an_unconfigured_server_fails_clearly() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: PsList\ntasks:\n  - name: list them\n    ps_list:\n      server: ghost\n",
    )
    .unwrap();

    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().failure());
    assert!(out.contains("ghost"), "stdout was: {out}");
}

#[test]
fn ps_kill_without_confirm_fails_clearly_and_makes_no_connection() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: PsKill\ntasks:\n  - name: kill it\n    ps_kill:\n      \
         server: ghost\n      pid: 1234\n",
    )
    .unwrap();

    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().failure());
    assert!(
        out.contains("refused to run without confirm: true"),
        "stdout was: {out}"
    );
}

#[test]
fn ps_kill_dry_run_previews_without_connecting() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: PsKill\ntasks:\n  - name: kill it\n    ps_kill:\n      \
         server: ghost\n      pid: 1234\n      confirm: true\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["play", "playbook.yml", "--dry"])
            .assert()
            .success(),
    );
    assert!(out.contains("1234"), "stdout was: {out}");
}

#[test]
fn ps_kill_against_an_unconfigured_server_fails_clearly() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: PsKill\ntasks:\n  - name: kill it\n    ps_kill:\n      \
         server: ghost\n      pid: 1234\n      confirm: true\n",
    )
    .unwrap();

    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().failure());
    assert!(out.contains("ghost"), "stdout was: {out}");
}

#[test]
fn stat_dry_run_previews_without_connecting() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Stat\ntasks:\n  - name: check it\n    stat:\n      server: ghost\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["play", "playbook.yml", "--dry"])
            .assert()
            .success(),
    );
    assert!(out.contains("stat"), "stdout was: {out}");
}

#[test]
fn stat_against_an_unconfigured_server_fails_clearly() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Stat\ntasks:\n  - name: check it\n    stat:\n      server: ghost\n",
    )
    .unwrap();

    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().failure());
    assert!(out.contains("ghost"), "stdout was: {out}");
}

/// Initializes a real git repo with a pinned, deterministic branch name and two commits
/// (one categorizable as a feature), mirroring `tests/git_cmd.rs`'s own `init_repo`
/// helper exactly, so `git_summary:`/`git_changelog:` have real data to report.
fn init_repo(dir: &std::path::Path) {
    let git = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    };
    git(&["init", "-b", "main"]);
    git(&["config", "user.email", "test@example.com"]);
    git(&["config", "user.name", "Test"]);
    std::fs::write(dir.join("README.md"), "hello\n").unwrap();
    git(&["add", "README.md"]);
    git(&["commit", "-m", "initial commit"]);
    std::fs::write(dir.join("feature.md"), "feature\n").unwrap();
    git(&["add", "feature.md"]);
    git(&["commit", "-m", "feat: add a feature"]);
}

#[test]
fn git_summary_task_registers_the_playbooks_own_repo_summary() {
    let (mut cmd, dir) = tooler();
    init_repo(dir.path());
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Summary\ntasks:\n  - name: summarize\n    git_summary: {}\n    \
         register: s\n  - name: show\n    debug: \"{{s}}\"\n",
    )
    .unwrap();

    // The playbook.yml file itself is untracked in the freshly-init'd repo, so `clean`
    // is always false here -- assert on branch/recent instead, which don't depend on
    // that incidental detail.
    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().success());
    assert!(out.contains("\"branch\":\"main\""), "stdout was: {out}");
    assert!(out.contains("add a feature"), "stdout was: {out}");
}

#[test]
fn git_changelog_task_registers_categorized_commits() {
    let (mut cmd, dir) = tooler();
    init_repo(dir.path());
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Changelog\ntasks:\n  - name: changelog\n    git_changelog: {}\n    \
         register: c\n  - name: show\n    debug: \"{{c}}\"\n",
    )
    .unwrap();

    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().success());
    assert!(out.contains("add a feature"), "stdout was: {out}");
    assert!(out.contains("\"features\":"), "stdout was: {out}");
}

#[test]
fn gh_prs_dry_run_previews_without_connecting() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Prs\ntasks:\n  - name: list them\n    gh_prs: {}\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["play", "playbook.yml", "--dry"])
            .assert()
            .success(),
    );
    assert!(out.contains("gh pr list"), "stdout was: {out}");
}

#[test]
fn gh_prs_rejects_an_invalid_after_date() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Prs\ntasks:\n  - name: list them\n    gh_prs:\n      after: not-a-date\n",
    )
    .unwrap();

    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().failure());
    assert!(out.contains("expected a date"), "stdout was: {out}");
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
fn write_csv_and_read_csv_round_trip_the_same_rows() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: CSV round trip\ntasks:\n  \
         - name: seed rows\n    set_fact:\n      \
         rows: '[{\"name\":\"Alice\",\"age\":\"30\"},{\"name\":\"Bob\",\"age\":\"25\"}]'\n  \
         - name: write it\n    write_csv:\n      path: out.csv\n      data: \"{{rows}}\"\n    \
         register: written\n  \
         - name: read it back\n    read_csv:\n      path: out.csv\n    register: back\n  \
         - name: show\n    debug: \"{{back}}\"\n",
    )
    .unwrap();

    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().success());
    assert!(out.contains("2 row(s)"), "stdout was: {out}");
    assert!(out.contains("\"name\":\"Alice\""), "stdout was: {out}");
    assert!(out.contains("\"age\":\"25\""), "stdout was: {out}");

    // Column order is alphabetical (serde_json::Value::Object isn't built with the
    // preserve_order feature in this crate), not the source YAML's field order.
    let written = std::fs::read_to_string(dir.path().join("out.csv")).unwrap();
    assert_eq!(written, "age,name\n30,Alice\n25,Bob\n");
}

#[test]
fn write_csv_task_rejects_a_path_that_escapes_the_playbook_directory() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Escape\ntasks:\n  - name: t\n    write_csv:\n      \
         path: \"../outside.csv\"\n      data: \"[]\"\n",
    )
    .unwrap();

    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().failure());
    assert!(
        out.contains("escapes the playbook directory"),
        "stdout was: {out}"
    );
    assert!(!dir.path().parent().unwrap().join("outside.csv").exists());
}

#[test]
fn write_csv_task_rejects_data_that_is_not_a_json_array() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Bad data\ntasks:\n  - name: t\n    write_csv:\n      \
         path: out.csv\n      data: '{\"not\": \"an array\"}'\n",
    )
    .unwrap();

    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().failure());
    assert!(out.contains("must be a JSON array"), "stdout was: {out}");
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

#[test]
fn a_typo_d_task_field_fails_clearly_instead_of_being_silently_ignored() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Typo\ntasks:\n  - name: risky task\n    run: echo hi\n    delya: 5\n    \
         registerr: oops\n",
    )
    .unwrap();

    let out = cmd.args(["play", "playbook.yml"]).assert().failure();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    assert!(
        stderr.contains("unknown field `delya`"),
        "stderr was: {stderr}"
    );
}
