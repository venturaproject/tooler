use crate::{context::Context, output::OutputFormat};
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
        /// Bearer token for Authorization header [env: TOOLER_HTTP_TOKEN]
        #[arg(short, long, env = "TOOLER_HTTP_TOKEN")]
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
        /// Bearer token for Authorization header [env: TOOLER_HTTP_TOKEN]
        #[arg(short, long, env = "TOOLER_HTTP_TOKEN")]
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

/// True if `a` and `b` share scheme+host+port. Used to gate auto-attaching a stored
/// profile token: it must only go to the host the profile was configured for, never to
/// an arbitrary URL (e.g. one supplied by an MCP tool call).
fn same_origin(a: &str, b: &str) -> bool {
    match (reqwest::Url::parse(a), reqwest::Url::parse(b)) {
        (Ok(ua), Ok(ub)) => ua.origin() == ub.origin(),
        _ => false,
    }
}

fn active_token(token: Option<String>, ctx: &Context, url: &str) -> Result<Option<String>> {
    if token.is_some() {
        return Ok(token);
    }
    let profile = ctx.config.profile.get(&ctx.profile);
    let base_matches = profile
        .and_then(|p| p.base_url.as_deref())
        .is_some_and(|base| same_origin(base, url));
    if !base_matches {
        return Ok(None);
    }
    if let Some(p) = profile
        && p.token_url.is_some()
    {
        return crate::oauth::get_valid_access_token(&ctx.profile, p);
    }
    crate::secrets::get_token(&ctx.profile)
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
            let token = active_token(token, ctx, &url)?;
            do_request("GET", &url, None, token, headers, timeout, ctx)
        }
        HttpSubcommand::Post {
            url,
            body,
            token,
            headers,
            timeout,
        } => {
            let url = resolve_url(&url, ctx)?;
            let token = active_token(token, ctx, &url)?;
            do_request("POST", &url, body.as_deref(), token, headers, timeout, ctx)
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
    ctx: &Context,
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
    let body_text = response.text()?;
    let body_json = serde_json::from_str::<serde_json::Value>(&body_text).ok();

    if ctx.output == OutputFormat::Json {
        let payload = serde_json::json!({
            "method": method,
            "url": url,
            "status": status.as_u16(),
            "ok": status.is_success(),
            "body": body_json.unwrap_or(serde_json::Value::String(body_text)),
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
        if !status.is_success() {
            bail!("HTTP {}", status.as_u16());
        }
        return Ok(());
    }

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

    if let Some(json) = body_json {
        println!("{}", serde_json::to_string_pretty(&json)?);
    } else {
        println!("{body_text}");
    }

    if !status.is_success() {
        bail!("HTTP {}", status.as_u16());
    }

    Ok(())
}
