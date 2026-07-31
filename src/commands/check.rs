use crate::{context::Context, output::OutputFormat};
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

pub fn run(args: CheckArgs, ctx: &Context) -> Result<()> {
    match args.subcommand {
        CheckSubcommand::Url { url, timeout } => check_url(&url, timeout, ctx),
        CheckSubcommand::Port {
            host,
            port,
            timeout,
        } => check_port(&host, port, timeout, ctx),
    }
}

/// Sends a GET request and returns the raw HTTP status code. A non-2xx response
/// is still `Ok` (it's a valid response the caller may want to inspect) -- only
/// an `Err` means the request itself failed (DNS, connection, timeout). Shared by
/// `check_url` and `deploy`'s post-restart health check, so both use the same
/// client setup/timeout handling.
pub(crate) fn probe_url(url: &str, timeout_secs: u64) -> Result<u16> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .build()?;
    let res = client.get(url).send()?;
    Ok(res.status().as_u16())
}

fn check_url(url: &str, timeout_secs: u64, ctx: &Context) -> Result<()> {
    match probe_url(url, timeout_secs) {
        Ok(status) if (200..300).contains(&status) => {
            if ctx.output == OutputFormat::Json {
                println!(
                    "{}",
                    serde_json::json!({"url": url, "success": true, "status": status})
                );
                return Ok(());
            }
            println!(
                "{} {} ({})",
                "✓".green().bold(),
                url,
                status.to_string().green()
            );
            Ok(())
        }
        Ok(status) => {
            if ctx.output == OutputFormat::Json {
                println!(
                    "{}",
                    serde_json::json!({"url": url, "success": false, "status": status})
                );
                std::process::exit(1);
            }
            println!(
                "{} {} ({})",
                "✗".red().bold(),
                url,
                status.to_string().red()
            );
            anyhow::bail!("HTTP {}", status)
        }
        Err(e) => {
            if ctx.output == OutputFormat::Json {
                println!(
                    "{}",
                    serde_json::json!({"url": url, "success": false, "error": e.to_string()})
                );
                std::process::exit(1);
            }
            println!("{} {} — {}", "✗".red().bold(), url, e.to_string().dimmed());
            Err(e)
        }
    }
}

fn check_port(host: &str, port: u16, timeout_secs: u64, ctx: &Context) -> Result<()> {
    let addr = format!("{host}:{port}");
    let socket_addr = addr
        .to_socket_addrs()
        .with_context(|| format!("Cannot resolve '{addr}'"))?
        .next()
        .with_context(|| format!("No address found for '{addr}'"))?;

    match TcpStream::connect_timeout(&socket_addr, Duration::from_secs(timeout_secs)) {
        Ok(_) => {
            if ctx.output == OutputFormat::Json {
                println!(
                    "{}",
                    serde_json::json!({"host": host, "port": port, "success": true})
                );
                return Ok(());
            }
            println!("{} {host}:{port} is open", "✓".green().bold());
            Ok(())
        }
        Err(e) => {
            if ctx.output == OutputFormat::Json {
                println!(
                    "{}",
                    serde_json::json!({"host": host, "port": port, "success": false, "error": e.to_string()})
                );
                std::process::exit(1);
            }
            println!(
                "{} {host}:{port} — {}",
                "✗".red().bold(),
                e.to_string().dimmed()
            );
            Err(e.into())
        }
    }
}
