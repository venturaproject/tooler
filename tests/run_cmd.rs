mod common;

use common::tooler;

fn stdout_of(assert: assert_cmd::assert::Assert) -> String {
    String::from_utf8_lossy(&assert.get_output().stdout).to_string()
}

fn write_tooler_toml(dir: &std::path::Path) {
    std::fs::write(
        dir.join(".tooler.toml"),
        "[scripts]\nhello = \"echo hi-from-script\"\n",
    )
    .unwrap();
}

#[test]
fn no_args_lists_scripts() {
    let (mut cmd, dir) = tooler();
    write_tooler_toml(dir.path());
    let out = stdout_of(cmd.args(["run"]).assert().success());
    assert!(out.contains("hello"));
}

#[test]
fn runs_a_named_script() {
    let (mut cmd, dir) = tooler();
    write_tooler_toml(dir.path());
    let out = stdout_of(cmd.args(["run", "hello"]).assert().success());
    assert!(out.contains("hi-from-script"));
}

#[test]
fn dry_run_previews_without_executing() {
    // Plain mode always echoes the command line itself (even dry); the distinguishing
    // signal is that a *real* run additionally produces the script's own output, so
    // "hi-from-script" appears twice (echoed command + actual `echo` output) instead of
    // once.
    let (mut cmd, dir) = tooler();
    write_tooler_toml(dir.path());
    let out = stdout_of(cmd.args(["run", "hello", "--dry"]).assert().success());
    assert_eq!(out.matches("hi-from-script").count(), 1);
}

#[test]
fn unknown_script_fails() {
    let (mut cmd, dir) = tooler();
    write_tooler_toml(dir.path());
    cmd.args(["run", "nonexistent"]).assert().failure();
}
