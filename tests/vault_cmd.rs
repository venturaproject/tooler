mod common;

use common::{tooler, tooler_in};

#[test]
fn encrypt_then_decrypt_round_trips_back_to_the_original_content() {
    let (mut cmd, dir) = tooler();
    let file = dir.path().join("secrets.yml");
    std::fs::write(&file, "api_key: super-secret\n").unwrap();

    cmd.env("TOOLER_VAULT_PASSWORD", "hunter2")
        .args(["vault", "encrypt", "secrets.yml"])
        .assert()
        .success();

    let encrypted = std::fs::read_to_string(&file).unwrap();
    assert!(
        encrypted.starts_with("TOOLERVAULT;1;AES256GCM\n"),
        "content was: {encrypted}"
    );

    tooler_in(dir.path())
        .env("TOOLER_VAULT_PASSWORD", "hunter2")
        .args(["vault", "decrypt", "secrets.yml"])
        .assert()
        .success();

    let decrypted = std::fs::read_to_string(&file).unwrap();
    assert_eq!(decrypted, "api_key: super-secret\n");
}

#[test]
fn encrypting_an_already_encrypted_file_fails_clearly() {
    let (mut cmd, dir) = tooler();
    std::fs::write(dir.path().join("secrets.yml"), "api_key: x\n").unwrap();

    cmd.env("TOOLER_VAULT_PASSWORD", "pw")
        .args(["vault", "encrypt", "secrets.yml"])
        .assert()
        .success();

    let out = tooler_in(dir.path())
        .env("TOOLER_VAULT_PASSWORD", "pw")
        .args(["vault", "encrypt", "secrets.yml"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    assert!(
        stderr.contains("already vault-encrypted"),
        "stderr was: {stderr}"
    );
}

#[test]
fn decrypting_a_plain_file_fails_clearly() {
    let (mut cmd, dir) = tooler();
    std::fs::write(dir.path().join("plain.yml"), "host: example.com\n").unwrap();

    let out = cmd
        .env("TOOLER_VAULT_PASSWORD", "pw")
        .args(["vault", "decrypt", "plain.yml"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    assert!(
        stderr.contains("is not vault-encrypted"),
        "stderr was: {stderr}"
    );
}

#[test]
fn view_prints_the_decrypted_content_without_modifying_the_file() {
    let (mut cmd, dir) = tooler();
    let file = dir.path().join("secrets.yml");
    std::fs::write(&file, "api_key: super-secret\n").unwrap();

    cmd.env("TOOLER_VAULT_PASSWORD", "pw")
        .args(["vault", "encrypt", "secrets.yml"])
        .assert()
        .success();
    let encrypted_before = std::fs::read_to_string(&file).unwrap();

    let out = tooler_in(dir.path())
        .env("TOOLER_VAULT_PASSWORD", "pw")
        .args(["vault", "view", "secrets.yml"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).to_string();
    assert!(
        stdout.contains("api_key: super-secret"),
        "stdout was: {stdout}"
    );

    let encrypted_after = std::fs::read_to_string(&file).unwrap();
    assert_eq!(
        encrypted_before, encrypted_after,
        "view: must not modify the file on disk"
    );
}

#[test]
fn rekey_rotates_the_passphrase_and_the_old_one_no_longer_works() {
    let (mut cmd, dir) = tooler();
    let file = dir.path().join("secrets.yml");
    std::fs::write(&file, "api_key: super-secret\n").unwrap();

    cmd.env("TOOLER_VAULT_PASSWORD", "old-pw")
        .args(["vault", "encrypt", "secrets.yml"])
        .assert()
        .success();

    tooler_in(dir.path())
        .env("TOOLER_VAULT_PASSWORD", "old-pw")
        .env("NEW_PW", "new-pw")
        .args([
            "vault",
            "rekey",
            "secrets.yml",
            "--new-password-env",
            "NEW_PW",
        ])
        .assert()
        .success();

    // Old password no longer decrypts it.
    let out = tooler_in(dir.path())
        .env("TOOLER_VAULT_PASSWORD", "old-pw")
        .args(["vault", "decrypt", "secrets.yml"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    assert!(stderr.contains("wrong password"), "stderr was: {stderr}");

    // New password does, and recovers the original plaintext.
    tooler_in(dir.path())
        .env("TOOLER_VAULT_PASSWORD", "new-pw")
        .args(["vault", "decrypt", "secrets.yml"])
        .assert()
        .success();
    let decrypted = std::fs::read_to_string(&file).unwrap();
    assert_eq!(decrypted, "api_key: super-secret\n");
}

#[test]
fn rekeying_a_plain_file_fails_clearly() {
    let (mut cmd, dir) = tooler();
    std::fs::write(dir.path().join("plain.yml"), "host: example.com\n").unwrap();

    let out = cmd
        .env("NEW_PW", "new-pw")
        .args([
            "vault",
            "rekey",
            "plain.yml",
            "--new-password-env",
            "NEW_PW",
        ])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    assert!(
        stderr.contains("is not vault-encrypted"),
        "stderr was: {stderr}"
    );
}

#[test]
fn encrypt_without_the_password_env_var_set_fails_clearly() {
    let (mut cmd, dir) = tooler();
    std::fs::write(dir.path().join("secrets.yml"), "api_key: x\n").unwrap();

    let out = cmd
        .env_remove("TOOLER_VAULT_PASSWORD")
        .args(["vault", "encrypt", "secrets.yml"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    assert!(
        stderr.contains("TOOLER_VAULT_PASSWORD"),
        "stderr was: {stderr}"
    );
}
