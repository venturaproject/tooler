use crate::{config, context::Context, output::OutputFormat};
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
    /// Get a value by key (e.g. default.output)
    Get { key: String },
    /// Set a value by key (e.g. default.output json)
    Set { key: String, value: String },
    /// List configured profiles
    Profiles,
    /// Print the config file path
    Path,
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
                println!(
                    "{}",
                    serde_json::json!({"profiles": names, "active": ctx.profile})
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
                    println!("  {}{}", name.cyan(), marker);
                }
            }
        }
        ConfigSubcommand::Get { key } => {
            let value = match key.as_str() {
                "default.output" => ctx.config.default.output.clone(),
                "default.color" => ctx.config.default.color.to_string(),
                _ => bail!(
                    "Unknown key '{}'. Available: default.output, default.color",
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
                    "Unknown key '{}'. Available: default.output, default.color",
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
    }
    Ok(())
}
