mod common;

use common::tooler;

// This suite deliberately makes no live Adzuna calls and never touches the real OS
// keychain — same reasoning `config_cmd.rs` documents for secret-bearing paths: the
// keychain is tied to the login session, not `$HOME`, so it can't be sandboxed by a
// TempDir the way config files can. Real end-to-end search is a manual verification
// step once a real Adzuna key is available (see the plan).

#[test]
fn search_fails_cleanly_without_credentials() {
    let (mut cmd, _dir) = tooler();
    let assert = cmd
        .env_remove("TOOLER_ADZUNA_APP_ID")
        .env_remove("TOOLER_ADZUNA_APP_KEY")
        .args([
            "--profile",
            "jobs_cmd_test_no_creds",
            "jobs",
            "search",
            "--what",
            "developer",
            "--where",
            "madrid",
        ])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();
    assert!(
        stderr.contains("tooler jobs configure"),
        "stderr was: {stderr}"
    );
}

// Credential-precedence behavior (explicit > env > stored secret) is covered by the
// in-process unit tests in `src/commands/jobs.rs`, which can call `secrets::delete_secret`
// directly for cleanup. Exercising it here would require either a live Adzuna call or a
// real keychain write this suite avoids on principle (see module comment above).
