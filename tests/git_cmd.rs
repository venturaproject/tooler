mod common;

use common::tooler_in;
use std::process::Command as StdCommand;
use tempfile::tempdir;

fn stdout_of(assert: assert_cmd::assert::Assert) -> String {
    String::from_utf8_lossy(&assert.get_output().stdout).to_string()
}

/// Initializes a real git repo with a pinned, deterministic branch name (independent of
/// the test machine's `init.defaultBranch`) and one commit, so `summary`/`changelog`
/// have something to report.
fn init_repo() -> tempfile::TempDir {
    let dir = tempdir().unwrap();
    let git = |args: &[&str]| {
        let status = StdCommand::new("git")
            .args(args)
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    };
    git(&["init", "-b", "main"]);
    git(&["config", "user.email", "test@example.com"]);
    git(&["config", "user.name", "Test"]);
    std::fs::write(dir.path().join("README.md"), "hello\n").unwrap();
    git(&["add", "README.md"]);
    git(&["commit", "-m", "initial commit"]);
    // `git changelog` with no tags ranges from the *root* commit to HEAD, so a
    // single-commit repo always shows "no commits since" — add a second commit so
    // there's something strictly after the root for the changelog to report.
    std::fs::write(dir.path().join("feature.md"), "feature\n").unwrap();
    git(&["add", "feature.md"]);
    git(&["commit", "-m", "feat: add a feature"]);
    dir
}

#[test]
fn summary_reports_the_current_branch() {
    let dir = init_repo();
    let out = stdout_of(
        tooler_in(dir.path())
            .args(["--output", "json", "git", "summary"])
            .assert()
            .success(),
    );
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(value["branch"], "main");
    assert_eq!(value["clean"], true);
}

#[test]
fn changelog_lists_commits_after_the_root() {
    let dir = init_repo();
    let out = stdout_of(
        tooler_in(dir.path())
            .args(["git", "changelog"])
            .assert()
            .success(),
    );
    assert!(out.contains("add a feature"));
}

#[test]
fn clean_preview_reports_nothing_to_delete() {
    let dir = init_repo();
    // No --confirm, no merged branches to delete: a pure, side-effect-free preview.
    tooler_in(dir.path())
        .args(["git", "clean"])
        .assert()
        .success();
}
