use anyhow::{Context, Result};
use keyring::Entry;

const SERVICE: &str = "tooler";

fn entry(account: &str) -> Result<Entry> {
    Entry::new(SERVICE, account).context("Failed to access the OS credential store")
}

/// Reads a named secret for a profile from the OS keychain (Keychain on macOS,
/// Credential Manager on Windows, Secret Service on Linux). Returns `None` if
/// nothing has been stored under this profile/key.
pub fn get_secret(profile: &str, key: &str) -> Result<Option<String>> {
    match entry(&format!("profile:{profile}:{key}"))?.get_password() {
        Ok(value) => Ok(Some(value)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(e).context("Failed to read secret from the OS credential store"),
    }
}

pub fn set_secret(profile: &str, key: &str, value: &str) -> Result<()> {
    entry(&format!("profile:{profile}:{key}"))?
        .set_password(value)
        .context("Failed to write secret to the OS credential store")
}

pub fn delete_secret(profile: &str, key: &str) -> Result<()> {
    match entry(&format!("profile:{profile}:{key}"))?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e).context("Failed to delete secret from the OS credential store"),
    }
}

/// Reads a profile's HTTP bearer token. Returns `None` if nothing has been stored.
pub fn get_token(profile: &str) -> Result<Option<String>> {
    get_secret(profile, "token")
}

pub fn set_token(profile: &str, value: &str) -> Result<()> {
    set_secret(profile, "token", value)
}

pub fn delete_token(profile: &str) -> Result<()> {
    delete_secret(profile, "token")
}
