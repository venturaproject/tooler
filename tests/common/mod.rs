// Shared test helpers: not every integration-test binary that includes this module uses
// every function in it (each `tests/*.rs` file is compiled as its own separate binary),
// so an unused item here is expected, not a real dead-code smell.
#![allow(dead_code)]

use assert_cmd::Command;
use std::path::Path;
use tempfile::TempDir;

/// A fresh, isolated `Command` + its backing `TempDir` (kept alive for the test's
/// duration; auto-cleaned on drop). `HOME` and the working directory are both pointed at
/// the temp dir, so `~/.tooler/config.toml`, `.tooler.toml`, and `playbooks/` all resolve
/// inside it — never touching the real user's config or this repo's own files.
pub fn tooler() -> (Command, TempDir) {
    let dir = tempfile::tempdir().expect("create temp dir");
    (tooler_in(dir.path()), dir)
}

/// A fresh `Command` (assert_cmd's `Command` is single-use) targeting an
/// already-created isolated directory — for multi-step tests that need several
/// invocations against the same `TempDir`.
pub fn tooler_in(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("tooler").expect("find tooler binary");
    cmd.env("HOME", dir).current_dir(dir);
    cmd
}
