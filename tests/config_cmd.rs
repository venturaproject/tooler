mod common;

use common::tooler_in;
use tempfile::tempdir;

fn stdout_of(assert: assert_cmd::assert::Assert) -> String {
    String::from_utf8_lossy(&assert.get_output().stdout).to_string()
}

#[test]
fn set_and_get_default_output() {
    let dir = tempdir().unwrap();
    tooler_in(dir.path())
        .args(["config", "set", "default.output", "json"])
        .assert()
        .success();
    let out = stdout_of(
        tooler_in(dir.path())
            .args(["config", "get", "default.output"])
            .assert()
            .success(),
    );
    assert!(out.contains("json"));
}

#[test]
fn set_and_get_profile_base_url() {
    let dir = tempdir().unwrap();
    tooler_in(dir.path())
        .args([
            "config",
            "set",
            "profile.staging.base_url",
            "https://staging.example.com",
        ])
        .assert()
        .success();
    let out = stdout_of(
        tooler_in(dir.path())
            .args(["config", "get", "profile.staging.base_url"])
            .assert()
            .success(),
    );
    assert!(out.contains("staging.example.com"));
}

#[test]
fn set_and_get_profile_oauth_fields() {
    let dir = tempdir().unwrap();
    tooler_in(dir.path())
        .args([
            "config",
            "set",
            "profile.exact.token_url",
            "https://example.com/oauth2/token",
        ])
        .assert()
        .success();
    tooler_in(dir.path())
        .args(["config", "set", "profile.exact.client_id", "abc123"])
        .assert()
        .success();

    let out = stdout_of(
        tooler_in(dir.path())
            .args(["config", "get", "profile.exact.client_id"])
            .assert()
            .success(),
    );
    assert!(out.contains("abc123"));
}

#[test]
fn unset_removes_a_value() {
    let dir = tempdir().unwrap();
    tooler_in(dir.path())
        .args(["config", "set", "profile.staging.base_url", "https://x"])
        .assert()
        .success();
    tooler_in(dir.path())
        .args(["config", "unset", "profile.staging.base_url"])
        .assert()
        .success();
    tooler_in(dir.path())
        .args(["config", "get", "profile.staging.base_url"])
        .assert()
        .failure();
}

// `config profiles` calls `secrets::get_token` per profile to render the `[token]`
// marker, which needs the real OS keychain (tied to the login session, not `$HOME` —
// see the plan's note on why secret-bearing paths stay out of this suite). With zero
// profiles configured that call is never reached, so this still exercises the
// list-building/empty-state logic without touching the keychain.
#[test]
fn profiles_reports_none_configured_when_empty() {
    let dir = tempdir().unwrap();
    let out = stdout_of(
        tooler_in(dir.path())
            .args(["config", "profiles"])
            .assert()
            .success(),
    );
    assert!(out.to_lowercase().contains("no profiles"));
}

#[test]
fn path_prints_the_config_file_location_inside_home() {
    if cfg!(windows) {
        eprintln!(
            "skipping path_prints_the_config_file_location_inside_home: dirs::home_dir() \
             on Windows resolves via SHGetKnownFolderPath directly, ignoring HOME/ \
             USERPROFILE env var overrides -- this suite's isolated-HOME technique (see \
             tests/common::tooler_in) can't redirect it there"
        );
        return;
    }
    let dir = tempdir().unwrap();
    let out = stdout_of(
        tooler_in(dir.path())
            .args(["config", "path"])
            .assert()
            .success(),
    );
    assert!(out.contains(dir.path().to_str().unwrap()));
    assert!(out.contains("config.toml"));
}

#[test]
fn show_reflects_a_value_that_was_set() {
    if cfg!(windows) {
        eprintln!(
            "skipping show_reflects_a_value_that_was_set: dirs::home_dir() on Windows \
             resolves via SHGetKnownFolderPath directly, ignoring HOME/USERPROFILE env \
             var overrides -- this suite's isolated-HOME technique (see \
             tests/common::tooler_in) can't redirect it there"
        );
        return;
    }
    let dir = tempdir().unwrap();
    tooler_in(dir.path())
        .args(["config", "set", "default.color", "false"])
        .assert()
        .success();
    let out = stdout_of(
        tooler_in(dir.path())
            .args(["config", "show"])
            .assert()
            .success(),
    );
    assert!(out.contains("color = false"));
}
