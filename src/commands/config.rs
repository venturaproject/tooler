use crate::{config, context::Context, output::OutputFormat, secrets};
use anyhow::{Result, bail};
use clap::{Args, Subcommand};
use colored::Colorize;

#[derive(Args)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub subcommand: ConfigSubcommand,
}

#[derive(Subcommand)]
pub enum ConfigSubcommand {
    /// Show full configuration
    Show,
    /// Get a value by key (e.g. default.output, profile.staging.base_url, profile.staging.token)
    Get { key: String },
    /// Set a value by key (e.g. default.output json, profile.staging.token secret123).
    /// Profile tokens are stored encrypted in the OS keychain, never in the config file.
    Set { key: String, value: String },
    /// List configured profiles
    Profiles,
    /// Print the config file path
    Path,
    /// Unset a value by key (e.g. profile.staging.token)
    Unset { key: String },
}

/// Splits a "profile.<name>.<field>" key into (name, field).
fn parse_profile_key(key: &str) -> Option<(&str, &str)> {
    let rest = key.strip_prefix("profile.")?;
    rest.split_once('.')
}

pub fn run(args: ConfigArgs, ctx: &Context) -> Result<()> {
    let json = ctx.output == OutputFormat::Json;
    match args.subcommand {
        ConfigSubcommand::Show => {
            if json {
                println!("{}", serde_json::to_string_pretty(&ctx.config)?);
            } else {
                println!("{}", toml::to_string_pretty(&ctx.config)?);
            }
        }
        ConfigSubcommand::Path => {
            let path = config::config_path();
            if json {
                println!(
                    "{}",
                    serde_json::json!({"path": path.display().to_string()})
                );
            } else {
                println!("{}", path.display());
            }
        }
        ConfigSubcommand::Profiles => {
            let mut names: Vec<&String> = ctx.config.profile.keys().collect();
            names.sort();

            if json {
                let profiles: Vec<_> = names
                    .iter()
                    .map(|name| -> Result<serde_json::Value> {
                        let has_token = secrets::get_token(name)?.is_some();
                        Ok(serde_json::json!({
                            "name": name,
                            "base_url": ctx.config.profile.get(*name).and_then(|p| p.base_url.clone()),
                            "has_token": has_token,
                        }))
                    })
                    .collect::<Result<Vec<_>>>()?;
                println!(
                    "{}",
                    serde_json::json!({"profiles": profiles, "active": ctx.profile})
                );
                return Ok(());
            }
            if names.is_empty() {
                println!("{}", "No profiles configured.".dimmed());
            } else {
                for name in names {
                    let marker = if *name == ctx.profile {
                        " (active)".dimmed().to_string()
                    } else {
                        String::new()
                    };
                    let token_marker = if secrets::get_token(name)?.is_some() {
                        " [token]".dimmed().to_string()
                    } else {
                        String::new()
                    };
                    println!("  {}{}{}", name.cyan(), marker, token_marker);
                }
            }
        }
        ConfigSubcommand::Get { key } => {
            if let Some((name, field)) = parse_profile_key(&key) {
                let value = match field {
                    "base_url" => ctx
                        .config
                        .profile
                        .get(name)
                        .and_then(|p| p.base_url.clone())
                        .ok_or_else(|| anyhow::anyhow!("No base_url set for profile '{name}'"))?,
                    "token" => secrets::get_token(name)?.ok_or_else(|| {
                        anyhow::anyhow!(
                            "No token set for profile '{name}' (or the OS keychain is locked)"
                        )
                    })?,
                    _ => bail!(
                        "Unknown profile key '{}'. Available: base_url, token",
                        field
                    ),
                };
                if json {
                    println!("{}", serde_json::json!({"key": key, "value": value}));
                } else {
                    println!("{value}");
                }
                return Ok(());
            }
            let value = match key.as_str() {
                "default.output" => ctx.config.default.output.clone(),
                "default.color" => ctx.config.default.color.to_string(),
                _ => bail!(
                    "Unknown key '{}'. Available: default.output, default.color, profile.<name>.base_url, profile.<name>.token",
                    key
                ),
            };
            if json {
                println!("{}", serde_json::json!({"key": key, "value": value}));
            } else {
                println!("{value}");
            }
        }
        ConfigSubcommand::Set { key, value } => {
            if let Some((name, field)) = parse_profile_key(&key) {
                match field {
                    "base_url" => {
                        let mut cfg = config::load()?;
                        cfg.profile.entry(name.to_string()).or_default().base_url =
                            Some(value.clone());
                        config::save(&cfg)?;
                        if json {
                            println!("{}", serde_json::json!({"key": key, "value": value}));
                        } else {
                            println!("{} {} = {}", "set".green().bold(), key.cyan(), value);
                        }
                    }
                    "token" => {
                        secrets::set_token(name, &value)?;
                        // Ensure the profile is registered in the config file (with no
                        // base_url) so it shows up in `config profiles` / `show`.
                        let mut cfg = config::load()?;
                        cfg.profile.entry(name.to_string()).or_default();
                        config::save(&cfg)?;
                        if json {
                            println!(
                                "{}",
                                serde_json::json!({"key": key, "value": "***", "stored": "os keychain"})
                            );
                        } else {
                            println!(
                                "{} {} = {} {}",
                                "set".green().bold(),
                                key.cyan(),
                                "***".dimmed(),
                                "(stored in OS keychain)".dimmed()
                            );
                        }
                    }
                    _ => bail!(
                        "Unknown profile key '{}'. Available: base_url, token",
                        field
                    ),
                }
                return Ok(());
            }
            let mut cfg = config::load()?;
            match key.as_str() {
                "default.output" => {
                    if !["plain", "json", "table"].contains(&value.as_str()) {
                        bail!("Invalid value. Use: plain, json, table");
                    }
                    cfg.default.output = value.clone();
                }
                "default.color" => {
                    cfg.default.color = value
                        .parse()
                        .map_err(|_| anyhow::anyhow!("Use 'true' or 'false'"))?;
                }
                _ => bail!(
                    "Unknown key '{}'. Available: default.output, default.color, profile.<name>.base_url, profile.<name>.token",
                    key
                ),
            }
            config::save(&cfg)?;
            if json {
                println!("{}", serde_json::json!({"key": key, "value": value}));
            } else {
                println!("{} {} = {}", "set".green().bold(), key.cyan(), value);
            }
        }
        ConfigSubcommand::Unset { key } => {
            if let Some((name, field)) = parse_profile_key(&key) {
                match field {
                    "base_url" => {
                        let mut cfg = config::load()?;
                        if let Some(p) = cfg.profile.get_mut(name) {
                            p.base_url = None;
                        }
                        config::save(&cfg)?;
                    }
                    "token" => secrets::delete_token(name)?,
                    _ => bail!(
                        "Unknown profile key '{}'. Available: base_url, token",
                        field
                    ),
                }
                if json {
                    println!("{}", serde_json::json!({"key": key, "unset": true}));
                } else {
                    println!("{} {}", "unset".red().bold(), key.cyan());
                }
                return Ok(());
            }
            bail!(
                "Unknown key '{}'. Available: profile.<name>.base_url, profile.<name>.token",
                key
            );
        }
    }
    Ok(())
}
