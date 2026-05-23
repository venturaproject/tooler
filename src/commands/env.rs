use crate::{context::Context, output::OutputFormat};
use anyhow::{Context as _, Result, bail};
use clap::{Args, Subcommand};
use colored::Colorize;
use std::collections::HashMap;

#[derive(Args)]
pub struct EnvArgs {
    #[command(subcommand)]
    pub subcommand: EnvSubcommand,
}

#[derive(Subcommand)]
pub enum EnvSubcommand {
    /// Show variables from a .env file (values masked by default)
    Show {
        #[arg(default_value = ".env")]
        file: String,
        /// Show real values instead of masking them
        #[arg(long)]
        reveal: bool,
    },
    /// List only the variable names
    List {
        #[arg(default_value = ".env")]
        file: String,
    },
    /// Get a specific variable value
    Get {
        key: String,
        #[arg(default_value = ".env")]
        file: String,
    },
    /// Show keys present in one file but missing in the other
    Diff { file_a: String, file_b: String },
    /// Verify all keys from a reference file exist in the target
    Check {
        /// Reference file (e.g. .env.example)
        reference: String,
        /// File to check [default: .env]
        #[arg(default_value = ".env")]
        target: String,
    },
}

fn parse_env_file(path: &str) -> Result<HashMap<String, String>> {
    let content =
        std::fs::read_to_string(path).with_context(|| format!("Cannot read file: {path}"))?;

    let mut map = HashMap::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let value = value.trim().trim_matches('"').trim_matches('\'');
            map.insert(key.trim().to_string(), value.to_string());
        }
    }
    Ok(map)
}

fn mask(value: &str) -> String {
    if value.is_empty() {
        return "(empty)".dimmed().to_string();
    }
    let visible = value.len().min(3);
    format!(
        "{}{}",
        &value[..visible],
        "*".repeat(value.len().saturating_sub(visible))
    )
}

pub fn run(args: EnvArgs, ctx: &Context) -> Result<()> {
    match args.subcommand {
        EnvSubcommand::Show { file, reveal } => {
            let vars = parse_env_file(&file)?;
            let mut pairs: Vec<(String, String)> = vars
                .into_iter()
                .map(|(k, v)| (k, if reveal { v } else { mask(&v) }))
                .collect();
            pairs.sort_by(|a, b| a.0.cmp(&b.0));

            let kv_refs: Vec<(&str, &str)> = pairs
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();

            if ctx.output == OutputFormat::Plain {
                println!("{} {}", "env:".bold(), file.cyan());
                println!("{}", "─".repeat(40).dimmed());
            }
            ctx.output.print_kv(&kv_refs);
        }

        EnvSubcommand::List { file } => {
            let vars = parse_env_file(&file)?;
            let mut keys: Vec<String> = vars.into_keys().collect();
            keys.sort();
            for key in &keys {
                println!("{key}");
            }
        }

        EnvSubcommand::Get { key, file } => {
            let vars = parse_env_file(&file)?;
            match vars.get(&key) {
                Some(val) => println!("{val}"),
                None => bail!("Key '{}' not found in {}", key, file),
            }
        }

        EnvSubcommand::Diff { file_a, file_b } => {
            let vars_a = parse_env_file(&file_a)?;
            let vars_b = parse_env_file(&file_b)?;

            let mut all_keys: Vec<String> = vars_a.keys().chain(vars_b.keys()).cloned().collect();
            all_keys.sort();
            all_keys.dedup();

            let mut diffs = false;
            for key in &all_keys {
                match (vars_a.contains_key(key), vars_b.contains_key(key)) {
                    (true, false) => {
                        println!(
                            "{} {} {}",
                            "−".red().bold(),
                            key,
                            format!("(only in {file_a})").dimmed()
                        );
                        diffs = true;
                    }
                    (false, true) => {
                        println!(
                            "{} {} {}",
                            "+".green().bold(),
                            key,
                            format!("(only in {file_b})").dimmed()
                        );
                        diffs = true;
                    }
                    _ => {}
                }
            }
            if !diffs {
                println!("{}", "No differences in keys.".green());
            }
        }

        EnvSubcommand::Check { reference, target } => {
            let ref_vars = parse_env_file(&reference)?;
            let target_vars = parse_env_file(&target)?;

            let mut missing: Vec<String> = ref_vars
                .into_keys()
                .filter(|k| !target_vars.contains_key(k))
                .collect();

            if missing.is_empty() {
                println!(
                    "{} all keys from {} present in {}",
                    "✓".green().bold(),
                    reference,
                    target
                );
            } else {
                missing.sort();
                eprintln!("{} missing keys in {}:", "✗".red().bold(), target);
                for key in &missing {
                    eprintln!("  {}", key.yellow());
                }
                bail!("{} missing key(s)", missing.len());
            }
        }
    }
    Ok(())
}
