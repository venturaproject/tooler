use crate::{context::Context, output::OutputFormat, project};
use anyhow::{Result, bail};
use clap::Args;
use colored::Colorize;

#[derive(Args)]
pub struct RunArgs {
    /// Script name to run (omit to list available scripts)
    pub script: Option<String>,

    /// Show the command without executing it
    #[arg(long)]
    pub dry: bool,

    /// Extra arguments appended to the script command
    #[arg(last = true)]
    pub extra: Vec<String>,
}

pub fn run(args: RunArgs, ctx: &Context) -> Result<()> {
    let (project, root) = project::load()?;

    match args.script {
        None => list_scripts(&project, ctx),
        Some(name) => exec_script(&name, &project, &root, args.dry, &args.extra, ctx),
    }
}

fn list_scripts(project: &project::ProjectConfig, ctx: &Context) -> Result<()> {
    let mut names: Vec<&String> = project.scripts.keys().collect();
    names.sort();

    if ctx.output == OutputFormat::Json {
        let scripts: serde_json::Map<String, serde_json::Value> = names
            .iter()
            .map(|n| (n.to_string(), project.scripts[*n].clone().into()))
            .collect();
        println!(
            "{}",
            serde_json::json!({"scripts": serde_json::Value::Object(scripts)})
        );
        return Ok(());
    }

    if project.scripts.is_empty() {
        println!("{}", "No scripts defined.".dimmed());
        println!(
            "Create a {} file with a {} section:",
            ".tooler.toml".cyan(),
            "[scripts]".bold()
        );
        println!();
        println!("  {}", "[scripts]".dimmed());
        println!(
            "  {} = {}",
            "build".dimmed(),
            r#""cargo build --release""#.dimmed()
        );
        println!("  {} = {}", "test".dimmed(), r#""cargo test""#.dimmed());
        return Ok(());
    }

    let config_path = project::find_config()
        .map(|p| p.display().to_string())
        .unwrap_or_default();

    println!("{} {}", "scripts:".bold().cyan(), config_path.dimmed());
    println!("{}", "─".repeat(40).dimmed());

    for name in names {
        println!("  {:20} {}", name.bold(), project.scripts[name].dimmed());
    }
    Ok(())
}

fn exec_script(
    name: &str,
    project: &project::ProjectConfig,
    root: &std::path::Path,
    dry: bool,
    extra: &[String],
    ctx: &Context,
) -> Result<()> {
    let json = ctx.output == OutputFormat::Json;

    let cmd = match project.scripts.get(name) {
        Some(c) => c.clone(),
        None => {
            let mut available: Vec<&str> = project.scripts.keys().map(String::as_str).collect();
            available.sort();
            let message = format!(
                "Script '{}' not found. Available: {}",
                name,
                available.join(", ")
            );
            if json {
                println!("{}", serde_json::json!({"script": name, "error": message}));
                std::process::exit(1);
            }
            bail!(message)
        }
    };

    let full_cmd = if extra.is_empty() {
        cmd.clone()
    } else {
        format!("{} {}", cmd, extra.join(" "))
    };

    if !json {
        println!("{} {}", "$".bold().green(), full_cmd.dimmed());
    }

    if dry {
        if json {
            println!(
                "{}",
                serde_json::json!({"script": name, "command": full_cmd, "dry": true})
            );
        }
        return Ok(());
    }

    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(&full_cmd)
        .current_dir(root)
        .status()?;

    if json {
        println!(
            "{}",
            serde_json::json!({
                "script": name,
                "command": full_cmd,
                "success": status.success(),
                "exit_code": status.code(),
            })
        );
        if !status.success() {
            std::process::exit(1);
        }
        return Ok(());
    }

    if !status.success() {
        bail!(
            "script '{}' failed (exit {})",
            name,
            status.code().unwrap_or(1)
        );
    }
    Ok(())
}
