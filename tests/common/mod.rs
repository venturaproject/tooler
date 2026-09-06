// Shared test helpers: not every integration-test binary that includes this module uses
// every function in it (each `tests/*.rs` file is compiled as its own separate binary),
// so an unused item here is expected, not a real dead-code smell.
#![allow(dead_code)]

use assert_cmd::Command;
use std::path::Path;
use tempfile::TempDir;

/// A fresh, isolated `Command` + its backing `TempDir` (kept alive for the test's
/// duration; auto-cleaned on drop). `HOME`/`TOOLER_HOME` and the working directory are
/// all pointed at the temp dir, so `~/.tooler/config.toml`, `.tooler.toml`, and
/// `playbooks/` all resolve inside it — never touching the real user's config or this
/// repo's own files.
pub fn tooler() -> (Command, TempDir) {
    let dir = tempfile::tempdir().expect("create temp dir");
    (tooler_in(dir.path()), dir)
}

/// A fresh `Command` (assert_cmd's `Command` is single-use) targeting an
/// already-created isolated directory — for multi-step tests that need several
/// invocations against the same `TempDir`.
pub fn tooler_in(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("tooler").expect("find tooler binary");
    cmd.env("HOME", dir)
        // `HOME` alone isn't enough on Windows: `dirs::home_dir()` there resolves via
        // `SHGetKnownFolderPath` directly and never consults `HOME`/`USERPROFILE` at
        // all, so without this every test would silently read/write the real runner's
        // profile directory instead of this isolated one (see `config::tooler_dir`).
        .env("TOOLER_HOME", dir.join(".tooler"))
        .current_dir(dir);
    cmd
}
