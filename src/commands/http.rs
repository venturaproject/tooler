use crate::context::Context;
use anyhow::{Context as _, Result, bail};
use clap::{Args, Subcommand};
use colored::Colorize;
use std::time::Duration;

#[derive(Args)]
pub struct HttpArgs {
    #[command(subcommand)]
    pub subcommand: HttpSubcommand,
}

#[derive(Subcommand)]
pub enum HttpSubcommand {
    /// Perform a GET request
    Get {
        /// URL or path (path uses profile base_url)
        url: String,
        /// Bearer token for Authorization header
        #[arg(short, long)]
        token: Option<String>,
        /// Extra headers in "Key: Value" format
        #[arg(short = 'H', long = "header")]
        headers: Vec<String>,
        /// Timeout in seconds
        #[arg(long, default_value_t = 10)]
        timeout: u64,
    },
    /// Perform a POST request with a JSON body
    Post {
        url: String,
        /// JSON body string
        #[arg(short, long)]
        body: Option<String>,
        #[arg(short, long)]
        token: Option<String>,
        #[arg(short = 'H', long = "header")]
        headers: Vec<String>,
        #[arg(long, default_value_t = 10)]
        timeout: u64,
    },
}

fn resolve_url(url: &str, ctx: &Context) -> Result<String> {
    if url.starts_with("http://") || url.starts_with("https://") {
        return Ok(url.to_string());
    }
    if let Some(profile) = ctx.config.profile.get(&ctx.profile)
        && let Some(base) = &profile.base_url
    {
        return Ok(format!(
            "{}/{}",
            base.trim_end_matches('/'),
            url.trim_start_matches('/')
        ));
    }
    bail!(
        "'{}' is not an absolute URL and profile '{}' has no base_url.\n  Set it with: tooler config set profile.{}.base_url <url>",
        url,
        ctx.profile,
        ctx.profile
    )
}

fn active_token(token: Option<String>, ctx: &Context) -> Option<String> {
    token.or_else(|| ctx.config.profile.get(&ctx.profile)?.token.clone())
}

pub fn run(args: HttpArgs, ctx: &Context) -> Result<()> {
    match args.subcommand {
        HttpSubcommand::Get {
            url,
            token,
            headers,
            timeout,
        } => {
            let url = resolve_url(&url, ctx)?;
            do_request(
                "GET",
                &url,
                None,
                active_token(token, ctx),
                headers,
                timeout,
                ctx,
            )
        }
        HttpSubcommand::Post {
            url,
            body,
            token,
            headers,
            timeout,
        } => {
            let url = resolve_url(&url, ctx)?;
            do_request(
                "POST",
                &url,
                body.as_deref(),
                active_token(token, ctx),
                headers,
                timeout,
                ctx,
            )
        }
    }
}

fn do_request(
    method: &str,
    url: &str,
    body: Option<&str>,
    token: Option<String>,
    extra_headers: Vec<String>,
    timeout_secs: u64,
    _ctx: &Context,
) -> Result<()> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .build()?;

    let mut req = match method {
        "POST" => client.post(url),
        _ => client.get(url),
    };

    if let Some(t) = token {
        req = req.header("Authorization", format!("Bearer {t}"));
    }

    for h in &extra_headers {
        if let Some((key, val)) = h.split_once(':') {
            req = req.header(key.trim(), val.trim());
        }
    }

    if let Some(b) = body {
        req = req
            .header("Content-Type", "application/json")
            .body(b.to_string());
    }

    let response = req
        .send()
        .with_context(|| format!("Request failed: {url}"))?;

    let status = response.status();
    let status_label = status.as_u16().to_string();
    let status_colored = if status.is_success() {
        status_label.green()
    } else if status.is_client_error() {
        status_label.yellow()
    } else {
        status_label.red()
    };

    println!(
        "{} {} — {}",
        method.bold().cyan(),
        url.dimmed(),
        status_colored
    );
    println!("{}", "─".repeat(50).dimmed());

    let body_text = response.text()?;
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&body_text) {
        println!("{}", serde_json::to_string_pretty(&json)?);
    } else {
        println!("{body_text}");
    }

    if !status.is_success() {
        bail!("HTTP {}", status.as_u16());
    }

    Ok(())
}
