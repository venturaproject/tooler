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
    /// Get a value by key (e.g. default.output, profile.staging.base_url, profile.staging.token,
    /// profile.staging.token_url, profile.staging.client_id, profile.staging.client_secret,
    /// profile.staging.refresh_token, mail.notify.host, mail.notify.password)
    Get { key: String },
    /// Set a value by key (e.g. default.output json, profile.staging.token secret123,
    /// mail.notify.host mail16.serv00.com). Profile tokens/client_secret/refresh_token and
    /// mail.<name>.password are stored encrypted in the OS keychain, never in the config
    /// file. Setting profile.<name>.token_url marks a profile as OAuth2-managed: `tooler
    /// http` then refreshes and caches an access token from the configured refresh_token
    /// instead of using a static token.
    Set { key: String, value: String },
    /// List configured profiles
    Profiles,
    /// Print the config file path
    Path,
    /// Unset a value by key (e.g. profile.staging.token)
    Unset { key: String },
}

/// Splits a "profile.<name>.<field>" key into (name, field).
pub(crate) fn parse_profile_key(key: &str) -> Option<(&str, &str)> {
    let rest = key.strip_prefix("profile.")?;
    rest.split_once('.')
}

/// Splits a "mail.<name>.<field>" key into (name, field).
fn parse_mail_key(key: &str) -> Option<(&str, &str)> {
    let rest = key.strip_prefix("mail.")?;
    rest.split_once('.')
}

const MAIL_KEYS: &str = "host, port, user, from, tls, imap_host, imap_port, password";

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
                        let profile = ctx.config.profile.get(*name);
                        Ok(serde_json::json!({
                            "name": name,
                            "base_url": profile.and_then(|p| p.base_url.clone()),
                            "has_token": has_token,
                            "token_url": profile.and_then(|p| p.token_url.clone()),
                            "client_id": profile.and_then(|p| p.client_id.clone()),
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
                    let oauth_marker = if ctx
                        .config
                        .profile
                        .get(name)
                        .and_then(|p| p.token_url.as_ref())
                        .is_some()
                    {
                        " [oauth2]".dimmed().to_string()
                    } else {
                        String::new()
                    };
                    println!(
                        "  {}{}{}{}",
                        name.cyan(),
                        marker,
                        token_marker,
                        oauth_marker
                    );
                }
            }
        }
        ConfigSubcommand::Get { key } => {
            if let Some((name, field)) = parse_mail_key(&key) {
                let value = match field {
                    "host" => ctx
                        .config
                        .mail
                        .get(name)
                        .map(|m| m.host.clone())
                        .ok_or_else(|| anyhow::anyhow!("No host set for mail profile '{name}'"))?,
                    "port" => ctx
                        .config
                        .mail
                        .get(name)
                        .map(|m| m.port.to_string())
                        .ok_or_else(|| anyhow::anyhow!("No port set for mail profile '{name}'"))?,
                    "user" => ctx
                        .config
                        .mail
                        .get(name)
                        .map(|m| m.user.clone())
                        .ok_or_else(|| anyhow::anyhow!("No user set for mail profile '{name}'"))?,
                    "from" => ctx
                        .config
                        .mail
                        .get(name)
                        .and_then(|m| m.from.clone())
                        .ok_or_else(|| anyhow::anyhow!("No from set for mail profile '{name}'"))?,
                    "tls" => ctx
                        .config
                        .mail
                        .get(name)
                        .and_then(|m| m.tls.clone())
                        .ok_or_else(|| anyhow::anyhow!("No tls set for mail profile '{name}'"))?,
                    "imap_host" => ctx
                        .config
                        .mail
                        .get(name)
                        .and_then(|m| m.imap_host.clone())
                        .ok_or_else(|| {
                            anyhow::anyhow!("No imap_host set for mail profile '{name}'")
                        })?,
                    "imap_port" => ctx
                        .config
                        .mail
                        .get(name)
                        .and_then(|m| m.imap_port)
                        .map(|p| p.to_string())
                        .ok_or_else(|| {
                            anyhow::anyhow!("No imap_port set for mail profile '{name}'")
                        })?,
                    "password" => secrets::get_secret(&format!("mail:{name}"), "password")?
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "No password set for mail profile '{name}' (or the OS keychain is locked)"
                            )
                        })?,
                    _ => bail!("Unknown mail profile key '{}'. Available: {}", field, MAIL_KEYS),
                };
                if json {
                    println!("{}", serde_json::json!({"key": key, "value": value}));
                } else {
                    println!("{value}");
                }
                return Ok(());
            }
            if let Some((name, field)) = parse_profile_key(&key) {
                let value = match field {
                    "base_url" => ctx
                        .config
                        .profile
                        .get(name)
                        .and_then(|p| p.base_url.clone())
                        .ok_or_else(|| anyhow::anyhow!("No base_url set for profile '{name}'"))?,
                    "token_url" => ctx
                        .config
                        .profile
                        .get(name)
                        .and_then(|p| p.token_url.clone())
                        .ok_or_else(|| anyhow::anyhow!("No token_url set for profile '{name}'"))?,
                    "client_id" => ctx
                        .config
                        .profile
                        .get(name)
                        .and_then(|p| p.client_id.clone())
                        .ok_or_else(|| anyhow::anyhow!("No client_id set for profile '{name}'"))?,
                    "token" => secrets::get_token(name)?.ok_or_else(|| {
                        anyhow::anyhow!(
                            "No token set for profile '{name}' (or the OS keychain is locked)"
                        )
                    })?,
                    "client_secret" => secrets::get_secret(name, "client_secret")?.ok_or_else(|| {
                        anyhow::anyhow!(
                            "No client_secret set for profile '{name}' (or the OS keychain is locked)"
                        )
                    })?,
                    "refresh_token" => secrets::get_secret(name, "refresh_token")?.ok_or_else(|| {
                        anyhow::anyhow!(
                            "No refresh_token set for profile '{name}' (or the OS keychain is locked)"
                        )
                    })?,
                    _ => bail!(
                        "Unknown profile key '{}'. Available: base_url, token, token_url, client_id, client_secret, refresh_token",
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
                    "Unknown key '{}'. Available: default.output, default.color, profile.<name>.base_url, profile.<name>.token, profile.<name>.token_url, profile.<name>.client_id, profile.<name>.client_secret, profile.<name>.refresh_token",
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
            if let Some((name, field)) = parse_mail_key(&key) {
                match field {
                    "host" => {
                        let mut cfg = ctx.config.clone();
                        cfg.mail.entry(name.to_string()).or_default().host = value.clone();
                        config::save(&cfg)?;
                    }
                    "port" => {
                        let port: u16 = value
                            .parse()
                            .map_err(|_| anyhow::anyhow!("Invalid port '{}'", value))?;
                        let mut cfg = ctx.config.clone();
                        cfg.mail.entry(name.to_string()).or_default().port = port;
                        config::save(&cfg)?;
                    }
                    "user" => {
                        let mut cfg = ctx.config.clone();
                        cfg.mail.entry(name.to_string()).or_default().user = value.clone();
                        config::save(&cfg)?;
                    }
                    "from" => {
                        let mut cfg = ctx.config.clone();
                        cfg.mail.entry(name.to_string()).or_default().from = Some(value.clone());
                        config::save(&cfg)?;
                    }
                    "tls" => {
                        if !["starttls", "tls", "none"].contains(&value.as_str()) {
                            bail!("Invalid tls value '{}'. Use: starttls, tls, none", value);
                        }
                        let mut cfg = ctx.config.clone();
                        cfg.mail.entry(name.to_string()).or_default().tls = Some(value.clone());
                        config::save(&cfg)?;
                    }
                    "imap_host" => {
                        let mut cfg = ctx.config.clone();
                        cfg.mail.entry(name.to_string()).or_default().imap_host =
                            Some(value.clone());
                        config::save(&cfg)?;
                    }
                    "imap_port" => {
                        let port: u16 = value
                            .parse()
                            .map_err(|_| anyhow::anyhow!("Invalid imap_port '{}'", value))?;
                        let mut cfg = ctx.config.clone();
                        cfg.mail.entry(name.to_string()).or_default().imap_port = Some(port);
                        config::save(&cfg)?;
                    }
                    "password" => {
                        secrets::set_secret(&format!("mail:{name}"), "password", &value)?;
                        // Ensure the profile is registered in the config file (with no
                        // host/port/user yet) so it shows up in `config show`.
                        let mut cfg = ctx.config.clone();
                        cfg.mail.entry(name.to_string()).or_default();
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
                        return Ok(());
                    }
                    _ => bail!(
                        "Unknown mail profile key '{}'. Available: {}",
                        field,
                        MAIL_KEYS
                    ),
                }
                if json {
                    println!("{}", serde_json::json!({"key": key, "value": value}));
                } else {
                    println!("{} {} = {}", "set".green().bold(), key.cyan(), value);
                }
                return Ok(());
            }
            if let Some((name, field)) = parse_profile_key(&key) {
                match field {
                    "base_url" => {
                        let mut cfg = ctx.config.clone();
                        cfg.profile.entry(name.to_string()).or_default().base_url =
                            Some(value.clone());
                        config::save(&cfg)?;
                        if json {
                            println!("{}", serde_json::json!({"key": key, "value": value}));
                        } else {
                            println!("{} {} = {}", "set".green().bold(), key.cyan(), value);
                        }
                    }
                    "token_url" => {
                        let mut cfg = ctx.config.clone();
                        cfg.profile.entry(name.to_string()).or_default().token_url =
                            Some(value.clone());
                        config::save(&cfg)?;
                        if json {
                            println!("{}", serde_json::json!({"key": key, "value": value}));
                        } else {
                            println!("{} {} = {}", "set".green().bold(), key.cyan(), value);
                        }
                    }
                    "client_id" => {
                        let mut cfg = ctx.config.clone();
                        cfg.profile.entry(name.to_string()).or_default().client_id =
                            Some(value.clone());
                        config::save(&cfg)?;
                        if json {
                            println!("{}", serde_json::json!({"key": key, "value": value}));
                        } else {
                            println!("{} {} = {}", "set".green().bold(), key.cyan(), value);
                        }
                    }
                    "token" | "client_secret" | "refresh_token" => {
                        secrets::set_secret(name, field, &value)?;
                        // Ensure the profile is registered in the config file (with no
                        // base_url) so it shows up in `config profiles` / `show`.
                        let mut cfg = ctx.config.clone();
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
                        "Unknown profile key '{}'. Available: base_url, token, token_url, client_id, client_secret, refresh_token",
                        field
                    ),
                }
                return Ok(());
            }
            let mut cfg = ctx.config.clone();
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
                    "Unknown key '{}'. Available: default.output, default.color, profile.<name>.base_url, profile.<name>.token, profile.<name>.token_url, profile.<name>.client_id, profile.<name>.client_secret, profile.<name>.refresh_token",
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
            if let Some((name, field)) = parse_mail_key(&key) {
                match field {
                    "host" => {
                        let mut cfg = ctx.config.clone();
                        if let Some(m) = cfg.mail.get_mut(name) {
                            m.host.clear();
                        }
                        config::save(&cfg)?;
                    }
                    "port" => {
                        let mut cfg = ctx.config.clone();
                        if let Some(m) = cfg.mail.get_mut(name) {
                            m.port = 0;
                        }
                        config::save(&cfg)?;
                    }
                    "user" => {
                        let mut cfg = ctx.config.clone();
                        if let Some(m) = cfg.mail.get_mut(name) {
                            m.user.clear();
                        }
                        config::save(&cfg)?;
                    }
                    "from" => {
                        let mut cfg = ctx.config.clone();
                        if let Some(m) = cfg.mail.get_mut(name) {
                            m.from = None;
                        }
                        config::save(&cfg)?;
                    }
                    "tls" => {
                        let mut cfg = ctx.config.clone();
                        if let Some(m) = cfg.mail.get_mut(name) {
                            m.tls = None;
                        }
                        config::save(&cfg)?;
                    }
                    "imap_host" => {
                        let mut cfg = ctx.config.clone();
                        if let Some(m) = cfg.mail.get_mut(name) {
                            m.imap_host = None;
                        }
                        config::save(&cfg)?;
                    }
                    "imap_port" => {
                        let mut cfg = ctx.config.clone();
                        if let Some(m) = cfg.mail.get_mut(name) {
                            m.imap_port = None;
                        }
                        config::save(&cfg)?;
                    }
                    "password" => secrets::delete_secret(&format!("mail:{name}"), "password")?,
                    _ => bail!(
                        "Unknown mail profile key '{}'. Available: {}",
                        field,
                        MAIL_KEYS
                    ),
                }
                if json {
                    println!("{}", serde_json::json!({"key": key, "unset": true}));
                } else {
                    println!("{} {}", "unset".red().bold(), key.cyan());
                }
                return Ok(());
            }
            if let Some((name, field)) = parse_profile_key(&key) {
                match field {
                    "base_url" => {
                        let mut cfg = ctx.config.clone();
                        if let Some(p) = cfg.profile.get_mut(name) {
                            p.base_url = None;
                        }
                        config::save(&cfg)?;
                    }
                    "token_url" => {
                        let mut cfg = ctx.config.clone();
                        if let Some(p) = cfg.profile.get_mut(name) {
                            p.token_url = None;
                        }
                        config::save(&cfg)?;
                    }
                    "client_id" => {
                        let mut cfg = ctx.config.clone();
                        if let Some(p) = cfg.profile.get_mut(name) {
                            p.client_id = None;
                        }
                        config::save(&cfg)?;
                    }
                    "token" | "client_secret" | "refresh_token" => {
                        secrets::delete_secret(name, field)?
                    }
                    _ => bail!(
                        "Unknown profile key '{}'. Available: base_url, token, token_url, client_id, client_secret, refresh_token",
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
                "Unknown key '{}'. Available: profile.<name>.base_url, profile.<name>.token, profile.<name>.token_url, profile.<name>.client_id, profile.<name>.client_secret, profile.<name>.refresh_token",
                key
            );
        }
    }
    Ok(())
}
