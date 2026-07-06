use crate::{
    config::{self, Server},
    context::Context,
    output::OutputFormat,
};
use anyhow::{Result, bail};
use clap::{Args, Subcommand};
use colored::Colorize;

#[derive(Args)]
pub struct ServerArgs {
    #[command(subcommand)]
    pub subcommand: ServerSubcommand,
}

#[derive(Subcommand)]
pub enum ServerSubcommand {
    /// List configured servers
    List,

    /// Add or update a server profile
    Add {
        /// Profile name (e.g. gdn, staging, prod)
        name: String,
        /// Hostname or IP address
        #[arg(long)]
        host: String,
        /// SSH user
        #[arg(long)]
        user: Option<String>,
        /// SSH port [default: 22]
        #[arg(long)]
        port: Option<u16>,
        /// Path to private key (e.g. ~/.ssh/trd-gdn)
        #[arg(long)]
        key: Option<String>,
        /// Remote SSL certificate directory [default: /etc/nginx/ssl]
        #[arg(long)]
        ssl_dir: Option<String>,
    },

    /// Show details of a server profile
    Show { name: String },

    /// Remove a server profile
    Remove { name: String },
}

pub fn run(args: ServerArgs, ctx: &Context) -> Result<()> {
    match args.subcommand {
        ServerSubcommand::List => list(ctx),
        ServerSubcommand::Add {
            name,
            host,
            user,
            port,
            key,
            ssl_dir,
        } => add(&name, host, user, port, key, ssl_dir),
        ServerSubcommand::Show { name } => show(&name, ctx),
        ServerSubcommand::Remove { name } => remove(&name),
    }
}

fn list(ctx: &Context) -> Result<()> {
    let cfg = config::load()?;
    let mut names: Vec<&String> = cfg.server.keys().collect();
    names.sort();

    if ctx.output == OutputFormat::Json {
        let servers: serde_json::Map<String, serde_json::Value> = names
            .iter()
            .map(|n| {
                (
                    n.to_string(),
                    serde_json::to_value(&cfg.server[*n]).unwrap(),
                )
            })
            .collect();
        println!(
            "{}",
            serde_json::json!({"servers": serde_json::Value::Object(servers)})
        );
        return Ok(());
    }

    if cfg.server.is_empty() {
        println!("{}", "No servers configured.".dimmed());
        println!(
            "{}",
            "Add one with: tooler server add <name> --host <ip> --user <user> --key ~/.ssh/key"
                .dimmed()
        );
        return Ok(());
    }
    println!("{}", "servers:".bold().cyan());
    println!("{}", "─".repeat(50).dimmed());
    for name in names {
        let s = &cfg.server[name];
        let user_host = match &s.user {
            Some(u) => format!("{u}@{}", s.host),
            None => s.host.clone(),
        };
        let port = s.port.map(|p| format!(":{p}")).unwrap_or_default();
        let key = s
            .key
            .as_deref()
            .map(|k| format!("  key: {k}"))
            .unwrap_or_default();
        println!(
            "  {} {}{}{}",
            name.bold(),
            user_host.green(),
            port.dimmed(),
            key.dimmed()
        );
    }
    Ok(())
}

fn add(
    name: &str,
    host: String,
    user: Option<String>,
    port: Option<u16>,
    key: Option<String>,
    ssl_dir: Option<String>,
) -> Result<()> {
    let mut cfg = config::load()?;
    cfg.server.insert(
        name.to_string(),
        Server {
            host,
            user,
            port,
            key,
            ssl_dir,
        },
    );
    config::save(&cfg)?;
    println!("{} server '{}'", "added".green().bold(), name.cyan());
    Ok(())
}

fn show(name: &str, ctx: &Context) -> Result<()> {
    let cfg = config::load()?;
    let s = cfg
        .server
        .get(name)
        .ok_or_else(|| anyhow::anyhow!("Server '{}' not found", name))?;

    if ctx.output == OutputFormat::Json {
        println!("{}", serde_json::json!({"name": name, "server": s}));
        return Ok(());
    }

    println!("{} {}", "server:".bold().cyan(), name.bold());
    println!("{}", "─".repeat(40).dimmed());
    println!("{} {}", "host:   ".bold(), s.host);
    println!(
        "{} {}",
        "user:   ".bold(),
        s.user.as_deref().unwrap_or("(system default)").dimmed()
    );
    println!(
        "{} {}",
        "port:   ".bold(),
        s.port
            .map(|p| p.to_string())
            .unwrap_or_else(|| "22".into())
            .dimmed()
    );
    println!(
        "{} {}",
        "key:    ".bold(),
        s.key.as_deref().unwrap_or("(ssh-agent / default)").dimmed()
    );
    println!(
        "{} {}",
        "ssl_dir:".bold(),
        s.ssl_dir.as_deref().unwrap_or("/etc/nginx/ssl").dimmed()
    );
    Ok(())
}

fn remove(name: &str) -> Result<()> {
    let mut cfg = config::load()?;
    if cfg.server.remove(name).is_none() {
        bail!("Server '{}' not found", name);
    }
    config::save(&cfg)?;
    println!("{} server '{}'", "removed".red().bold(), name);
    Ok(())
}
