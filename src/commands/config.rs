use crate::{config, context::Context};
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
    match args.subcommand {
        ConfigSubcommand::Show => {
            println!("{}", toml::to_string_pretty(&ctx.config)?);
        }
        ConfigSubcommand::Path => {
            println!("{}", config::config_path().display());
        }
        ConfigSubcommand::Profiles => {
            if ctx.config.profile.is_empty() {
                println!("{}", "No profiles configured.".dimmed());
            } else {
                for name in ctx.config.profile.keys() {
                    let marker = if *name == ctx.profile {
                        " (active)".dimmed().to_string()
                    } else {
                        String::new()
                    };
                    println!("  {}{}", name.cyan(), marker);
                }
            }
        }
        ConfigSubcommand::Get { key } => match key.as_str() {
            "default.output" => println!("{}", ctx.config.default.output),
            "default.color" => println!("{}", ctx.config.default.color),
            _ => bail!(
                "Unknown key '{}'. Available: default.output, default.color",
                key
            ),
        },
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
            println!("{} {} = {}", "set".green().bold(), key.cyan(), value);
        }
    }
    Ok(())
}
