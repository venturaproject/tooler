use crate::{context::Context, output::OutputFormat};
use anyhow::Result;
use clap::Args;
use colored::Colorize;
use std::env;

#[derive(Args)]
pub struct InfoArgs {
    /// Show environment variables
    #[arg(short, long)]
    pub env: bool,

    /// Show working directory
    #[arg(short, long)]
    pub dir: bool,
}

pub fn run(args: InfoArgs, ctx: &Context) -> Result<()> {
    let show_all = !args.env && !args.dir;
    let env_keys = ["HOME", "USER", "SHELL", "TERM"];

    if ctx.output == OutputFormat::Json {
        let mut root = serde_json::Map::new();
        if args.dir || show_all {
            root.insert(
                "dir".into(),
                env::current_dir()?.display().to_string().into(),
            );
        }
        if args.env || show_all {
            let env_map: serde_json::Map<_, _> = env_keys
                .iter()
                .filter_map(|k| {
                    env::var(k)
                        .ok()
                        .map(|v| (k.to_string(), serde_json::Value::String(v)))
                })
                .collect();
            root.insert("env".into(), serde_json::Value::Object(env_map));
        }
        println!("{}", serde_json::to_string_pretty(&root)?);
        return Ok(());
    }

    let mut pairs: Vec<(String, String)> = vec![];

    if args.dir || show_all {
        pairs.push(("dir".into(), env::current_dir()?.display().to_string()));
    }
    if args.env || show_all {
        for key in env_keys {
            if let Ok(val) = env::var(key) {
                pairs.push((key.into(), val));
            }
        }
    }

    let kv_refs: Vec<(&str, &str)> = pairs
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    if ctx.output == OutputFormat::Plain {
        println!("{}", "tooler info".bold().cyan());
        println!("{}", "─".repeat(30).dimmed());
    }

    ctx.output.print_kv(&kv_refs);
    Ok(())
}
