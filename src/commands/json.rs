use crate::context::Context;
use anyhow::{Context as _, Result, bail};
use clap::Args;
use colored::Colorize;
use serde_json::Value;
use std::io::{self, Read};

#[derive(Args)]
pub struct JsonArgs {
    /// JSON file path (omit to read from stdin)
    pub file: Option<String>,

    /// Extract a field by dot-notation key (e.g. "user.name")
    #[arg(short, long)]
    pub key: Option<String>,

    /// Compact output instead of pretty-print
    #[arg(short, long)]
    pub compact: bool,
}

pub fn run(args: JsonArgs, _ctx: &Context) -> Result<()> {
    let raw = match &args.file {
        Some(path) => {
            std::fs::read_to_string(path).with_context(|| format!("Cannot read file: {path}"))?
        }
        None => {
            let mut buf = String::new();
            io::stdin().read_to_string(&mut buf)?;
            buf
        }
    };

    let mut value: Value = serde_json::from_str(&raw).context("Invalid JSON")?;

    if let Some(key) = &args.key {
        for part in key.split('.') {
            value = match value.get(part) {
                Some(v) => v.clone(),
                None => bail!("Key '{}' not found", key),
            };
        }
    }

    let output = if args.compact {
        serde_json::to_string(&value)?
    } else {
        serde_json::to_string_pretty(&value)?
    };

    println!("{}", colorize_json(&output));
    Ok(())
}

fn colorize_json(json: &str) -> String {
    json.lines()
        .map(|line| {
            if line.trim_start().starts_with('"')
                && line.contains(':')
                && let Some(pos) = line.find(':')
            {
                let (key, rest) = line.split_at(pos);
                return format!("{}{}", key.cyan(), rest);
            }
            if matches!(line.trim(), "{" | "}" | "[" | "]" | "{}," | "}," | "],") {
                return line.dimmed().to_string();
            }
            line.to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}
