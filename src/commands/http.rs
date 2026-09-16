use crate::{context::Context, output::OutputFormat};
use anyhow::{Context as _, Result, bail};
use clap::{Args, Subcommand};
use colored::Colorize;
use std::time::{Duration, Instant};

/// Flags shared by every verb: the target, auth, headers, query params, and
/// transport knobs. Flattened into each subcommand so `get`/`head`/`delete` (no body)
/// and `post`/`put`/`patch` (with `BodyArgs`) stay consistent without repeating
/// definitions.
#[derive(Args)]
pub struct CommonArgs {
    /// URL or path (path uses profile base_url)
    url: String,
    /// Bearer token for Authorization header [env: TOOLER_HTTP_TOKEN] (ignored if
    /// --basic is set)
    #[arg(short, long, env = "TOOLER_HTTP_TOKEN")]
    token: Option<String>,
    /// Extra headers in "Key: Value" format
    #[arg(short = 'H', long = "header")]
    headers: Vec<String>,
    /// Query parameter in "key=value" format (repeatable, values are URL-encoded)
    #[arg(short = 'q', long = "query")]
    query: Vec<String>,
    /// HTTP Basic auth as "user:pass" (takes precedence over --token/profile auth)
    #[arg(long)]
    basic: Option<String>,
    /// Timeout in seconds
    #[arg(long, default_value_t = 10)]
    timeout: u64,
    /// Print the outgoing request and every response header before the body
    #[arg(short, long)]
    verbose: bool,
}

/// Body flags for the write verbs (`post`/`put`/`patch`/`delete`) -- mutually
/// exclusive via clap's `body_source` group, so passing e.g. both `--body` and `--form`
/// is a parse-time error rather than a silently-ignored one.
#[derive(Args)]
pub struct BodyArgs {
    /// Raw request body (defaults to Content-Type: application/json unless -H already
    /// sets one)
    #[arg(short, long, group = "body_source")]
    body: Option<String>,
    /// Read the request body from a file
    #[arg(long, value_name = "PATH", group = "body_source")]
    body_file: Option<std::path::PathBuf>,
    /// Form field in "key=value" format (repeatable) -- sends
    /// application/x-www-form-urlencoded
    #[arg(short = 'f', long = "form", group = "body_source")]
    form: Vec<String>,
}

#[derive(Args)]
pub struct ReadArgs {
    #[command(flatten)]
    common: CommonArgs,
}

#[derive(Args)]
pub struct WriteArgs {
    #[command(flatten)]
    common: CommonArgs,
    #[command(flatten)]
    body: BodyArgs,
}

#[derive(Args)]
pub struct HttpArgs {
    #[command(subcommand)]
    pub subcommand: HttpSubcommand,
}

/// `login`'s own flags: everything a write request needs (URL, headers, body) plus
/// which JSON field in the response holds the token to store.
#[derive(Args)]
pub struct LoginArgs {
    #[command(flatten)]
    common: CommonArgs,
    #[command(flatten)]
    body: BodyArgs,
    /// Dot-path to the token field in the JSON response (e.g. "access_token" or
    /// "data.jwt")
    #[arg(long, default_value = "access_token")]
    token_field: String,
}

#[derive(Subcommand)]
pub enum HttpSubcommand {
    /// Perform a GET request
    Get(ReadArgs),
    /// Perform a HEAD request (headers only, no response body)
    Head(ReadArgs),
    /// Perform a DELETE request
    Delete(WriteArgs),
    /// Perform a POST request
    Post(WriteArgs),
    /// Perform a PUT request
    Put(WriteArgs),
    /// Perform a PATCH request
    Patch(WriteArgs),
    /// POST to a login/auth endpoint and store the returned bearer token/JWT for the
    /// active profile (in the OS keychain, same as `config set profile.<name>.token`) --
    /// subsequent requests with --profile <name> then send it automatically
    Login(LoginArgs),
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

enum BodySource {
    Raw(String),
    Form(Vec<(String, String)>),
}

/// Resolves `BodyArgs` into at most one body source -- clap's `body_source` group
/// already guarantees at most one of `body`/`body_file`/`form` was actually supplied.
fn resolve_body(args: &BodyArgs) -> Result<Option<BodySource>> {
    if let Some(b) = &args.body {
        return Ok(Some(BodySource::Raw(b.clone())));
    }
    if let Some(path) = &args.body_file {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("reading body file {}", path.display()))?;
        return Ok(Some(BodySource::Raw(content)));
    }
    if !args.form.is_empty() {
        let pairs = args
            .form
            .iter()
            .map(|f| {
                f.split_once('=')
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .ok_or_else(|| {
                        anyhow::anyhow!("invalid --form value '{f}', expected key=value")
                    })
            })
            .collect::<Result<Vec<_>>>()?;
        return Ok(Some(BodySource::Form(pairs)));
    }
    Ok(None)
}

struct RequestOptions {
    method: reqwest::Method,
    url: String,
    query: Vec<String>,
    headers: Vec<String>,
    token: Option<String>,
    basic: Option<String>,
    body: Option<BodySource>,
    verbose: bool,
    timeout: u64,
}

pub fn run(args: HttpArgs, ctx: &Context) -> Result<()> {
    match args.subcommand {
        HttpSubcommand::Get(a) => run_read(reqwest::Method::GET, a, ctx),
        HttpSubcommand::Head(a) => run_read(reqwest::Method::HEAD, a, ctx),
        HttpSubcommand::Delete(a) => run_write(reqwest::Method::DELETE, a, ctx),
        HttpSubcommand::Post(a) => run_write(reqwest::Method::POST, a, ctx),
        HttpSubcommand::Put(a) => run_write(reqwest::Method::PUT, a, ctx),
        HttpSubcommand::Patch(a) => run_write(reqwest::Method::PATCH, a, ctx),
        HttpSubcommand::Login(a) => run_login(a, ctx),
    }
}

fn run_read(method: reqwest::Method, args: ReadArgs, ctx: &Context) -> Result<()> {
    let opts = build_options(method, args.common, None, ctx)?;
    execute(opts, ctx)
}

fn run_write(method: reqwest::Method, args: WriteArgs, ctx: &Context) -> Result<()> {
    let body = resolve_body(&args.body)?;
    let opts = build_options(method, args.common, body, ctx)?;
    execute(opts, ctx)
}

fn build_options(
    method: reqwest::Method,
    common: CommonArgs,
    body: Option<BodySource>,
    ctx: &Context,
) -> Result<RequestOptions> {
    let url = resolve_url(&common.url, ctx)?;
    let token = if common.basic.is_some() {
        None
    } else {
        active_token(common.token, ctx, &url)?
    };
    Ok(RequestOptions {
        method,
        url,
        query: common.query,
        headers: common.headers,
        token,
        basic: common.basic,
        body,
        verbose: common.verbose,
        timeout: common.timeout,
    })
}

fn has_content_type_header(headers: &[String]) -> bool {
    headers.iter().any(|h| {
        h.split_once(':')
            .is_some_and(|(k, _)| k.trim().eq_ignore_ascii_case("content-type"))
    })
}

struct SentResponse {
    url: reqwest::Url,
    status: reqwest::StatusCode,
    headers: Vec<(String, String)>,
    body_text: String,
    elapsed_ms: u128,
}

/// Builds and sends the request `opts` describes -- shared by `execute` (every plain
/// verb) and `run_login` (which needs the raw response instead of `execute`'s display
/// logic). Verbose request-side logging lives here since both callers want it.
fn send_request(opts: &RequestOptions) -> Result<SentResponse> {
    let mut url =
        reqwest::Url::parse(&opts.url).with_context(|| format!("invalid URL: {}", opts.url))?;
    if !opts.query.is_empty() {
        let mut pairs = url.query_pairs_mut();
        for q in &opts.query {
            let (k, v) = q.split_once('=').ok_or_else(|| {
                anyhow::anyhow!("invalid --query value '{q}', expected key=value")
            })?;
            pairs.append_pair(k, v);
        }
    }

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(opts.timeout))
        .build()?;
    let mut req = client.request(opts.method.clone(), url.clone());

    if let Some(basic) = &opts.basic {
        let (user, pass) = basic
            .split_once(':')
            .ok_or_else(|| anyhow::anyhow!("invalid --basic value, expected user:pass"))?;
        req = req.basic_auth(user, Some(pass));
    } else if let Some(t) = &opts.token {
        req = req.header("Authorization", format!("Bearer {t}"));
    }

    for h in &opts.headers {
        if let Some((key, val)) = h.split_once(':') {
            req = req.header(key.trim(), val.trim());
        }
    }

    match &opts.body {
        Some(BodySource::Raw(b)) => {
            if !has_content_type_header(&opts.headers) {
                req = req.header("Content-Type", "application/json");
            }
            req = req.body(b.clone());
        }
        Some(BodySource::Form(pairs)) => {
            req = req.form(pairs);
        }
        None => {}
    }

    if opts.verbose {
        println!("{}", format!("> {} {}", opts.method, url).bold());
        if let Some(basic) = &opts.basic {
            let user = basic.split_once(':').map(|(u, _)| u).unwrap_or(basic);
            println!("  {}: Basic *** ({})", "Authorization".dimmed(), user);
        } else if opts.token.is_some() {
            println!("  {}: Bearer ***", "Authorization".dimmed());
        }
        for h in &opts.headers {
            println!("  {h}");
        }
        match &opts.body {
            Some(BodySource::Raw(b)) => {
                println!("  {}", format!("(body, {} bytes)", b.len()).dimmed())
            }
            Some(BodySource::Form(pairs)) => {
                println!("  {}", format!("(form, {} field(s))", pairs.len()).dimmed())
            }
            None => {}
        }
        println!();
    }

    let started = Instant::now();
    let response = req
        .send()
        .with_context(|| format!("Request failed: {url}"))?;
    let elapsed_ms = started.elapsed().as_millis();

    let status = response.status();
    let headers: Vec<(String, String)> = response
        .headers()
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let body_text = response.text()?;

    Ok(SentResponse {
        url,
        status,
        headers,
        body_text,
        elapsed_ms,
    })
}

fn execute(opts: RequestOptions, ctx: &Context) -> Result<()> {
    let sent = send_request(&opts)?;
    let SentResponse {
        url,
        status,
        headers: response_headers,
        body_text,
        elapsed_ms,
    } = sent;
    let body_json = serde_json::from_str::<serde_json::Value>(&body_text).ok();

    if ctx.output == OutputFormat::Json {
        let headers_map: serde_json::Map<String, serde_json::Value> = response_headers
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
            .collect();
        let payload = serde_json::json!({
            "method": opts.method.as_str(),
            "url": url.as_str(),
            "status": status.as_u16(),
            "ok": status.is_success(),
            "elapsed_ms": elapsed_ms,
            "headers": headers_map,
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

    if opts.verbose {
        println!("{}", format!("< {status_colored} ({elapsed_ms}ms)").bold());
        for (k, v) in &response_headers {
            println!("  {}: {v}", k.dimmed());
        }
        println!();
    } else {
        println!(
            "{} {} — {} ({elapsed_ms}ms)",
            opts.method.as_str().bold().cyan(),
            url.as_str().dimmed(),
            status_colored
        );
        println!("{}", "─".repeat(50).dimmed());
    }

    if let Some(json) = body_json {
        println!("{}", serde_json::to_string_pretty(&json)?);
    } else if !body_text.is_empty() {
        println!("{body_text}");
    }

    if !status.is_success() {
        bail!("HTTP {}", status.as_u16());
    }

    Ok(())
}

/// `tooler http login`: POSTs to an auth endpoint, extracts `token_field` (a dot-path,
/// see `commands::play::apply_json_filter`) from the JSON response, and stores it as
/// the active profile's bearer token in the OS keychain -- the same slot
/// `config set profile.<name>.token` and `tooler http`'s own auto-attach
/// (`active_token`) already use, so it's picked up by every later request against that
/// profile with no further setup. Never echoes the token itself.
fn run_login(args: LoginArgs, ctx: &Context) -> Result<()> {
    let body = resolve_body(&args.body)?;
    let opts = build_options(reqwest::Method::POST, args.common, body, ctx)?;
    let sent = send_request(&opts)?;

    if !sent.status.is_success() {
        let snippet: String = sent.body_text.chars().take(300).collect();
        bail!(
            "login request failed: HTTP {}{}",
            sent.status.as_u16(),
            if snippet.trim().is_empty() {
                String::new()
            } else {
                format!(": {snippet}")
            }
        );
    }

    let token = crate::commands::play::apply_json_filter(&sent.body_text, &args.token_field)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| {
            let snippet: String = sent.body_text.chars().take(300).collect();
            anyhow::anyhow!(
                "field '{}' not found (or empty) in the login response: {snippet}",
                args.token_field
            )
        })?;

    crate::secrets::set_token(&ctx.profile, &token)?;

    if ctx.output == OutputFormat::Json {
        let payload = serde_json::json!({
            "profile": ctx.profile,
            "token_saved": true,
            "status": sent.status.as_u16(),
            "elapsed_ms": sent.elapsed_ms,
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    println!(
        "{} token saved for profile {} ({}, {}ms)",
        "✓".green().bold(),
        ctx.profile.cyan(),
        sent.status.as_u16().to_string().green(),
        sent.elapsed_ms
    );
    Ok(())
}
