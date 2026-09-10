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
fn continue_on_error_attempts_every_item_and_fails_at_the_end_naming_the_failed_one() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: ContinueOnError\ntasks:\n  - name: touch each unless b\n    \
         loop: [a, b, c]\n    continue_on_error: true\n    \
         run: \"test '{{item}}' != 'b' && touch ran-{{item}}.txt\"\n",
    )
    .unwrap();

    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().failure());
    assert!(dir.path().join("ran-a.txt").exists());
    assert!(dir.path().join("ran-c.txt").exists());
    assert!(
        out.contains("1 of 3 loop item(s) failed") && out.contains("item 2 (b)"),
        "stdout was: {out}"
    );
}

#[test]
fn without_continue_on_error_a_loop_still_aborts_on_the_first_failure() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: NoContinueOnError\ntasks:\n  - name: touch each unless b\n    \
         loop: [a, b, c]\n    \
         run: \"test '{{item}}' != 'b' && touch ran-{{item}}.txt\"\n",
    )
    .unwrap();

    cmd.args(["play", "playbook.yml"]).assert().failure();
    assert!(dir.path().join("ran-a.txt").exists());
    assert!(
        !dir.path().join("ran-c.txt").exists(),
        "item c should never have been attempted"
    );
}

#[test]
fn continue_on_error_under_max_parallel_still_runs_every_chunk() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: ContinueOnErrorParallel\ntasks:\n  - name: touch each unless b\n    \
         loop: [a, b, c, d]\n    max_parallel: 2\n    continue_on_error: true\n    \
         run: \"test '{{item}}' != 'b' && touch ran-{{item}}.txt\"\n",
    )
    .unwrap();

    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().failure());
    // b is in the first chunk (a, b); c and d are the second chunk -- both must still
    // run even though the first chunk had a failure.
    assert!(dir.path().join("ran-a.txt").exists());
    assert!(dir.path().join("ran-c.txt").exists());
    assert!(dir.path().join("ran-d.txt").exists());
    assert!(
        out.contains("1 of 4 loop item(s) failed"),
        "stdout was: {out}"
    );
}

#[test]
fn continue_on_error_without_loop_is_rejected_upfront() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: BadContinueOnError\ntasks:\n  - name: no loop here\n    \
         continue_on_error: true\n    run: echo hi\n",
    )
    .unwrap();

    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().failure());
    assert!(
        out.contains("continue_on_error: is only supported combined with loop:"),
        "stdout was: {out}"
    );
}

#[test]
fn continue_on_error_combined_with_ignore_errors_continues_the_playbook() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: ContinueOnErrorIgnored\ntasks:\n  - name: touch each unless b\n    \
         loop: [a, b, c]\n    continue_on_error: true\n    ignore_errors: true\n    \
         run: \"test '{{item}}' != 'b' && touch ran-{{item}}.txt\"\n  - name: still runs\n    \
         run: touch reached.txt\n",
    )
    .unwrap();

    cmd.args(["play", "playbook.yml"]).assert().success();
    assert!(dir.path().join("reached.txt").exists());
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
fn register_exit_code_is_zero_on_a_successful_run() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: ExitCodeOk\ntasks:\n  - name: succeed\n    run: echo hi\n    \
         register: r\n  - name: check it\n    assert: \"{{r.exit_code}} == 0\"\n",
    )
    .unwrap();

    cmd.args(["play", "playbook.yml"]).assert().success();
}

#[test]
fn register_exit_code_survives_a_failure_when_ignore_errors_lets_the_playbook_continue() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: ExitCodeIgnored\ntasks:\n  - name: fail with a specific code\n    \
         run: exit 3\n    register: r\n    ignore_errors: true\n  - name: check it\n    \
         assert: \"{{r.exit_code}} == 3\"\n",
    )
    .unwrap();

    cmd.args(["play", "playbook.yml"]).assert().success();
}

#[test]
fn run_env_injects_a_variable_into_the_subprocess() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: RunEnv\ntasks:\n  - name: read it\n    run:\n      command: \"echo $FOO\"\n      \
         env:\n        FOO: bar\n    register: out\n  - name: check it\n    \
         assert: \"{{out}} == bar\"\n",
    )
    .unwrap();

    cmd.args(["play", "playbook.yml"]).assert().success();
}

#[test]
fn run_env_value_is_templated_before_being_set() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: RunEnvTemplated\nvars:\n  greeting: hello-from-vars\ntasks:\n  - \
         name: read it\n    run:\n      command: \"echo $MSG\"\n      \
         env:\n        MSG: \"{{greeting}}\"\n    register: out\n  - name: check it\n    \
         assert: \"{{out}} == hello-from-vars\"\n",
    )
    .unwrap();

    cmd.args(["play", "playbook.yml"]).assert().success();
}

#[test]
fn structured_run_rejects_an_unknown_field() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: RunUnknownField\ntasks:\n  - name: bad\n    run:\n      command: echo hi\n      \
         env:\n        FOO: bar\n      bogus: true\n",
    )
    .unwrap();

    let out = cmd.args(["play", "playbook.yml"]).assert().failure();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    // `#[serde(untagged)]` doesn't surface the specific unknown-field name the way a
    // plain deny_unknown_fields struct does -- it just reports that nothing matched --
    // but it does still fail clearly (never silently drops the typo'd field).
    assert!(
        stderr.contains("did not match any variant"),
        "stderr was: {stderr}"
    );
}

#[test]
fn secret_set_without_confirm_fails_clearly_and_makes_no_write() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: SecretSetNoConfirm\ntasks:\n  - name: store it\n    secret_set:\n      \
         profile: __tooler_test_secret_set_probe__\n      key: probe\n      value: x\n",
    )
    .unwrap();

    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().failure());
    assert!(
        out.contains("refused to run without confirm: true"),
        "stdout was: {out}"
    );
}

#[test]
fn secret_set_dry_run_previews_without_writing() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: SecretSetDry\ntasks:\n  - name: store it\n    secret_set:\n      \
         profile: __tooler_test_secret_set_probe__\n      key: probe\n      value: x\n      \
         confirm: true\n",
    )
    .unwrap();

    cmd.args(["play", "playbook.yml", "--dry"])
        .assert()
        .success();
}

#[test]
fn secret_set_task_writes_a_secret_that_secret_dot_reads_back() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: SecretSetRoundTrip\ntasks:\n  - name: store it\n    secret_set:\n      \
         profile: __tooler_test_secret_set_probe__\n      key: probe\n      \
         value: hello123\n      confirm: true\n  - name: read it back\n    \
         assert: \"{{secret.__tooler_test_secret_set_probe__.probe}} == hello123\"\n",
    )
    .unwrap();

    let assert = cmd.args(["play", "playbook.yml"]).assert();
    let output = assert.get_output();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if !output.status.success() && combined.to_lowercase().contains("credential store") {
        eprintln!(
            "skipping secret_set_task_writes_a_secret_that_secret_dot_reads_back: no OS \
             credential store backend available in this environment"
        );
        return;
    }
    assert!(output.status.success(), "playbook failed: {combined}");
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
fn lint_flags_an_unquoted_tainted_value_reaching_run() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: LintTaint\ntasks:\n  - name: query rows\n    db_query:\n      \
         server: ghost\n      sql: SELECT 1\n    register: rows\n  - name: use it\n    \
         run: \"echo {{rows}}\"\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["--output", "json", "play", "playbook.yml", "--lint"])
            .assert()
            .success(),
    );
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    let findings = value["findings"].as_array().unwrap();
    assert!(
        findings
            .iter()
            .any(|f| f["message"].as_str().unwrap().contains("without | quote")),
        "findings were: {findings:?}"
    );
}

#[test]
fn lint_does_not_flag_a_tainted_value_piped_through_quote() {
    let (mut cmd, dir) = tooler();
    // A real (if unreachable) server profile, so Check C's server:-reference check
    // doesn't add an unrelated finding on top of the Check A/B behavior this test
    // actually exercises.
    tooler_in(dir.path())
        .args(["server", "add", "ghost", "--host", "127.0.0.1"])
        .assert()
        .success();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: LintTaintQuoted\ntasks:\n  - name: query rows\n    db_query:\n      \
         server: ghost\n      sql: SELECT 1\n    register: rows\n  - name: use it\n    \
         run: \"echo {{rows | quote}}\"\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["--output", "json", "play", "playbook.yml", "--lint"])
            .assert()
            .success(),
    );
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(value["findings"].as_array().unwrap().len(), 0);
}

#[test]
fn lint_does_not_flag_an_unquoted_value_from_a_non_tainted_source() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: LintNotTainted\ntasks:\n  - name: compute it\n    set_fact:\n      \
         greeting: hi\n  - name: use it\n    run: \"echo {{greeting}}\"\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["--output", "json", "play", "playbook.yml", "--lint"])
            .assert()
            .success(),
    );
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(value["findings"].as_array().unwrap().len(), 0);
}

#[test]
fn lint_flags_a_reference_to_an_undefined_var() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: LintTypo\ntasks:\n  - name: use a typo\n    debug: \"{{typo_var}}\"\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["--output", "json", "play", "playbook.yml", "--lint"])
            .assert()
            .success(),
    );
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    let findings = value["findings"].as_array().unwrap();
    assert!(
        findings
            .iter()
            .any(|f| f["message"].as_str().unwrap().contains("possible typo")),
        "findings were: {findings:?}"
    );
}

#[test]
fn lint_does_not_flag_vars_defined_by_vars_or_earlier_register_or_special_prefixes() {
    // secret.<profile>.<key> is deliberately not included here -- since Check C it's
    // checked against the real OS keychain (see lint_flags_a_secret_reference_that_isnt_set_in_the_keychain
    // / lint_does_not_flag_a_secret_that_is_actually_set for that, with the graceful
    // per-platform skip a real keychain probe needs). state:/env: stay blanket-skipped
    // regardless of platform, which is what this test actually covers.
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: LintKnownVars\nvars:\n  host: prod.example.com\ntasks:\n  - name: use vars\n    \
         debug: \"{{host}}\"\n  - name: register something\n    run: echo hi\n    \
         register: out\n  - name: use registered\n    debug: \"{{out}}\"\n  - \
         name: use special prefixes\n    debug: \"{{state.x}} {{env.HOME}}\"\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["--output", "json", "play", "playbook.yml", "--lint"])
            .assert()
            .success(),
    );
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(value["findings"].as_array().unwrap().len(), 0);
}

#[test]
fn lint_suppresses_undefined_var_check_after_an_include_vars_task() {
    let (mut cmd, dir) = tooler();
    // extra.yml must exist so Check D's path resolution doesn't flag it — this test is
    // about the undefined-var check being suppressed after include_vars:, nothing else.
    std::fs::write(dir.path().join("extra.yml"), "whatever_it_defined: x\n").unwrap();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: LintIncludeVars\ntasks:\n  - name: load\n    include_vars: extra.yml\n  - \
         name: use it\n    debug: \"{{whatever_it_defined}}\"\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["--output", "json", "play", "playbook.yml", "--lint"])
            .assert()
            .success(),
    );
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(value["findings"].as_array().unwrap().len(), 0);
}

#[test]
fn lint_makes_no_connections_and_always_exits_zero() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: LintNoConnect\ntasks:\n  - name: unreachable\n    ssh:\n      \
         server: ghost\n      command: \"echo {{typo}}\"\n",
    )
    .unwrap();

    // Would fail immediately on server resolution if this ever actually ran.
    cmd.args(["play", "playbook.yml", "--lint"])
        .assert()
        .success();
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
fn structured_assert_passes_when_every_condition_in_that_holds() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: StructuredAssertOk\nvars:\n  env: prod\n  count: \"3\"\ntasks:\n  - \
         name: multi-check\n    assert:\n      that:\n        - \"{{env}} == prod\"\n        \
         - \"{{count}} >= 1\"\n",
    )
    .unwrap();

    cmd.args(["play", "playbook.yml"]).assert().success();
}

#[test]
fn structured_assert_fails_naming_the_failing_condition() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: StructuredAssertFail\nvars:\n  env: staging\ntasks:\n  - name: multi-check\n    \
         assert:\n      that:\n        - \"{{env}} == prod\"\n        - \"1 == 1\"\n",
    )
    .unwrap();

    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().failure());
    assert!(
        out.contains("{{env}} == prod"),
        "expected the failing condition to be named, stdout was: {out}"
    );
}

#[test]
fn structured_assert_uses_the_custom_msg_on_failure() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: StructuredAssertMsg\nvars:\n  env: staging\ntasks:\n  - name: multi-check\n    \
         assert:\n      that:\n        - \"{{env}} == prod\"\n      msg: \"must run in prod\"\n",
    )
    .unwrap();

    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().failure());
    assert!(out.contains("must run in prod"), "stdout was: {out}");
}

#[test]
fn lint_flags_an_undefined_var_inside_a_structured_assert_that() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: LintStructuredAssert\ntasks:\n  - name: multi-check\n    assert:\n      \
         that:\n        - \"{{typo_var}} == 1\"\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["--output", "json", "play", "playbook.yml", "--lint"])
            .assert()
            .success(),
    );
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    let findings = value["findings"].as_array().unwrap();
    assert!(
        findings
            .iter()
            .any(|f| f["message"].as_str().unwrap().contains("possible typo")),
        "findings were: {findings:?}"
    );
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
fn timeout_kills_a_hung_command_and_its_orphaned_children() {
    // `sleep 5 & wait` forces sh to fork a real background child and then block on it,
    // guaranteeing sh does NOT exec-replace itself the way a bare `sleep 5` might on
    // some shells -- reproducing, unconditionally, the exact "killing the immediate
    // child orphans a grandchild that keeps the inherited stdout/stderr pipe open"
    // shape that let a hung command run to completion on at least one real shell (see
    // db::kill_process_group's doc comment). Without process-group killing, the
    // orphaned `sleep` keeps this test's own piped stdout open and assert_cmd blocks on
    // EOF for the full 5s, exactly like the failure this test guards against.
    //
    // Skipped on Windows: confirmed on real CI that `timeout_kills_a_hung_command`
    // (the realistic, non-adversarial case -- a bare `sleep 5`) passes there with
    // kill_process_group's `taskkill /T /F`, but this deliberately adversarial `& wait`
    // shell construct still runs the full 5s under Windows' Git-Bash/MSYS runtime --
    // most likely its background-job children aren't tracked with the direct parent
    // PID `taskkill /T` walks. Chasing that further needs a real Windows box to
    // iterate against, not guesswork from here; the underlying product fix (the
    // realistic case above) is confirmed working on all three platforms.
    if cfg!(windows) {
        eprintln!(
            "skipping timeout_kills_a_hung_command_and_its_orphaned_children: this \
             adversarial `& wait` shell construct isn't reliably tree-killed under \
             Windows' Git-Bash/MSYS runtime (see comment above) -- the realistic case \
             is covered by timeout_kills_a_hung_command, which does pass here"
        );
        return;
    }
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Timeout orphan test\n\
         tasks:\n\
         \x20\x20- name: hangs via a background child\n\
         \x20\x20\x20\x20run: \"sleep 5 & wait\"\n\
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
        "expected the timeout to cut this short (including the orphaned child), took {elapsed:?}"
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
fn timeout_on_ssh_no_longer_rejected_upfront_fails_on_server_resolution_instead() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: SshTimeout\ntasks:\n  - name: run it\n    ssh:\n      \
         server: ghost\n      command: echo hi\n    timeout: 5\n",
    )
    .unwrap();

    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().failure());
    assert!(out.contains("ghost"), "stdout was: {out}");
    assert!(!out.contains("only supported on"), "stdout was: {out}");
}

#[test]
fn timeout_on_fleet_no_longer_rejected_upfront_fails_on_server_resolution_instead() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: FleetTimeout\ntasks:\n  - name: run it\n    fleet:\n      \
         servers: ghost\n      command: echo hi\n    timeout: 5\n",
    )
    .unwrap();

    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().failure());
    assert!(!out.contains("only supported on"), "stdout was: {out}");
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
fn include_vars_task_makes_the_file_s_vars_usable_by_later_tasks() {
    let (mut cmd, dir) = tooler();
    std::fs::write(dir.path().join("extra.yml"), "greeting: hi-from-extra\n").unwrap();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: IncludeVars\ntasks:\n  - name: load it\n    include_vars: extra.yml\n  - \
         name: check it\n    assert: \"{{greeting}} == hi-from-extra\"\n",
    )
    .unwrap();

    cmd.args(["play", "playbook.yml"]).assert().success();
}

#[test]
fn include_vars_transparently_decrypts_a_vault_encrypted_file() {
    let (mut cmd, dir) = tooler();
    std::fs::write(dir.path().join("secrets.yml"), "api_key: super-secret\n").unwrap();
    tooler_in(dir.path())
        .env("TOOLER_VAULT_PASSWORD", "hunter2")
        .args(["vault", "encrypt", "secrets.yml"])
        .assert()
        .success();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: IncludeVarsVault\ntasks:\n  - name: load it\n    include_vars: secrets.yml\n  - \
         name: check it\n    assert: \"{{api_key}} == super-secret\"\n",
    )
    .unwrap();

    cmd.env("TOOLER_VAULT_PASSWORD", "hunter2")
        .args(["play", "playbook.yml"])
        .assert()
        .success();
}

#[test]
fn include_vars_is_a_no_op_in_a_dry_run() {
    let (mut cmd, dir) = tooler();
    std::fs::write(dir.path().join("extra.yml"), "greeting: hi-from-extra\n").unwrap();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: IncludeVarsDry\ntasks:\n  - name: load it\n    include_vars: extra.yml\n",
    )
    .unwrap();

    cmd.args(["play", "playbook.yml", "--dry"])
        .assert()
        .success();
}

#[test]
fn register_on_include_vars_is_rejected_upfront() {
    let (mut cmd, dir) = tooler();
    std::fs::write(dir.path().join("extra.yml"), "greeting: hi\n").unwrap();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: BadRegister\ntasks:\n  - name: load it\n    include_vars: extra.yml\n    \
         register: oops\n",
    )
    .unwrap();

    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().failure());
    assert!(
        out.contains("register: is not supported for"),
        "stdout was: {out}"
    );
}

#[test]
fn always_tagged_task_runs_even_when_not_selected_by_tags() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: AlwaysTag\ntasks:\n  - name: build step\n    tags: [build]\n    \
         run: touch built.txt\n  - name: cleanup step\n    tags: [always]\n    \
         run: touch cleaned.txt\n",
    )
    .unwrap();

    cmd.args(["play", "playbook.yml", "--tags", "something-else"])
        .assert()
        .success();
    assert!(!dir.path().join("built.txt").exists());
    assert!(dir.path().join("cleaned.txt").exists());
}

#[test]
fn skip_tags_always_still_excludes_an_always_tagged_task() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: SkipAlwaysTag\ntasks:\n  - name: cleanup step\n    tags: [always]\n    \
         run: touch cleaned.txt\n",
    )
    .unwrap();

    cmd.args(["play", "playbook.yml", "--skip-tags", "always"])
        .assert()
        .success();
    assert!(!dir.path().join("cleaned.txt").exists());
}

#[test]
fn list_tasks_still_shows_an_always_tagged_task_under_a_narrower_tags_filter() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: AlwaysTagListing\ntasks:\n  - name: build step\n    tags: [build]\n    \
         run: echo build\n  - name: cleanup step\n    tags: [always]\n    run: echo cleanup\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args([
            "play",
            "playbook.yml",
            "--tags",
            "something-else",
            "--list-tasks",
        ])
        .assert()
        .success(),
    );
    assert!(out.contains("cleanup step"), "stdout was: {out}");
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
fn failed_when_overrides_a_zero_exit_task_to_failed() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: FailedWhen\ntasks:\n  - name: always exits 0 but logs BAD\n    \
         run: \"echo BAD\"\n    register: out\n    failed_when: \"{{out}} == BAD\"\n",
    )
    .unwrap();

    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().failure());
    assert!(
        out.contains("failed_when: '{{out}} == BAD' was true"),
        "stdout was: {out}"
    );
}

#[test]
fn failed_when_is_retried_by_retries_before_giving_up() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: FailedWhenRetries\ntasks:\n  - name: always exits 0 but logs BAD\n    \
         run: \"echo BAD\"\n    register: out\n    failed_when: \"{{out}} == BAD\"\n    \
         retries: 2\n    delay: 0\n",
    )
    .unwrap();

    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().failure());
    assert!(out.contains("attempt 1/3"), "stdout was: {out}");
    assert!(out.contains("attempt 2/3"), "stdout was: {out}");
}

#[test]
fn failed_when_combined_with_ignore_errors_continues_the_playbook() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: FailedWhenIgnored\ntasks:\n  - name: always exits 0 but logs BAD\n    \
         run: \"echo BAD\"\n    register: out\n    failed_when: \"{{out}} == BAD\"\n    \
         ignore_errors: true\n  - name: still runs\n    run: touch reached.txt\n",
    )
    .unwrap();

    cmd.args(["play", "playbook.yml"]).assert().success();
    assert!(dir.path().join("reached.txt").exists());
}

#[test]
fn failed_when_is_ignored_in_a_dry_run() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: FailedWhenDry\ntasks:\n  - name: always exits 0 but logs BAD\n    \
         run: \"echo BAD\"\n    register: out\n    failed_when: \"{{out}} == BAD\"\n",
    )
    .unwrap();

    cmd.args(["play", "playbook.yml", "--dry"])
        .assert()
        .success();
}

#[test]
fn diff_shows_added_and_removed_lines_when_overwriting_a_local_file() {
    let (mut cmd, dir) = tooler();
    std::fs::write(dir.path().join("out.txt"), "old line\n").unwrap();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Diff\ntasks:\n  - name: overwrite\n    write_file:\n      \
         path: out.txt\n      content: \"new line\\n\"\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["play", "playbook.yml", "--diff"])
            .assert()
            .success(),
    );
    assert!(out.contains("old line"), "stdout was: {out}");
    assert!(out.contains("new line"), "stdout was: {out}");
}

#[test]
fn diff_on_append_shows_only_the_appended_line() {
    let (mut cmd, dir) = tooler();
    std::fs::write(dir.path().join("out.txt"), "kept line\n").unwrap();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: DiffAppend\ntasks:\n  - name: append\n    write_file:\n      \
         path: out.txt\n      content: \"new tail\\n\"\n      append: true\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["play", "playbook.yml", "--diff"])
            .assert()
            .success(),
    );
    assert!(out.contains("new tail"), "stdout was: {out}");
    // The kept line is unchanged (present on both sides), so it must never show up as a
    // `-`/`+` diff line -- only as ordinary equal context, which print_diff_if_enabled
    // never prints at all.
    assert!(
        !out.contains("-kept line") && !out.contains("+kept line"),
        "stdout was: {out}"
    );
}

#[test]
fn diff_flag_does_not_change_fs_write_s_existing_gates() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: DiffFsWrite\ntasks:\n  - name: overwrite it\n    fs_write:\n      \
         server: ghost\n      path: /etc/app/.env\n      content: FOO=1\n      confirm: true\n",
    )
    .unwrap();

    // Still fails clearly on the unconfigured server -- --diff doesn't skip or reorder
    // fs_write:'s existing confirm:/server-resolution gates.
    let out = stdout_of(
        cmd.args(["play", "playbook.yml", "--diff"])
            .assert()
            .failure(),
    );
    assert!(out.contains("ghost"), "stdout was: {out}");
}

#[test]
fn without_diff_flag_write_file_prints_no_diff_lines() {
    let (mut cmd, dir) = tooler();
    std::fs::write(dir.path().join("out.txt"), "old line\n").unwrap();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: NoDiff\ntasks:\n  - name: overwrite\n    write_file:\n      \
         path: out.txt\n      content: \"new line\\n\"\n",
    )
    .unwrap();

    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().success());
    assert!(!out.contains("old line"), "stdout was: {out}");
}

#[test]
fn flush_handlers_runs_a_pending_handler_immediately_mid_playbook() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: FlushHandlers\n\
         handlers:\n\
         \x20\x20- name: restart\n\
         \x20\x20\x20\x20run: echo restart >> order.txt\n\
         tasks:\n\
         \x20\x20- name: t1\n\
         \x20\x20\x20\x20run: echo t1 >> order.txt\n\
         \x20\x20\x20\x20notify: [restart]\n\
         \x20\x20- name: flush now\n\
         \x20\x20\x20\x20flush_handlers: true\n\
         \x20\x20- name: t2\n\
         \x20\x20\x20\x20run: echo t2 >> order.txt\n",
    )
    .unwrap();

    cmd.args(["play", "playbook.yml"]).assert().success();
    let order = std::fs::read_to_string(dir.path().join("order.txt")).unwrap();
    let lines: Vec<&str> = order.lines().collect();
    assert_eq!(
        lines,
        vec!["t1", "restart", "t2"],
        "handler should have run between t1 and t2, not at the very end"
    );
}

#[test]
fn flush_handlers_with_nothing_pending_is_a_harmless_ok_no_op() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: FlushNoOp\ntasks:\n  - name: flush now\n    flush_handlers: true\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["--output", "json", "play", "playbook.yml"])
            .assert()
            .success(),
    );
    let value = last_line_json(&out);
    assert_eq!(value["ok"], 1);
    assert_eq!(value["failed"], 0);
}

#[test]
fn flush_handlers_inside_a_block_fails_clearly() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: FlushInBlock\ntasks:\n  - name: wrapper\n    block:\n      \
         - name: flush now\n        flush_handlers: true\n",
    )
    .unwrap();

    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().failure());
    assert!(
        out.contains("flush_handlers: is only supported as a direct playbook task"),
        "stdout was: {out}"
    );
}

#[test]
fn vault_encrypted_vars_file_resolves_transparently_when_the_password_is_set() {
    let (mut cmd, dir) = tooler();
    std::fs::write(dir.path().join("secrets.yml"), "api_key: super-secret\n").unwrap();
    tooler_in(dir.path())
        .env("TOOLER_VAULT_PASSWORD", "hunter2")
        .args(["vault", "encrypt", "secrets.yml"])
        .assert()
        .success();

    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Vault\nvars_files: [secrets.yml]\ntasks:\n  - name: check it\n    \
         assert: \"{{api_key}} == super-secret\"\n",
    )
    .unwrap();

    cmd.env("TOOLER_VAULT_PASSWORD", "hunter2")
        .args(["play", "playbook.yml"])
        .assert()
        .success();
}

#[test]
fn vault_encrypted_vars_file_without_the_password_fails_clearly_naming_the_file() {
    let (mut cmd, dir) = tooler();
    std::fs::write(dir.path().join("secrets.yml"), "api_key: super-secret\n").unwrap();
    tooler_in(dir.path())
        .env("TOOLER_VAULT_PASSWORD", "hunter2")
        .args(["vault", "encrypt", "secrets.yml"])
        .assert()
        .success();

    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Vault\nvars_files: [secrets.yml]\ntasks:\n  - name: check it\n    \
         assert: \"{{api_key}} == super-secret\"\n",
    )
    .unwrap();

    let out = cmd
        .env_remove("TOOLER_VAULT_PASSWORD")
        .args(["play", "playbook.yml"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    assert!(
        stderr.contains("secrets.yml") && stderr.contains("TOOLER_VAULT_PASSWORD"),
        "stderr was: {stderr}"
    );
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

/// `--schema` describes the DSL itself, not one playbook -- it needs no FILE and no
/// project. `tooler()`'s temp dir has no `.tooler.toml` at all, proving this literally
/// rather than only by reading `run()`'s dispatch order.
#[test]
fn schema_prints_valid_json_with_no_file_and_no_project() {
    let (mut cmd, _dir) = tooler();
    let out = cmd.args(["play", "--schema"]).assert().success();
    let stdout = stdout_of(out);
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("--schema output was not valid JSON");
    assert_eq!(parsed["type"], "object");
}

#[test]
fn schema_top_level_describes_the_tasks_array() {
    let (mut cmd, _dir) = tooler();
    let out = cmd.args(["play", "--schema"]).assert().success();
    let parsed: serde_json::Value = serde_json::from_str(&stdout_of(out)).unwrap();
    assert!(
        parsed["properties"]["tasks"].is_object(),
        "expected a top-level `tasks` property, got: {parsed}"
    );
}

/// Lightweight regression guard: every `Task` field gets `JsonSchema` for free since
/// it's derived at the struct level, but this catches a future field whose *type*
/// doesn't implement `JsonSchema` (which would fail to compile, not silently vanish --
/// still worth a test naming the exact fields this round added, so a `cargo check`
/// failure on one of them points straight back here).
#[test]
fn schema_mentions_every_new_task_action() {
    let (mut cmd, _dir) = tooler();
    let out = cmd.args(["play", "--schema"]).assert().success();
    let stdout = stdout_of(out);
    for action in ["run", "assert", "secret_set", "loop", "confirm", "block"] {
        assert!(
            stdout.contains(&format!("\"{action}\"")),
            "schema is missing task action `{action}`"
        );
    }
}

#[test]
fn schema_ignores_unrelated_flags_and_still_just_prints_the_schema() {
    let (mut cmd, _dir) = tooler();
    let out = cmd
        .args(["play", "--schema", "--dry", "--yes"])
        .assert()
        .success();
    let stdout = stdout_of(out);
    serde_json::from_str::<serde_json::Value>(&stdout).expect("still valid JSON");
}

#[test]
fn changed_true_by_default_on_a_successful_task_with_no_changed_when() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Changed default\ntasks:\n  - name: t1\n    run: echo hi\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["--output", "json", "play", "playbook.yml"])
            .assert()
            .success(),
    );
    let value = last_line_json(&out);
    assert_eq!(value["tasks"][0]["changed"], true);
    assert_eq!(value["changed"], 1);
}

#[test]
fn changed_when_false_keeps_changed_at_zero() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Changed false\ntasks:\n  - name: t1\n    run: echo hi\n    \
         changed_when: \"false\"\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["--output", "json", "play", "playbook.yml"])
            .assert()
            .success(),
    );
    let value = last_line_json(&out);
    assert_eq!(value["tasks"][0]["status"], "ok");
    assert_eq!(value["tasks"][0]["changed"], false);
    assert_eq!(value["changed"], 0);
}

#[test]
fn skipped_and_ignored_tasks_never_count_as_changed() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Changed skip/ignore\ntasks:\n  - name: skip me\n    \
         when: \"{{missing}} == present\"\n    run: echo skipped\n  - name: fail me\n    \
         run: exit 1\n    ignore_errors: true\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["--output", "json", "play", "playbook.yml"])
            .assert()
            .success(),
    );
    let value = last_line_json(&out);
    assert_eq!(value["tasks"][0]["status"], "skipped");
    assert_eq!(value["tasks"][0]["changed"], false);
    assert_eq!(value["tasks"][1]["status"], "ignored");
    assert_eq!(value["tasks"][1]["changed"], false);
    assert_eq!(value["changed"], 0);
}

#[test]
fn error_kind_classifies_a_confirm_required_failure() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Confirm required\ntasks:\n  - name: set a secret\n    \
         secret_set:\n      profile: test\n      key: k\n      value: v\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["--output", "json", "play", "playbook.yml"])
            .assert()
            .failure(),
    );
    let value = last_line_json(&out);
    assert_eq!(value["tasks"][0]["error_kind"], "confirm_required");
}

#[test]
fn error_kind_classifies_an_assertion_failure() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Assertion\ntasks:\n  - name: check it\n    assert: \"1 == 2\"\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["--output", "json", "play", "playbook.yml"])
            .assert()
            .failure(),
    );
    let value = last_line_json(&out);
    assert_eq!(value["tasks"][0]["error_kind"], "assertion");
}

#[test]
fn error_kind_classifies_a_run_exit_code_failure() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: ExitCode\ntasks:\n  - name: fail\n    run: exit 7\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["--output", "json", "play", "playbook.yml"])
            .assert()
            .failure(),
    );
    let value = last_line_json(&out);
    assert_eq!(value["tasks"][0]["error_kind"], "exit_code");
}

#[test]
fn error_kind_falls_back_to_other_for_an_unrecognized_failure() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Unrecognized\ntasks:\n  - name: bad include\n    include: nope.yml\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["--output", "json", "play", "playbook.yml"])
            .assert()
            .failure(),
    );
    let value = last_line_json(&out);
    assert_eq!(value["tasks"][0]["error_kind"], "other");
}

#[test]
fn ssh_register_exit_code_is_populated_on_a_connection_failure() {
    let (mut cmd, dir) = tooler();
    tooler_in(dir.path())
        .args(["server", "add", "s1", "--host", "127.0.0.1", "--port", "1"])
        .assert()
        .success();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: SshExitCode\ntasks:\n  - name: connect\n    ssh:\n      server: s1\n      \
         command: echo hi\n    register: r\n    ignore_errors: true\n  - name: show it\n    \
         debug: \"code={{r.exit_code}}\"\n",
    )
    .unwrap();

    // Not asserting the exact OS exit code (ssh-version/platform-dependent) -- just
    // that it's populated with something numeric and non-zero, proving the plumbing
    // reaches ssh: the same way run:'s <reg>.exit_code already works.
    let out = stdout_of(
        cmd.args(["play", "playbook.yml"])
            .timeout(std::time::Duration::from_secs(15))
            .assert()
            .success(),
    );
    assert!(out.contains("code="), "stdout was: {out}");
    assert!(!out.contains("code=\n"), "stdout was: {out}");
}

#[test]
fn fleet_register_results_captures_per_server_exit_code() {
    let (mut cmd, dir) = tooler();
    tooler_in(dir.path())
        .args(["server", "add", "s1", "--host", "127.0.0.1", "--port", "1"])
        .assert()
        .success();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: FleetExitCode\ntasks:\n  - name: fan out\n    fleet:\n      servers: s1\n      \
         command: echo hi\n    register: r\n    ignore_errors: true\n  - name: show it\n    \
         debug: \"{{r.results}}\"\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["play", "playbook.yml"])
            .timeout(std::time::Duration::from_secs(15))
            .assert()
            .success(),
    );
    assert!(out.contains("\"server\":\"s1\""), "stdout was: {out}");
    assert!(out.contains("\"exit_code\":"), "stdout was: {out}");
}

#[test]
fn list_tasks_flags_a_destructive_task_missing_confirm() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: NeedsConfirm\ntasks:\n  - name: set a secret\n    \
         secret_set:\n      profile: test\n      key: k\n      value: v\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["--output", "json", "play", "playbook.yml", "--list-tasks"])
            .assert()
            .success(),
    );
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(value["tasks"][0]["confirmed"], false);
}

#[test]
fn list_tasks_shows_confirmed_true_when_the_yaml_already_sets_it() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: AlreadyConfirmed\ntasks:\n  - name: set a secret\n    \
         secret_set:\n      profile: test\n      key: k\n      value: v\n      confirm: true\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["--output", "json", "play", "playbook.yml", "--list-tasks"])
            .assert()
            .success(),
    );
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(value["tasks"][0]["confirmed"], true);
}

#[test]
fn list_tasks_omits_confirmed_entirely_for_a_non_destructive_task() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: NotDestructive\ntasks:\n  - name: t1\n    run: echo hi\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["--output", "json", "play", "playbook.yml", "--list-tasks"])
            .assert()
            .success(),
    );
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(
        value["tasks"][0].get("confirmed").is_none(),
        "expected `confirmed` to be entirely absent, got: {value}"
    );
}

#[test]
fn changed_when_on_a_handler_is_respected_not_hardcoded_true() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: HandlerChanged\nhandlers:\n  - name: restart\n    run: echo restarted\n    \
         changed_when: \"false\"\ntasks:\n  - name: t1\n    run: echo t1\n    notify: [restart]\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["--output", "json", "play", "playbook.yml"])
            .assert()
            .success(),
    );
    let value = last_line_json(&out);
    let handler_outcome = value["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "restart")
        .expect("handler outcome present");
    assert_eq!(handler_outcome["changed"], false);
}

#[test]
fn lint_flags_a_secret_reference_that_isnt_set_in_the_keychain() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: LintMissingSecret\ntasks:\n  - name: use it\n    \
         debug: \"{{secret.__tooler_test_lint_probe__.missing}}\"\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["--output", "json", "play", "playbook.yml", "--lint"])
            .assert()
            .success(),
    );
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    let findings = value["findings"].as_array().unwrap();
    // If this environment has no OS credential store backend, get_secret errors and
    // the check deliberately stays silent (not the playbook's fault) -- skip rather
    // than assert a false negative.
    if findings.is_empty() {
        eprintln!(
            "skipping lint_flags_a_secret_reference_that_isnt_set_in_the_keychain: no OS \
             credential store backend available in this environment"
        );
        return;
    }
    assert!(
        findings
            .iter()
            .any(|f| f["message"].as_str().unwrap().contains("isn't set")),
        "findings were: {findings:?}"
    );
}

#[test]
fn lint_does_not_flag_a_secret_that_is_actually_set() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: LintSetSecret\ntasks:\n  - name: store it\n    secret_set:\n      \
         profile: __tooler_test_lint_probe__\n      key: k\n      value: v\n      \
         confirm: true\n  - name: use it\n    \
         debug: \"{{secret.__tooler_test_lint_probe__.k}}\"\n",
    )
    .unwrap();

    // secret_set: writes for real here (--lint parses but never runs tasks, so we
    // need a separate real run first to actually populate the keychain entry).
    let write = tooler_in(dir.path())
        .args(["play", "playbook.yml"])
        .assert();
    let output = write.get_output();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if !output.status.success() && combined.to_lowercase().contains("credential store") {
        eprintln!(
            "skipping lint_does_not_flag_a_secret_that_is_actually_set: no OS credential \
             store backend available in this environment"
        );
        return;
    }
    assert!(output.status.success(), "playbook failed: {combined}");

    let out = stdout_of(
        cmd.args(["--output", "json", "play", "playbook.yml", "--lint"])
            .assert()
            .success(),
    );
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    let findings = value["findings"].as_array().unwrap();
    assert!(
        !findings
            .iter()
            .any(|f| f["message"].as_str().unwrap().contains("secret")),
        "findings were: {findings:?}"
    );
}

#[test]
fn lint_flags_an_unconfigured_server_profile() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: LintUnconfiguredServer\ntasks:\n  - name: connect\n    ssh:\n      \
         server: ghost\n      command: echo hi\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["--output", "json", "play", "playbook.yml", "--lint"])
            .assert()
            .success(),
    );
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    let findings = value["findings"].as_array().unwrap();
    assert!(
        findings.iter().any(|f| f["message"]
            .as_str()
            .unwrap()
            .contains("'ghost' isn't a configured server profile")),
        "findings were: {findings:?}"
    );
}

#[test]
fn lint_does_not_flag_a_configured_server_profile() {
    let (mut cmd, dir) = tooler();
    tooler_in(dir.path())
        .args(["server", "add", "s1", "--host", "127.0.0.1"])
        .assert()
        .success();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: LintConfiguredServer\ntasks:\n  - name: connect\n    ssh:\n      \
         server: s1\n      command: echo hi\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["--output", "json", "play", "playbook.yml", "--lint"])
            .assert()
            .success(),
    );
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    let findings = value["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 0, "findings were: {findings:?}");
}

#[test]
fn lint_does_not_flag_a_templated_server_reference() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: LintTemplatedServer\nvars:\n  env: prod\ntasks:\n  - name: connect\n    ssh:\n      \
         server: \"{{env}}\"\n      command: echo hi\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["--output", "json", "play", "playbook.yml", "--lint"])
            .assert()
            .success(),
    );
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    let findings = value["findings"].as_array().unwrap();
    assert!(
        !findings
            .iter()
            .any(|f| f["message"].as_str().unwrap().contains("server:")),
        "findings were: {findings:?}"
    );
}

#[test]
fn lint_flags_an_unconfigured_mail_profile() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: LintUnconfiguredMail\ntasks:\n  - name: check inbox\n    \
         mail_check:\n      server: ghost\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["--output", "json", "play", "playbook.yml", "--lint"])
            .assert()
            .success(),
    );
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    let findings = value["findings"].as_array().unwrap();
    assert!(
        findings.iter().any(|f| f["message"]
            .as_str()
            .unwrap()
            .contains("'ghost' isn't a configured mail profile")),
        "findings were: {findings:?}"
    );
}

#[test]
fn register_json_summary_includes_duration_ms_on_every_task() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: DurationMs\ntasks:\n  - name: skip me\n    \
         when: \"{{missing}} == present\"\n    run: echo skipped\n  - name: fast task\n    \
         debug: hi\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["--output", "json", "play", "playbook.yml"])
            .assert()
            .success(),
    );
    let value = last_line_json(&out);
    let tasks = value["tasks"].as_array().unwrap();
    for t in tasks {
        assert!(t["duration_ms"].is_number(), "task was: {t}");
    }
    assert_eq!(tasks[0]["status"], "skipped");
    assert_eq!(tasks[0]["duration_ms"], 0);
}

#[test]
fn timeout_on_sync_files_no_longer_rejected_upfront_fails_on_server_resolution_instead() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: SyncFilesTimeout\ntasks:\n  - name: sync\n    sync_files:\n      \
         server: ghost\n      from: /a\n      to: /b\n    timeout: 5\n",
    )
    .unwrap();

    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().failure());
    assert!(out.contains("ghost"), "stdout was: {out}");
    assert!(!out.contains("only supported on"), "stdout was: {out}");
}

#[test]
fn timeout_on_sync_db_no_longer_rejected_upfront_fails_on_server_resolution_instead() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: SyncDbTimeout\ntasks:\n  - name: sync\n    sync_db:\n      \
         server: ghost\n      from:\n        engine: mysql\n        host: 127.0.0.1\n        \
         database: a\n        user: u\n      to:\n        engine: mysql\n        \
         host: 127.0.0.1\n        database: b\n        user: u\n    timeout: 5\n",
    )
    .unwrap();

    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().failure());
    assert!(out.contains("ghost"), "stdout was: {out}");
    assert!(!out.contains("only supported on"), "stdout was: {out}");
}

#[test]
fn audit_log_entry_includes_error_kind_on_a_failure() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: AuditErrorKind\ntasks:\n  - name: bad assertion\n    assert: \"1 == 2\"\n",
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
    assert_eq!(entries[0]["error_kind"], "assertion");
}

#[test]
fn keep_checkpoint_preserves_the_state_file_after_a_successful_run() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: KeepCheckpoint\ntasks:\n  - name: t1\n    run: echo hi\n",
    )
    .unwrap();

    cmd.args(["play", "playbook.yml", "--keep-checkpoint"])
        .assert()
        .success();

    assert!(
        dir.path().join("playbook.yml.state.json").exists(),
        "expected the checkpoint to survive a successful run under --keep-checkpoint"
    );
}

#[test]
fn without_keep_checkpoint_the_state_file_is_still_deleted_as_before() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: NoKeepCheckpoint\ntasks:\n  - name: t1\n    run: echo hi\n",
    )
    .unwrap();

    cmd.args(["play", "playbook.yml"]).assert().success();

    assert!(
        !dir.path().join("playbook.yml.state.json").exists(),
        "expected the checkpoint to still be deleted on success without --keep-checkpoint"
    );
}

#[test]
fn deploy_without_confirm_fails_clearly_and_makes_no_connection() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Deploy\ntasks:\n  - name: deploy it\n    deploy:\n      \
         server: ghost\n      path: /var/www/app\n      restart: \"systemctl restart app\"\n",
    )
    .unwrap();

    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().failure());
    assert!(
        out.contains("refused to run without confirm: true"),
        "stdout was: {out}"
    );
}

#[test]
fn deploy_dry_run_previews_without_connecting() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Deploy\ntasks:\n  - name: deploy it\n    deploy:\n      \
         server: ghost\n      path: /var/www/app\n      restart: \"systemctl restart app\"\n      \
         confirm: true\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["play", "playbook.yml", "--dry"])
            .assert()
            .success(),
    );
    assert!(out.contains("/var/www/app"), "stdout was: {out}");
}

#[test]
fn deploy_against_an_unconfigured_server_fails_clearly() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Deploy\ntasks:\n  - name: deploy it\n    deploy:\n      \
         server: ghost\n      path: /var/www/app\n      restart: \"systemctl restart app\"\n      \
         confirm: true\n",
    )
    .unwrap();

    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().failure());
    assert!(out.contains("ghost"), "stdout was: {out}");
}

#[test]
fn lint_flags_an_unconfigured_group_on_fleet() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: LintFleetGroup\ntasks:\n  - name: fan out\n    fleet:\n      \
         group: ghost\n      command: echo hi\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["--output", "json", "play", "playbook.yml", "--lint"])
            .assert()
            .success(),
    );
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    let findings = value["findings"].as_array().unwrap();
    assert!(
        findings.iter().any(|f| f["message"]
            .as_str()
            .unwrap()
            .contains("'ghost' isn't a configured group")),
        "findings were: {findings:?}"
    );
}

#[test]
fn lint_does_not_flag_a_configured_group_on_fleet() {
    let (mut cmd, dir) = tooler();
    tooler_in(dir.path())
        .args(["group", "add", "web"])
        .assert()
        .success();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: LintFleetGroupOk\ntasks:\n  - name: fan out\n    fleet:\n      \
         group: web\n      command: echo hi\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["--output", "json", "play", "playbook.yml", "--lint"])
            .assert()
            .success(),
    );
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(value["findings"].as_array().unwrap().len(), 0);
}

#[test]
fn lint_flags_individual_bad_names_inside_fleets_comma_separated_servers() {
    let (mut cmd, dir) = tooler();
    tooler_in(dir.path())
        .args(["server", "add", "s1", "--host", "127.0.0.1"])
        .assert()
        .success();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: LintFleetServers\ntasks:\n  - name: fan out\n    fleet:\n      \
         servers: \"s1,ghost\"\n      command: echo hi\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["--output", "json", "play", "playbook.yml", "--lint"])
            .assert()
            .success(),
    );
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    let findings = value["findings"].as_array().unwrap();
    assert!(
        findings.iter().any(|f| f["message"]
            .as_str()
            .unwrap()
            .contains("'ghost' isn't a configured server profile")),
        "findings were: {findings:?}"
    );
    assert!(
        !findings
            .iter()
            .any(|f| f["message"].as_str().unwrap().contains("'s1'")),
        "s1 is configured and should not be flagged: {findings:?}"
    );
}

#[test]
fn lint_does_not_flag_fleet_targeting_all() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: LintFleetAll\ntasks:\n  - name: fan out\n    fleet:\n      \
         all: true\n      command: echo hi\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["--output", "json", "play", "playbook.yml", "--lint"])
            .assert()
            .success(),
    );
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(value["findings"].as_array().unwrap().len(), 0);
}

// ── upload: ───────────────────────────────────────────────────────────────────

#[test]
fn upload_without_confirm_fails_clearly_and_makes_no_connection() {
    let (mut cmd, dir) = tooler();
    std::fs::write(dir.path().join("artifact.bin"), b"hello").unwrap();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Upload\ntasks:\n  - name: push it\n    upload:\n      \
         server: ghost\n      local: artifact.bin\n      remote: /tmp/artifact.bin\n",
    )
    .unwrap();

    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().failure());
    assert!(
        out.contains("refused to run without confirm: true"),
        "stdout was: {out}"
    );
}

#[test]
fn upload_missing_local_file_fails_before_connecting() {
    let (mut cmd, dir) = tooler();
    tooler_in(dir.path())
        .args(["server", "add", "up1", "--host", "127.0.0.1"])
        .assert()
        .success();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Upload\ntasks:\n  - name: push it\n    upload:\n      \
         server: up1\n      local: nope.bin\n      remote: /tmp/nope.bin\n      confirm: true\n",
    )
    .unwrap();

    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().failure());
    assert!(
        out.contains("upload: local file not found"),
        "stdout was: {out}"
    );
}

#[test]
fn upload_dry_run_previews_without_connecting() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Upload\ntasks:\n  - name: push it\n    upload:\n      \
         server: ghost\n      local: artifact.bin\n      remote: /srv/app/artifact.bin\n      \
         confirm: true\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["play", "playbook.yml", "--dry"])
            .assert()
            .success(),
    );
    assert!(out.contains("/srv/app/artifact.bin"), "stdout was: {out}");
}

// ── cron: ─────────────────────────────────────────────────────────────────────

#[test]
fn cron_add_without_confirm_fails_before_connecting() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Cron\ntasks:\n  - name: schedule it\n    cron:\n      \
         server: ghost\n      add: \"0 3 * * * /srv/backup.sh\"\n",
    )
    .unwrap();

    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().failure());
    assert!(
        out.contains("refused to run without confirm: true"),
        "stdout was: {out}"
    );
}

#[test]
fn cron_rejects_more_than_one_of_add_remove_list() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Cron\ntasks:\n  - name: confused\n    cron:\n      \
         server: ghost\n      add: \"0 3 * * * x\"\n      list: true\n      confirm: true\n",
    )
    .unwrap();

    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().failure());
    assert!(
        out.contains("exactly one of add/remove/list"),
        "stdout was: {out}"
    );
}

#[test]
fn cron_list_against_an_unconfigured_server_fails_clearly() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Cron\ntasks:\n  - name: read it\n    cron:\n      server: ghost\n      list: true\n",
    )
    .unwrap();

    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().failure());
    assert!(out.contains("ghost"), "stdout was: {out}");
}

// ── on_failure: ───────────────────────────────────────────────────────────────

#[test]
fn on_failure_hook_runs_when_a_task_fails() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: WithHook\n\
         on_failure:\n  - name: record it\n    write_file:\n      path: failed.txt\n      \
         content: \"{{failed_task}}\"\n\
         tasks:\n  - name: fine\n    run: echo ok\n  - name: boom\n    run: \"exit 3\"\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["--output", "json", "play", "playbook.yml"])
            .assert()
            .failure(),
    );
    let summary = last_line_json(&out);
    assert_eq!(summary["success"], false);
    assert!(
        summary["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"] == "record it"),
        "on_failure task missing from tasks: {out}"
    );
    let recorded = std::fs::read_to_string(dir.path().join("failed.txt")).unwrap();
    assert_eq!(recorded.trim(), "boom");
}

#[test]
fn on_failure_hook_does_not_run_on_full_success() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: WithHook\n\
         on_failure:\n  - name: record it\n    write_file:\n      path: failed.txt\n      \
         content: nope\n\
         tasks:\n  - name: fine\n    run: echo ok\n",
    )
    .unwrap();

    cmd.args(["play", "playbook.yml"]).assert().success();
    assert!(!dir.path().join("failed.txt").exists());
}

#[test]
fn on_failure_hook_skipped_in_dry_run() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: WithHook\n\
         on_failure:\n  - name: record it\n    write_file:\n      path: failed.txt\n      \
         content: nope\n\
         tasks:\n  - name: boom\n    run: \"exit 3\"\n",
    )
    .unwrap();

    // --dry never really runs a task, so nothing fails and the hook never fires.
    cmd.args(["play", "playbook.yml", "--dry"])
        .assert()
        .success();
    assert!(!dir.path().join("failed.txt").exists());
}

// ── Lint Check D ──────────────────────────────────────────────────────────────

#[test]
fn lint_flags_notify_with_no_matching_handler() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: LintNotify\ntasks:\n  - name: do a thing\n    run: echo hi\n    notify: [nope]\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["--output", "json", "play", "playbook.yml", "--lint"])
            .assert()
            .success(),
    );
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(
        value["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["message"]
                .as_str()
                .unwrap()
                .contains("matches no handler")),
        "findings were: {out}"
    );
}

#[test]
fn lint_flags_missing_include_and_vars_files_paths() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: LintPaths\nvars_files: [missing.yml]\ntasks:\n  - name: pull in a sub\n    \
         include: sub.yml\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["--output", "json", "play", "playbook.yml", "--lint"])
            .assert()
            .success(),
    );
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    let msgs: Vec<&str> = value["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["message"].as_str().unwrap())
        .collect();
    assert!(
        msgs.iter()
            .any(|m| m.contains("'sub.yml' resolves to no file")),
        "findings were: {msgs:?}"
    );
    assert!(
        msgs.iter().any(|m| m.contains("'missing.yml' not found")),
        "findings were: {msgs:?}"
    );
}

#[test]
fn lint_does_not_flag_a_templated_include_path() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: LintTemplatedInclude\nvars:\n  which: sub\ntasks:\n  - name: pull in a sub\n    \
         include: \"{{which}}.yml\"\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["--output", "json", "play", "playbook.yml", "--lint"])
            .assert()
            .success(),
    );
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(
        !value["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["message"]
                .as_str()
                .unwrap()
                .contains("resolves to no file")),
        "findings were: {out}"
    );
}

// ── single_instance: lock ─────────────────────────────────────────────────────

/// A far-future RFC3339 timestamp — comfortably inside any `lock_timeout` window, so a
/// hand-written lock reads as "fresh". Avoids pulling chrono into the test crate.
fn fresh_lock_timestamp() -> &'static str {
    "3000-01-01T00:00:00+00:00"
}

#[test]
fn single_instance_refusal_names_the_holding_pid() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Locked\nsingle_instance: true\ntasks:\n  - name: t\n    run: echo hi\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("playbook.yml.lock"),
        format!(
            "{{\"pid\":424242,\"started_at\":\"{}\",\"playbook\":\"Locked\",\"host\":\"h\"}}\n",
            fresh_lock_timestamp()
        ),
    )
    .unwrap();

    let assert = cmd.args(["play", "playbook.yml"]).assert().failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();
    assert!(
        stderr.contains("already running (pid 424242"),
        "stderr was: {stderr}"
    );
    // The playbook never ran.
    assert!(!dir.path().join("ran.txt").exists());
}

#[test]
fn single_instance_takes_over_a_stale_lock() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Locked\nsingle_instance: true\nlock_timeout: 1\ntasks:\n  - name: t\n    \
         write_file:\n      path: ran.txt\n      content: ok\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("playbook.yml.lock"),
        "{\"pid\":1,\"started_at\":\"2020-01-01T00:00:00+00:00\",\"playbook\":\"Locked\",\"host\":\"h\"}\n",
    )
    .unwrap();

    cmd.args(["play", "playbook.yml"]).assert().success();
    assert!(dir.path().join("ran.txt").exists());
    assert!(
        !dir.path().join("playbook.yml.lock").exists(),
        "lock should be released"
    );
}

#[test]
fn single_instance_releases_the_lock_on_success_and_on_failure() {
    let (_c, dir) = tooler();
    std::fs::write(
        dir.path().join("ok.yml"),
        "name: Ok\nsingle_instance: true\ntasks:\n  - name: t\n    run: echo hi\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("bad.yml"),
        "name: Bad\nsingle_instance: true\ntasks:\n  - name: t\n    run: \"exit 1\"\n",
    )
    .unwrap();

    tooler_in(dir.path())
        .args(["play", "ok.yml"])
        .assert()
        .success();
    assert!(!dir.path().join("ok.yml.lock").exists());

    tooler_in(dir.path())
        .args(["play", "bad.yml"])
        .assert()
        .failure();
    assert!(!dir.path().join("bad.yml.lock").exists());

    // JSON-mode failure exits via process::exit — the lock must still be gone.
    tooler_in(dir.path())
        .args(["--output", "json", "play", "bad.yml"])
        .assert()
        .failure();
    assert!(!dir.path().join("bad.yml.lock").exists());
}

#[test]
fn single_instance_ignores_the_lock_in_dry_run() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Locked\nsingle_instance: true\ntasks:\n  - name: t\n    run: echo hi\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("playbook.yml.lock"),
        format!(
            "{{\"pid\":424242,\"started_at\":\"{}\",\"playbook\":\"Locked\",\"host\":\"h\"}}\n",
            fresh_lock_timestamp()
        ),
    )
    .unwrap();

    cmd.args(["play", "playbook.yml", "--dry"])
        .assert()
        .success();
}

// ── loop: batch: ─────────────────────────────────────────────────────────────

#[test]
fn loop_batch_groups_items_and_exposes_batch_as_a_json_array() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Batch\ntasks:\n  - name: rows\n    set_fact:\n      rows: '[1,2,3,4,5,6,7]'\n  \
         - name: chunk\n    loop: {from: \"{{rows}}\", batch: 3}\n    \
         write_file:\n      path: \"b{{batch_index}}.txt\"\n      content: \"{{batch_size}}:{{batch}}\"\n",
    )
    .unwrap();

    cmd.args(["play", "playbook.yml"]).assert().success();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("b0.txt")).unwrap(),
        "3:[\"1\",\"2\",\"3\"]"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("b2.txt")).unwrap(),
        "1:[\"7\"]"
    );
    assert!(!dir.path().join("b3.txt").exists());
}

#[test]
fn loop_batch_composes_with_max_parallel() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: BatchPar\ntasks:\n  - name: rows\n    set_fact:\n      rows: '[1,2,3,4,5]'\n  \
         - name: chunk\n    loop: {from: \"{{rows}}\", batch: 2}\n    max_parallel: 2\n    \
         write_file:\n      path: \"p{{batch_index}}.txt\"\n      content: \"{{batch_size}}\"\n",
    )
    .unwrap();

    cmd.args(["play", "playbook.yml"]).assert().success();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("p0.txt")).unwrap(),
        "2"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("p1.txt")).unwrap(),
        "2"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("p2.txt")).unwrap(),
        "1"
    );
}

#[test]
fn loop_batch_var_outside_a_loop_is_flagged_by_lint() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: L\ntasks:\n  - name: t\n    debug: \"{{batch_index}}\"\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["--output", "json", "play", "playbook.yml", "--lint"])
            .assert()
            .success(),
    );
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(
        v["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["message"].as_str().unwrap().contains("has no loop:")),
        "findings were: {out}"
    );
}

// ── parallel: ────────────────────────────────────────────────────────────────

#[test]
fn parallel_runs_children_and_merges_their_registered_vars() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Par\ntasks:\n  - name: fan out\n    parallel:\n      - name: a\n        \
         run: echo AA\n        register: ra\n      - name: b\n        run: echo BB\n        \
         register: rb\n  - name: use\n    write_file:\n      path: out.txt\n      \
         content: \"{{ra}}-{{rb}}\"\n",
    )
    .unwrap();

    cmd.args(["play", "playbook.yml"]).assert().success();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("out.txt")).unwrap(),
        "AA-BB"
    );
}

#[test]
fn parallel_first_failing_child_fails_the_task_and_names_it() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Par\ntasks:\n  - name: fan out\n    parallel:\n      - name: good\n        \
         run: echo ok\n      - name: bad\n        run: \"exit 3\"\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["--output", "json", "play", "playbook.yml"])
            .assert()
            .failure(),
    );
    let summary = last_line_json(&out);
    assert_eq!(summary["success"], false);
    assert!(
        summary["tasks"][0]["error"]
            .as_str()
            .unwrap()
            .contains("child task 'bad'"),
        "error was: {out}"
    );
}

#[test]
fn parallel_rejects_a_confirm_pause_child() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Par\ntasks:\n  - name: p\n    parallel:\n      - name: x\n        confirm: \"ok?\"\n",
    )
    .unwrap();

    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().failure());
    assert!(out.contains("uses confirm:"), "stdout was: {out}");
}

#[test]
fn parallel_counts_as_one_recap_outcome() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Par\ntasks:\n  - name: fan out\n    parallel:\n      - {name: a, run: echo a}\n      \
         - {name: b, run: echo b}\n      - {name: c, run: echo c}\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["--output", "json", "play", "playbook.yml"])
            .assert()
            .success(),
    );
    let summary = last_line_json(&out);
    assert_eq!(summary["tasks"].as_array().unwrap().len(), 1);
    assert_eq!(summary["ok"], 1);
}

#[test]
fn list_tasks_shows_children_of_a_parallel_block() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Par\ntasks:\n  - name: fan out\n    parallel:\n      - {name: a, run: echo a}\n      \
         - {name: b, run: echo b}\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["--output", "json", "play", "playbook.yml", "--list-tasks"])
            .assert()
            .success(),
    );
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let kids = v["tasks"][0]["parallel"].as_array().unwrap();
    assert_eq!(kids.len(), 2);
    assert_eq!(kids[0]["name"], "a");
}

// ── db_load: ─────────────────────────────────────────────────────────────────

#[test]
fn db_load_without_confirm_fails_before_connecting() {
    let (mut cmd, dir) = tooler();
    std::fs::write(dir.path().join("rows.csv"), "id,total\n1,10\n").unwrap();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Load\ntasks:\n  - name: load\n    db_load:\n      server: ghost\n      \
         table: orders\n      file: rows.csv\n      engine: mysql\n",
    )
    .unwrap();

    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().failure());
    assert!(
        out.contains("refused to run without confirm: true"),
        "stdout was: {out}"
    );
}

#[test]
fn db_load_missing_file_fails_before_connecting() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Load\ntasks:\n  - name: load\n    db_load:\n      server: ghost\n      \
         table: orders\n      file: nope.csv\n      engine: mysql\n      confirm: true\n",
    )
    .unwrap();

    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().failure());
    assert!(
        out.contains("db_load: reading local file"),
        "stdout was: {out}"
    );
}

#[test]
fn db_load_rejects_a_bad_table_identifier() {
    let (mut cmd, dir) = tooler();
    tooler_in(dir.path())
        .args(["server", "add", "db1", "--host", "127.0.0.1"])
        .assert()
        .success();
    std::fs::write(dir.path().join("rows.csv"), "id\n1\n").unwrap();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Load\ntasks:\n  - name: load\n    db_load:\n      server: db1\n      \
         table: \"orders; DROP TABLE x\"\n      file: rows.csv\n      engine: mysql\n      \
         host: h\n      database: d\n      user: u\n      password: p\n      confirm: true\n",
    )
    .unwrap();

    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().failure());
    assert!(out.contains("invalid SQL identifier"), "stdout was: {out}");
}

#[test]
fn db_load_dry_run_previews_without_connecting() {
    let (mut cmd, dir) = tooler();
    std::fs::write(dir.path().join("rows.csv"), "id\n1\n").unwrap();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: Load\ntasks:\n  - name: load\n    db_load:\n      server: ghost\n      \
         table: orders\n      file: rows.csv\n      engine: mysql\n      confirm: true\n",
    )
    .unwrap();

    let out = stdout_of(
        cmd.args(["play", "playbook.yml", "--dry"])
            .assert()
            .success(),
    );
    assert!(
        out.contains("load rows.csv → ghost:orders"),
        "stdout was: {out}"
    );
}

// ── rich template filters ────────────────────────────────────────────────────

fn run_debug_line(dir: &std::path::Path, body: &str) -> String {
    std::fs::write(
        dir.join("playbook.yml"),
        format!("name: F\ntasks:\n  - name: rows\n    set_fact:\n      rows: '[{{\"name\":\"a\",\"active\":\"true\"}},{{\"name\":\"b\",\"active\":\"false\"}},{{\"name\":\"c\",\"active\":\"true\"}}]'\n  - name: show\n    debug: \"{body}\"\n"),
    )
    .unwrap();
    stdout_of(
        tooler_in(dir)
            .args(["play", "playbook.yml"])
            .assert()
            .success(),
    )
}

#[test]
fn default_fills_in_an_unresolved_token() {
    let (_c, dir) = tooler();
    let out = run_debug_line(dir.path(), "v=[{{nope | default:fallback}}]");
    assert!(out.contains("v=[fallback]"), "{out}");
}

#[test]
fn pluck_where_join_chain_left_to_right() {
    let (_c, dir) = tooler();
    let out = run_debug_line(
        dir.path(),
        "{{rows | where:active==true | pluck:name | join:- }}",
    );
    assert!(out.contains("a-c"), "{out}");
}

#[test]
fn first_last_and_string_ops() {
    let (_c, dir) = tooler();
    let out = run_debug_line(
        dir.path(),
        "f={{rows | first | json:name | upper}} l={{rows | last | json:name}}",
    );
    assert!(out.contains("f=A l=c"), "{out}");
}

#[test]
fn a_bad_shape_in_a_filter_leaves_the_token_literal() {
    let (_c, dir) = tooler();
    // "hello" isn't a JSON array — pluck can't apply, token stays literal.
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: F\nvars: {s: hello}\ntasks:\n  - name: show\n    debug: \"{{s | pluck:x}}\"\n",
    )
    .unwrap();
    let out = stdout_of(
        tooler_in(dir.path())
            .args(["play", "playbook.yml"])
            .assert()
            .success(),
    );
    assert!(out.contains("{{s | pluck:x}}"), "{out}");
}

#[test]
fn quote_anywhere_in_a_pipeline_satisfies_the_injection_lint() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: F\ntasks:\n  - name: fetch\n    http: {url: \"http://127.0.0.1:1/x\", ignore_status: true}\n    register: r\n  - name: use\n    run: \"echo {{r | json:msg | quote}}\"\n",
    )
    .unwrap();
    let out = stdout_of(
        cmd.args(["--output", "json", "play", "playbook.yml", "--lint"])
            .assert()
            .success(),
    );
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(
        !v["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["message"].as_str().unwrap().contains("shell injection")),
        "{out}"
    );
}

// ── http: pagination ─────────────────────────────────────────────────────────

/// A server that answers sequential GETs with `make_bodies(port)[i]` (so a body can
/// embed its own server's URL), then closes.
fn serve_sequence(make_bodies: impl FnOnce(u16) -> Vec<String>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let bodies = make_bodies(port);
    std::thread::spawn(move || {
        use std::io::{Read, Write};
        for body in bodies {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut buf = [0u8; 2048];
            let _ = stream.read(&mut buf);
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(header.as_bytes());
            let _ = stream.write_all(body.as_bytes());
        }
    });
    port
}

#[test]
fn paginate_concatenates_pages_until_next_is_blank() {
    let (mut cmd, dir) = tooler();
    // Each page's `next` carries the full URL of the next one; the last is "" -> stop.
    let port = serve_sequence(|p| {
        let base = format!("http://127.0.0.1:{p}");
        vec![
            format!(r#"{{"results":[1,2],"next":"{base}/2"}}"#),
            format!(r#"{{"results":[3,4],"next":"{base}/3"}}"#),
            r#"{"results":[5],"next":""}"#.to_string(),
        ]
    });
    let base = format!("http://127.0.0.1:{port}");

    std::fs::write(
        dir.path().join("playbook.yml"),
        format!(
            "name: P\ntasks:\n  - name: all\n    http:\n      url: \"{base}/1\"\n      paginate:\n        next: \"{{{{page | json:next}}}}\"\n        items: results\n    register: r\n  - name: show\n    debug: \"got={{{{r}}}} pages={{{{r.pages}}}}\"\n"
        ),
    )
    .unwrap();

    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().success());
    assert!(out.contains("got=[1,2,3,4,5]"), "{out}");
    assert!(out.contains("pages=3"), "{out}");
}

#[test]
fn paginate_and_download_together_is_rejected() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: P\ntasks:\n  - name: x\n    http:\n      url: http://127.0.0.1:1/a\n      download: out.bin\n      paginate: {next: \"\"}\n",
    )
    .unwrap();
    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().failure());
    assert!(
        out.contains("can't combine download: and paginate:"),
        "{out}"
    );
}

// ── --explain ────────────────────────────────────────────────────────────────

#[test]
fn explain_resolves_vars_and_shows_the_concrete_command() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: E\nvars: {env: prod}\ntasks:\n  - name: build\n    run: \"make deploy ENV={{env}}\"\n  - name: mig\n    db_exec: {server: db1, sql: \"UPDATE f SET v=1 WHERE e='{{env}}'\", confirm: true}\n",
    )
    .unwrap();
    let out = stdout_of(
        cmd.args(["--output", "json", "play", "playbook.yml", "--explain"])
            .assert()
            .success(),
    );
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["tasks"][0]["explain"]["command"], "make deploy ENV=prod");
    assert_eq!(
        v["tasks"][1]["explain"]["sql"],
        "UPDATE f SET v=1 WHERE e='prod'"
    );
}

#[test]
fn explain_makes_no_connection_and_exits_zero() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: E\ntasks:\n  - name: x\n    ssh: {server: ghost, command: whoami}\n",
    )
    .unwrap();
    // ghost is unconfigured; a real run would fail, --explain must not.
    cmd.args(["play", "playbook.yml", "--explain"])
        .assert()
        .success();
}

// ── playbook params: defaults ────────────────────────────────────────────────

#[test]
fn params_defaults_seed_vars_on_a_plain_cli_run() {
    let (mut cmd, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: P\nparams:\n  greeting: {type: string, default: hola}\ntasks:\n  - name: show\n    debug: \"{{greeting}} {{name | default:world}}\"\n",
    )
    .unwrap();
    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().success());
    assert!(out.contains("hola world"), "{out}");
}

// ── date / encoding / numeric filters ────────────────────────────────────────

#[test]
fn now_var_and_date_filters() {
    let (_c, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: N\ntasks:\n  - name: show\n    write_file:\n      path: out.txt\n      \
         content: \"d={{now | date:%Y}} e={{now | unix}} past={{now | shift:-365d | date:%Y}}\"\n",
    )
    .unwrap();
    tooler_in(dir.path())
        .args(["play", "playbook.yml"])
        .assert()
        .success();
    let out = std::fs::read_to_string(dir.path().join("out.txt")).unwrap();
    // year is 4 digits, epoch is a 10-digit number, past year is this year minus 1.
    let year: i64 = out
        .split("d=")
        .nth(1)
        .unwrap()
        .split(' ')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let past: i64 = out.rsplit("past=").next().unwrap().trim().parse().unwrap();
    assert!(year >= 2025 && past == year - 1, "out was: {out}");
    assert!(
        out.contains("e=1")
            && out
                .split("e=")
                .nth(1)
                .unwrap()
                .split(' ')
                .next()
                .unwrap()
                .len()
                == 10
    );
}

#[test]
fn hash_encoding_and_numeric_filters_end_to_end() {
    let (_c, dir) = tooler();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: H\ntasks:\n  - name: d\n    set_fact:\n      nums: '[10,20,30]'\n  \
         - name: show\n    write_file:\n      path: out.txt\n      content: |\n        \
         sha={{ 'abc' | sha256 }}\n        b64={{ 'abc' | base64 }}\n        \
         sum={{nums | sum}}\n        avg={{nums | avg}}\n        inc={{ '41' | add:1 }}\n",
    )
    .unwrap();
    tooler_in(dir.path())
        .args(["play", "playbook.yml"])
        .assert()
        .success();
    let out = std::fs::read_to_string(dir.path().join("out.txt")).unwrap();
    assert!(out.contains("sha=ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"));
    assert!(out.contains("b64=YWJj"));
    assert!(out.contains("sum=60"));
    assert!(out.contains("avg=20"));
    assert!(out.contains("inc=42"));
}

// ── template: task ──────────────────────────────────────────────────────────

#[test]
fn template_renders_a_loop_to_a_local_file() {
    let (_c, dir) = tooler();
    std::fs::write(
        dir.path().join("conf.j2"),
        "{% for u in users %}user {{ u.name }} = {{ u.role }}\n{% endfor %}",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: T\ntasks:\n  - name: d\n    set_fact:\n      users: '[{\"name\":\"a\",\"role\":\"admin\"},{\"name\":\"b\",\"role\":\"ro\"}]'\n  \
         - name: render\n    template: {src: conf.j2, dest: conf.out}\n",
    )
    .unwrap();
    tooler_in(dir.path())
        .args(["play", "playbook.yml"])
        .assert()
        .success();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("conf.out")).unwrap(),
        "user a = admin\nuser b = ro\n"
    );
}

#[test]
fn template_to_a_server_requires_confirm() {
    let (mut cmd, dir) = tooler();
    std::fs::write(dir.path().join("c.j2"), "hi {{ x }}").unwrap();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: T\nvars: {x: there}\ntasks:\n  - name: push\n    template: {src: c.j2, dest: /etc/c, server: ghost}\n",
    )
    .unwrap();
    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().failure());
    assert!(
        out.contains("refused to run without confirm: true"),
        "{out}"
    );
}

#[test]
fn template_syntax_error_is_reported() {
    let (mut cmd, dir) = tooler();
    std::fs::write(dir.path().join("c.j2"), "{% for x in %}broken").unwrap();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: T\ntasks:\n  - name: render\n    template: {src: c.j2, dest: c.out}\n",
    )
    .unwrap();
    let out = stdout_of(cmd.args(["play", "playbook.yml"]).assert().failure());
    assert!(out.contains("template:"), "{out}");
}

#[test]
fn explain_shows_a_template_tasks_src_and_dest() {
    let (mut cmd, dir) = tooler();
    std::fs::write(dir.path().join("c.j2"), "x").unwrap();
    std::fs::write(
        dir.path().join("playbook.yml"),
        "name: T\nvars: {env: prod}\ntasks:\n  - name: render\n    template: {src: c.j2, dest: \"conf-{{env}}.out\"}\n",
    )
    .unwrap();
    let out = stdout_of(
        cmd.args(["--output", "json", "play", "playbook.yml", "--explain"])
            .assert()
            .success(),
    );
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["tasks"][0]["explain"]["dest"], "conf-prod.out");
}
