use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct Config {
    #[serde(default)]
    pub default: DefaultSection,
    #[serde(default)]
    pub profile: HashMap<String, Profile>,
    #[serde(default)]
    pub server: HashMap<String, Server>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct DefaultSection {
    pub output: String,
    pub color: bool,
}

impl Default for DefaultSection {
    fn default() -> Self {
        Self {
            output: "plain".to_string(),
            color: true,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct Profile {
    pub base_url: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct Server {
    pub host: String,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub key: Option<String>,
    pub ssl_dir: Option<String>,
}

pub fn config_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".tooler")
        .join("config.toml")
}

pub fn load() -> Result<Config> {
    let path = config_path();
    if !path.exists() {
        return Ok(Config::default());
    }
    let content = std::fs::read_to_string(&path)?;
    let config: Config = toml::from_str(&content)?;
    migrate_legacy_tokens(&content, &config)?;
    Ok(config)
}

/// Older versions stored profile tokens as plaintext `token = "..."` under
/// `[profile.<name>]`. `Profile` no longer has that field, so it's silently ignored by
/// serde on load -- and would be permanently dropped on the next `save()`. Move any such
/// tokens into the OS keychain and rewrite the file without them before that can happen.
fn migrate_legacy_tokens(raw_content: &str, config: &Config) -> Result<()> {
    let raw: toml::Value = toml::from_str(raw_content)?;
    let Some(profiles) = raw.get("profile").and_then(|v| v.as_table()) else {
        return Ok(());
    };

    let mut migrated = Vec::new();
    for (name, value) in profiles {
        if let Some(token) = value.get("token").and_then(|v| v.as_str()) {
            crate::secrets::set_token(name, token).with_context(|| {
                format!(
                    "Found a legacy plaintext token for profile '{name}' in {} but could not \
                     migrate it to the OS keychain. Fix keychain access and retry, or remove \
                     the 'token' line for this profile manually.",
                    config_path().display()
                )
            })?;
            migrated.push(name.clone());
        }
    }

    if !migrated.is_empty() {
        save(config)?;
        eprintln!(
            "tooler: migrated plaintext token(s) for profile(s) {} from config.toml to the OS keychain",
            migrated.join(", ")
        );
    }
    Ok(())
}

pub fn save(config: &Config) -> Result<()> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, toml::to_string_pretty(config)?)?;
    Ok(())
}
