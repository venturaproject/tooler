use anyhow::{Context, Result};
use keyring::Entry;

const SERVICE: &str = "tooler";

fn entry(account: &str) -> Result<Entry> {
    Entry::new(SERVICE, account).context("Failed to access the OS credential store")
}

/// Reads a profile's HTTP bearer token from the OS keychain (Keychain on macOS,
/// Credential Manager on Windows, Secret Service on Linux). Returns `None` if
/// nothing has been stored for this profile.
pub fn get_token(profile: &str) -> Result<Option<String>> {
    match entry(&format!("profile:{profile}:token"))?.get_password() {
        Ok(value) => Ok(Some(value)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(e).context("Failed to read token from the OS credential store"),
    }
}

pub fn set_token(profile: &str, value: &str) -> Result<()> {
    entry(&format!("profile:{profile}:token"))?
        .set_password(value)
        .context("Failed to write token to the OS credential store")
}

pub fn delete_token(profile: &str) -> Result<()> {
    match entry(&format!("profile:{profile}:token"))?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e).context("Failed to delete token from the OS credential store"),
    }
}
