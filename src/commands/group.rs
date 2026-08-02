use crate::{
    config::{self, Group},
    context::Context,
    output::OutputFormat,
};
use anyhow::{Result, bail};
use clap::{Args, Subcommand};
use colored::Colorize;

#[derive(Args)]
pub struct GroupArgs {
    #[command(subcommand)]
    pub subcommand: GroupSubcommand,
}

#[derive(Subcommand)]
pub enum GroupSubcommand {
    /// List configured server groups
    List,

    /// Add or update a server group
    Add {
        /// Group name (e.g. web, db, staging)
        name: String,
        /// Comma-separated server profile names (must already exist, see: tooler server list)
        #[arg(long, value_delimiter = ',')]
        members: Vec<String>,
    },

    /// Show details of a server group
    Show { name: String },

    /// Remove a server group
    Remove { name: String },
}

pub fn run(args: GroupArgs, ctx: &Context) -> Result<()> {
    match args.subcommand {
        GroupSubcommand::List => list(ctx),
        GroupSubcommand::Add { name, members } => add(&name, members, ctx),
        GroupSubcommand::Show { name } => show(&name, ctx),
        GroupSubcommand::Remove { name } => remove(&name, ctx),
    }
}

fn list(ctx: &Context) -> Result<()> {
    let cfg = &ctx.config;
    let mut names: Vec<&String> = cfg.group.keys().collect();
    names.sort();

    if ctx.output == OutputFormat::Json {
        let groups: serde_json::Map<String, serde_json::Value> = names
            .iter()
            .map(|n| (n.to_string(), serde_json::to_value(&cfg.group[*n]).unwrap()))
            .collect();
        println!(
            "{}",
            serde_json::json!({"groups": serde_json::Value::Object(groups)})
        );
        return Ok(());
    }

    if cfg.group.is_empty() {
        println!("{}", "No groups configured.".dimmed());
        println!(
            "{}",
            "Add one with: tooler group add <name> --members server1,server2".dimmed()
        );
        return Ok(());
    }
    println!("{}", "groups:".bold().cyan());
    println!("{}", "─".repeat(50).dimmed());
    for name in names {
        let g = &cfg.group[name];
        println!(
            "  {} {}",
            name.bold(),
            format!("({} members)", g.members.len()).dimmed()
        );
    }
    Ok(())
}

fn add(name: &str, members: Vec<String>, ctx: &Context) -> Result<()> {
    for m in &members {
        if !ctx.config.server.contains_key(m) {
            bail!(
                "Server '{}' not found. Add it first with: tooler server add {} --host <ip>",
                m,
                m
            );
        }
    }

    let mut cfg = ctx.config.clone();
    let group = Group {
        members: members.clone(),
    };
    cfg.group.insert(name.to_string(), group.clone());
    config::save(&cfg)?;

    if ctx.output == OutputFormat::Json {
        println!("{}", serde_json::json!({"name": name, "group": group}));
        return Ok(());
    }
    println!("{} group '{}'", "added".green().bold(), name.cyan());
    Ok(())
}

fn show(name: &str, ctx: &Context) -> Result<()> {
    let cfg = &ctx.config;
    let g = cfg
        .group
        .get(name)
        .ok_or_else(|| anyhow::anyhow!("Group '{}' not found", name))?;

    if ctx.output == OutputFormat::Json {
        println!("{}", serde_json::json!({"name": name, "group": g}));
        return Ok(());
    }

    println!("{} {}", "group:".bold().cyan(), name.bold());
    println!("{}", "─".repeat(40).dimmed());
    if g.members.is_empty() {
        println!("  {}", "(no members)".dimmed());
    }
    for m in &g.members {
        let host = cfg
            .server
            .get(m)
            .map(|s| s.host.clone())
            .unwrap_or_else(|| "(not found)".to_string());
        println!("  {} {}", m.bold(), host.dimmed());
    }
    Ok(())
}

fn remove(name: &str, ctx: &Context) -> Result<()> {
    let mut cfg = ctx.config.clone();
    if cfg.group.remove(name).is_none() {
        bail!("Group '{}' not found", name);
    }
    config::save(&cfg)?;

    if ctx.output == OutputFormat::Json {
        println!("{}", serde_json::json!({"name": name, "removed": true}));
        return Ok(());
    }
    println!("{} group '{}'", "removed".red().bold(), name);
    Ok(())
}
