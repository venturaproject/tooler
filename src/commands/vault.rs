//! A lightweight, `ansible-vault`-alike file encryption CLI (`tooler vault
//! encrypt/decrypt/view`) — lets a `vars_files:`/`--vars-file` entry be committed to a
//! repo encrypted at rest instead of plaintext, without needing `gpg`/`ansible-vault` or
//! any other external binary on either the control machine or a remote target: AES-256-
//! GCM (via the `aes-gcm` crate) with an Argon2id-derived key (via the `argon2` crate),
//! both pure Rust. See `crate::commands::play::load_vars_file` for the transparent
//! decrypt-on-load integration.
use crate::{context::Context, output::OutputFormat};
use aes_gcm::{
    Aes256Gcm, Key, Nonce,
    aead::{Aead, KeyInit, OsRng, rand_core::RngCore},
};
use anyhow::{Context as _, Result, anyhow, bail};
use argon2::Argon2;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use clap::{Args, Subcommand};
use colored::Colorize;
use std::path::{Path, PathBuf};

/// First line of an encrypted file — how `is_vault_encrypted` recognizes one, the same
/// "sniff a magic header" approach `ansible-vault` uses.
const MAGIC: &str = "TOOLERVAULT;1;AES256GCM";
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;
const KEY_LEN: usize = 32;

#[derive(Args)]
pub struct VaultArgs {
    #[command(subcommand)]
    pub subcommand: VaultSubcommand,
}

#[derive(Subcommand)]
pub enum VaultSubcommand {
    /// Encrypt a file in place (fails if it's already vault-encrypted)
    Encrypt {
        file: PathBuf,
        /// Env var holding the passphrase
        #[arg(long, default_value = "TOOLER_VAULT_PASSWORD")]
        password_env: String,
    },
    /// Decrypt a file in place (fails if it isn't vault-encrypted)
    Decrypt {
        file: PathBuf,
        #[arg(long, default_value = "TOOLER_VAULT_PASSWORD")]
        password_env: String,
    },
    /// Print a file's decrypted contents to stdout without modifying it on disk
    View {
        file: PathBuf,
        #[arg(long, default_value = "TOOLER_VAULT_PASSWORD")]
        password_env: String,
    },
    /// Rotate a vault-encrypted file's passphrase in place -- decrypts with the old one
    /// and re-encrypts with a new one; the plaintext only ever exists in memory, never
    /// written to disk in between. Fails if the file isn't already vault-encrypted.
    Rekey {
        file: PathBuf,
        /// Env var holding the current passphrase
        #[arg(long, default_value = "TOOLER_VAULT_PASSWORD")]
        old_password_env: String,
        /// Env var holding the new passphrase
        #[arg(long)]
        new_password_env: String,
    },
}

pub fn run(args: VaultArgs, ctx: &Context) -> Result<()> {
    let json = ctx.output == OutputFormat::Json;
    match args.subcommand {
        VaultSubcommand::Encrypt { file, password_env } => encrypt_file(&file, &password_env, json),
        VaultSubcommand::Decrypt { file, password_env } => decrypt_file(&file, &password_env, json),
        VaultSubcommand::View { file, password_env } => view_file(&file, &password_env, json),
        VaultSubcommand::Rekey {
            file,
            old_password_env,
            new_password_env,
        } => rekey_file(&file, &old_password_env, &new_password_env, json),
    }
}

fn read_password(env_var: &str) -> Result<String> {
    std::env::var(env_var)
        .with_context(|| format!("set {env_var} to the vault passphrase (env var not set)"))
}

fn encrypt_file(path: &Path, password_env: &str, json: bool) -> Result<()> {
    let content = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    if is_vault_encrypted(&content) {
        bail!("{} is already vault-encrypted", path.display());
    }
    let password = read_password(password_env)?;
    let encrypted = encrypt(&content, &password)?;
    std::fs::write(path, encrypted.as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    if json {
        println!(
            "{}",
            serde_json::json!({ "path": path.display().to_string(), "encrypted": true })
        );
    } else {
        println!("{} {}", "✓ encrypted".green().bold(), path.display());
    }
    Ok(())
}

fn decrypt_file(path: &Path, password_env: &str, json: bool) -> Result<()> {
    let content = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    if !is_vault_encrypted(&content) {
        bail!("{} is not vault-encrypted", path.display());
    }
    let password = read_password(password_env)?;
    let plaintext = decrypt(&content, &password)?;
    std::fs::write(path, &plaintext).with_context(|| format!("writing {}", path.display()))?;
    if json {
        println!(
            "{}",
            serde_json::json!({ "path": path.display().to_string(), "decrypted": true })
        );
    } else {
        println!("{} {}", "✓ decrypted".green().bold(), path.display());
    }
    Ok(())
}

fn view_file(path: &Path, password_env: &str, json: bool) -> Result<()> {
    let content = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    if !is_vault_encrypted(&content) {
        bail!("{} is not vault-encrypted", path.display());
    }
    let password = read_password(password_env)?;
    let plaintext = decrypt(&content, &password)?;
    let text = String::from_utf8_lossy(&plaintext);
    if json {
        println!(
            "{}",
            serde_json::json!({ "path": path.display().to_string(), "content": text })
        );
    } else {
        print!("{text}");
    }
    Ok(())
}

fn rekey_file(
    path: &Path,
    old_password_env: &str,
    new_password_env: &str,
    json: bool,
) -> Result<()> {
    let content = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    if !is_vault_encrypted(&content) {
        bail!(
            "{} is not vault-encrypted (nothing to rekey)",
            path.display()
        );
    }
    let old_password = read_password(old_password_env)?;
    let plaintext = decrypt(&content, &old_password)?;
    let new_password = read_password(new_password_env)?;
    let reencrypted = encrypt(&plaintext, &new_password)?;
    std::fs::write(path, reencrypted.as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    if json {
        println!(
            "{}",
            serde_json::json!({ "path": path.display().to_string(), "rekeyed": true })
        );
    } else {
        println!("{} {}", "✓ rekeyed".green().bold(), path.display());
    }
    Ok(())
}

/// True when `content` starts with the vault magic header on its own line — the only
/// signal `load_vars_file`/`decrypt` use to tell an encrypted file from a plain one.
pub fn is_vault_encrypted(content: &[u8]) -> bool {
    content
        .split(|&b| b == b'\n')
        .next()
        .is_some_and(|line| line == MAGIC.as_bytes())
}

/// Encrypts `plaintext` under `password`, returning the full file content to write
/// (magic header line + base64 body line). A fresh random salt and nonce are generated
/// every call, so encrypting the same plaintext twice never produces the same output.
pub fn encrypt(plaintext: &[u8], password: &str) -> Result<String> {
    let mut salt = [0u8; SALT_LEN];
    OsRng.fill_bytes(&mut salt);
    let mut nonce_bytes = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce_bytes);

    let key = derive_key(password, &salt)?;
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce_bytes), plaintext)
        .map_err(|_| anyhow!("encryption failed"))?;

    let mut blob = Vec::with_capacity(SALT_LEN + NONCE_LEN + ciphertext.len());
    blob.extend_from_slice(&salt);
    blob.extend_from_slice(&nonce_bytes);
    blob.extend_from_slice(&ciphertext);

    Ok(format!("{MAGIC}\n{}\n", STANDARD.encode(blob)))
}

/// Decrypts a full vault file's raw bytes (header + body, as `encrypt` produced) back to
/// the original plaintext. Fails clearly on a wrong password, a truncated/corrupted body,
/// or a file that isn't vault-encrypted at all.
pub fn decrypt(file_content: &[u8], password: &str) -> Result<Vec<u8>> {
    if !is_vault_encrypted(file_content) {
        bail!("not a tooler vault file (missing {MAGIC} header)");
    }
    let text = std::str::from_utf8(file_content).context("vault file is not valid UTF-8")?;
    let body = text
        .lines()
        .nth(1)
        .ok_or_else(|| anyhow!("vault file is missing its encrypted body line"))?;
    let blob = STANDARD
        .decode(body.trim())
        .context("vault file's body isn't valid base64")?;
    if blob.len() < SALT_LEN + NONCE_LEN {
        bail!("vault file's encrypted body is too short to be valid");
    }
    let (salt, rest) = blob.split_at(SALT_LEN);
    let (nonce_bytes, ciphertext) = rest.split_at(NONCE_LEN);

    let key = derive_key(password, salt)?;
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
    cipher
        .decrypt(Nonce::from_slice(nonce_bytes), ciphertext)
        .map_err(|_| anyhow!("decryption failed — wrong password, or the file is corrupted"))
}

fn derive_key(password: &str, salt: &[u8]) -> Result<[u8; KEY_LEN]> {
    let mut key = [0u8; KEY_LEN];
    Argon2::default()
        .hash_password_into(password.as_bytes(), salt, &mut key)
        .map_err(|e| anyhow!("key derivation failed: {e}"))?;
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypt_then_decrypt_recovers_the_exact_original_bytes() {
        let plaintext = b"api_key: super-secret-value\nhost: example.com\n";
        let file = encrypt(plaintext, "correct horse battery staple").unwrap();
        let recovered = decrypt(file.as_bytes(), "correct horse battery staple").unwrap();
        assert_eq!(recovered, plaintext);
    }

    /// Exercises the same decrypt-then-re-encrypt sequence `rekey_file` performs (tested
    /// here at the pure-function level rather than through `rekey_file`/env vars, since
    /// `std::env::var` is process-wide state that unit tests running in parallel
    /// shouldn't share -- the CLI-level round trip is covered in `tests/vault_cmd.rs`).
    #[test]
    fn rekeying_changes_the_passphrase_while_preserving_the_plaintext() {
        let plaintext = b"api_key: rotate-me\n";
        let encrypted_a = encrypt(plaintext, "password-a").unwrap();
        let recovered = decrypt(encrypted_a.as_bytes(), "password-a").unwrap();
        let encrypted_b = encrypt(&recovered, "password-b").unwrap();

        assert_eq!(
            decrypt(encrypted_b.as_bytes(), "password-b").unwrap(),
            plaintext
        );
        let err = decrypt(encrypted_b.as_bytes(), "password-a").unwrap_err();
        assert!(
            err.to_string().contains("wrong password"),
            "error was: {err}"
        );
    }

    #[test]
    fn decrypt_with_the_wrong_password_fails_clearly() {
        let file = encrypt(b"secret", "right-password").unwrap();
        let err = decrypt(file.as_bytes(), "wrong-password").unwrap_err();
        assert!(
            err.to_string().contains("wrong password"),
            "error was: {err}"
        );
    }

    #[test]
    fn is_vault_encrypted_distinguishes_encrypted_from_plain_content() {
        let file = encrypt(b"secret", "pw").unwrap();
        assert!(is_vault_encrypted(file.as_bytes()));
        assert!(!is_vault_encrypted(b"host: example.com\n"));
        assert!(!is_vault_encrypted(b""));
    }

    #[test]
    fn encrypting_the_same_plaintext_twice_produces_different_ciphertext() {
        let a = encrypt(b"same plaintext", "pw").unwrap();
        let b = encrypt(b"same plaintext", "pw").unwrap();
        assert_ne!(a, b, "fresh salt/nonce should make every encryption unique");
        // Both still decrypt back to the same original.
        assert_eq!(decrypt(a.as_bytes(), "pw").unwrap(), b"same plaintext");
        assert_eq!(decrypt(b.as_bytes(), "pw").unwrap(), b"same plaintext");
    }

    #[test]
    fn decrypt_rejects_a_file_with_no_vault_header() {
        let err = decrypt(b"host: example.com\n", "pw").unwrap_err();
        assert!(
            err.to_string().contains("not a tooler vault file"),
            "error was: {err}"
        );
    }
}
