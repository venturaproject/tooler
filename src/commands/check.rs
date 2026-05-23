use crate::context::Context;
use anyhow::{Context as _, Result};
use clap::{Args, Subcommand};
use colored::Colorize;
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

#[derive(Args)]
pub struct CheckArgs {
    #[command(subcommand)]
    pub subcommand: CheckSubcommand,
}

#[derive(Subcommand)]
pub enum CheckSubcommand {
    /// Check if a URL returns a 2xx response
    Url {
        url: String,
        /// Timeout in seconds
        #[arg(short, long, default_value_t = 5)]
        timeout: u64,
    },
    /// Check if a TCP port is open
    Port {
        host: String,
        port: u16,
        /// Timeout in seconds
        #[arg(short, long, default_value_t = 3)]
        timeout: u64,
    },
}

pub fn run(args: CheckArgs, _ctx: &Context) -> Result<()> {
    match args.subcommand {
        CheckSubcommand::Url { url, timeout } => check_url(&url, timeout),
        CheckSubcommand::Port {
            host,
            port,
            timeout,
        } => check_port(&host, port, timeout),
    }
}

fn check_url(url: &str, timeout_secs: u64) -> Result<()> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .build()?;

    match client.get(url).send() {
        Ok(res) if res.status().is_success() => {
            println!(
                "{} {} ({})",
                "✓".green().bold(),
                url,
                res.status().as_u16().to_string().green()
            );
            Ok(())
        }
        Ok(res) => {
            println!(
                "{} {} ({})",
                "✗".red().bold(),
                url,
                res.status().as_u16().to_string().red()
            );
            anyhow::bail!("HTTP {}", res.status().as_u16())
        }
        Err(e) => {
            println!("{} {} — {}", "✗".red().bold(), url, e.to_string().dimmed());
            Err(e.into())
        }
    }
}

fn check_port(host: &str, port: u16, timeout_secs: u64) -> Result<()> {
    let addr = format!("{host}:{port}");
    let socket_addr = addr
        .to_socket_addrs()
        .with_context(|| format!("Cannot resolve '{addr}'"))?
        .next()
        .with_context(|| format!("No address found for '{addr}'"))?;

    match TcpStream::connect_timeout(&socket_addr, Duration::from_secs(timeout_secs)) {
        Ok(_) => {
            println!("{} {host}:{port} is open", "✓".green().bold());
            Ok(())
        }
        Err(e) => {
            println!(
                "{} {host}:{port} — {}",
                "✗".red().bold(),
                e.to_string().dimmed()
            );
            Err(e.into())
        }
    }
}
