use crate::{
    commands::{echo::EchoArgs, info::InfoArgs},
    context::Context,
};
use anyhow::Result;
use clap::Args;
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler, ServiceExt,
    handler::server::{
        router::{prompt::PromptRouter, tool::ToolRouter},
        wrapper::Parameters,
    },
    model::{
        CallToolResult, ContentBlock, Implementation, ListResourcesResult, PaginatedRequestParams,
        PromptMessage, ReadResourceRequestParams, ReadResourceResult, Resource, ResourceContents,
        Role, ServerCapabilities, ServerInfo,
    },
    prompt, prompt_handler, prompt_router,
    service::RequestContext,
    tool, tool_handler, tool_router,
    transport::stdio,
};
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Args)]
pub struct McpArgs {
    /// Serve over HTTP (Streamable HTTP transport) instead of stdio
    #[arg(long)]
    pub http: bool,
    /// Bind address when --http is set
    #[arg(long, default_value = "127.0.0.1:8642")]
    pub bind: String,
    /// Bearer token required for --http requests [env: TOOLER_MCP_TOKEN]
    #[arg(long, env = "TOOLER_MCP_TOKEN")]
    pub token: Option<String>,
    /// Append a JSON line per MCP tool call to this file [env: TOOLER_MCP_AUDIT_LOG]
    #[arg(long, env = "TOOLER_MCP_AUDIT_LOG")]
    pub audit_log: Option<std::path::PathBuf>,
}

pub fn run(args: McpArgs, _ctx: &Context) -> Result<()> {
    if args.http && args.token.is_none() {
        anyhow::bail!(
            "tooler mcp --http requires a bearer token: pass --token or set TOOLER_MCP_TOKEN. \
             Refusing to start an unauthenticated HTTP MCP server."
        );
    }

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        if args.http {
            run_http(args).await
        } else {
            let service = ToolerMcp::with_audit_log(args.audit_log)
                .serve(stdio())
                .await?;
            service.waiting().await?;
            anyhow::Ok(())
        }
    })
}

async fn run_http(args: McpArgs) -> Result<()> {
    use rmcp::transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    };
    use std::sync::Arc;

    let token = Arc::new(args.token.expect("checked in run()"));
    let audit_log = args.audit_log;

    let service = StreamableHttpService::new(
        move || Ok(ToolerMcp::with_audit_log(audit_log.clone())),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );

    let router = axum::Router::new().nest_service("/mcp", service).layer(
        axum::middleware::from_fn_with_state(token, require_bearer_token),
    );

    let addr: std::net::SocketAddr = args
        .bind
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid --bind address '{}': {e}", args.bind))?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    eprintln!("tooler mcp: listening on http://{addr}/mcp (bearer auth required)");
    axum::serve(listener, router).await?;
    Ok(())
}

async fn require_bearer_token(
    axum::extract::State(expected): axum::extract::State<std::sync::Arc<String>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Result<axum::response::Response, axum::http::StatusCode> {
    let provided = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    match provided {
        Some(p) if constant_time_eq(p.as_bytes(), expected.as_bytes()) => Ok(next.run(req).await),
        _ => Err(axum::http::StatusCode::UNAUTHORIZED),
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

// ── argv helpers ────────────────────────────────────────────────────────────

fn push_flag(argv: &mut Vec<String>, flag: &str, present: bool) {
    if present {
        argv.push(flag.to_string());
    }
}

fn push_opt(argv: &mut Vec<String>, flag: &str, value: &Option<String>) {
    if let Some(v) = value {
        argv.push(flag.to_string());
        argv.push(v.clone());
    }
}

fn push_opt_num<T: ToString>(argv: &mut Vec<String>, flag: &str, value: Option<T>) {
    if let Some(v) = value {
        argv.push(flag.to_string());
        argv.push(v.to_string());
    }
}

fn is_profile_token_key(key: &str) -> bool {
    crate::commands::config::parse_profile_key(key).is_some_and(|(_, field)| field == "token")
}

fn push_repeated(argv: &mut Vec<String>, flag: &str, values: &[String]) {
    for v in values {
        argv.push(flag.to_string());
        argv.push(v.clone());
    }
}

impl ToolerMcp {
    /// Self-invokes the current `tooler` binary as a subprocess with the given
    /// argv, and returns its output as an MCP tool result. Always appends
    /// `--output json` (a no-op for commands that don't branch on it) and sets
    /// `NO_COLOR=1` so plain-text responses come back without ANSI escapes.
    async fn exec_self(
        &self,
        mut argv: Vec<String>,
        cwd: &Option<String>,
    ) -> Result<CallToolResult, McpError> {
        let started = std::time::Instant::now();
        let logged_argv = argv.clone();

        let exe = std::env::current_exe().map_err(|e| {
            McpError::internal_error(format!("cannot resolve tooler binary: {e}"), None)
        })?;

        argv.push("--output".to_string());
        argv.push("json".to_string());

        let mut cmd = tokio::process::Command::new(exe);
        cmd.args(&argv).env("NO_COLOR", "1");
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }

        let output = cmd
            .output()
            .await
            .map_err(|e| McpError::internal_error(format!("failed to launch tooler: {e}"), None))?;

        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

        let result = if output.status.success() {
            CallToolResult::success(vec![ContentBlock::text(stdout)])
        } else {
            let mut message = String::new();
            if !stdout.trim().is_empty() {
                message.push_str(&stdout);
            }
            if !stderr.trim().is_empty() {
                if !message.is_empty() {
                    message.push_str("\n--- stderr ---\n");
                }
                message.push_str(&stderr);
            }
            if message.is_empty() {
                message = format!("tooler exited with status {}", output.status);
            }
            CallToolResult::error(vec![ContentBlock::text(message)])
        };

        self.write_audit(
            &logged_argv,
            cwd,
            output.status.success(),
            started.elapsed().as_millis(),
        );
        Ok(result)
    }

    /// Appends one JSON line per MCP tool call to `self.audit_log`, if set. A plain
    /// blocking write is intentional: this is one append per tool call (low
    /// frequency), not a hot path, so `tokio::fs`/locking would be over-engineering
    /// for a single-user CLI's audit trail.
    fn write_audit(&self, argv: &[String], cwd: &Option<String>, success: bool, duration_ms: u128) {
        let Some(path) = &self.audit_log else {
            return;
        };
        let line = serde_json::json!({
            "ts": chrono::Utc::now().to_rfc3339(),
            "argv": argv,
            "cwd": cwd,
            "success": success,
            "duration_ms": duration_ms,
        });
        use std::io::Write;
        let result = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut f| writeln!(f, "{line}"));
        if let Err(e) = result {
            eprintln!(
                "tooler mcp: failed to write audit log {}: {e}",
                path.display()
            );
        }
    }
}

// ── env ───────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
struct EnvShowArgs {
    /// Path to the .env file (defaults to ".env")
    file: Option<String>,
    /// Show real values instead of masking them
    #[serde(default)]
    reveal: bool,
    /// Working directory to resolve the file against
    cwd: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct EnvListArgs {
    /// Path to the .env file (defaults to ".env")
    file: Option<String>,
    cwd: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct EnvGetArgs {
    /// Variable name to look up
    key: String,
    /// Path to the .env file (defaults to ".env")
    file: Option<String>,
    cwd: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct EnvDiffArgs {
    file_a: String,
    file_b: String,
    cwd: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct EnvCheckArgs {
    /// Reference file (e.g. .env.example)
    reference: String,
    /// File to check (defaults to ".env")
    target: Option<String>,
    cwd: Option<String>,
}

// ── http ──────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
struct HttpGetArgs {
    /// URL or path (path uses the profile's base_url)
    url: String,
    /// Extra headers in "Key: Value" format
    #[serde(default)]
    headers: Vec<String>,
    /// Timeout in seconds
    timeout: Option<u64>,
    /// Config profile to use for base_url/token resolution
    profile: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct HttpPostArgs {
    url: String,
    /// JSON body string
    body: Option<String>,
    #[serde(default)]
    headers: Vec<String>,
    timeout: Option<u64>,
    profile: Option<String>,
}

// ── check ─────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
struct CheckUrlArgs {
    url: String,
    /// Timeout in seconds
    timeout: Option<u64>,
}

#[derive(Deserialize, JsonSchema)]
struct CheckPortArgs {
    host: String,
    port: u16,
    /// Timeout in seconds
    timeout: Option<u64>,
}

// ── config ────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
struct ConfigGetArgs {
    /// Config key, e.g. "default.output"
    key: String,
}

#[derive(Deserialize, JsonSchema)]
struct ConfigUnsetArgs {
    /// Config key, e.g. "profile.staging.token"
    key: String,
}

#[derive(Deserialize, JsonSchema)]
struct ConfigSetArgs {
    /// Config key, e.g. "default.output"
    key: String,
    value: String,
}

// ── json ──────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
struct JsonQueryArgs {
    /// JSON file path (stdin input is not available over MCP)
    file: String,
    /// Extract a field by dot-notation key (e.g. "user.name")
    key: Option<String>,
    /// Compact output instead of pretty-print
    #[serde(default)]
    compact: bool,
}

// ── run / play ────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
struct RunMcpArgs {
    /// Script name to run (omit to list available scripts)
    script: Option<String>,
    /// Show the command without executing it
    #[serde(default)]
    dry: bool,
    /// Extra arguments appended to the script command
    #[serde(default)]
    extra: Vec<String>,
    /// Working directory containing .tooler.toml
    cwd: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct PlayMcpArgs {
    /// Playbook YAML file to run (omit with init=true to generate a sample)
    file: Option<String>,
    /// Preview tasks without executing them
    #[serde(default)]
    dry: bool,
    /// Variable overrides in "key=value" form
    #[serde(default)]
    vars: Vec<String>,
    /// Comma-separated list of tags to run
    tags: Option<String>,
    /// Generate a sample playbook.yml instead of running one
    #[serde(default)]
    init: bool,
    cwd: Option<String>,
}

// ── git ───────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
struct GitCwdArgs {
    /// Repository directory (defaults to the MCP server's own working directory)
    cwd: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct GitCleanArgs {
    /// Also delete from origin
    #[serde(default)]
    remote: bool,
    /// Actually delete (default is preview-only)
    #[serde(default)]
    confirm: bool,
    /// Only branches with a trailing DDMMYY date suffix on/after this date (DDMMYY).
    /// When set (with `before`), targets any local branch in range regardless of
    /// merge status, instead of the default merged-only cleanup.
    after: Option<String>,
    /// Only branches with a trailing DDMMYY date suffix on/before this date (DDMMYY)
    before: Option<String>,
    cwd: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct GitChangelogArgs {
    /// Starting tag or commit (defaults to the latest tag)
    from: Option<String>,
    cwd: Option<String>,
}

// ── scaffold ──────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
struct ScaffoldNewArgs {
    /// Template name (see tooler_scaffold_list)
    template: String,
    /// Project name
    name: String,
    /// Destination directory (defaults to ./<name>)
    dir: Option<String>,
    cwd: Option<String>,
}

// ── report ────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
struct ReportArgs {
    /// Named JSON inputs, each `NAME=PATH` (e.g. from another tooler command's
    /// `--output json`). Omit to build the report from a single unnamed source.
    #[serde(default)]
    input: Vec<String>,
    /// Output file path to write the generated report to
    out: String,
    /// Report title
    title: Option<String>,
    cwd: Option<String>,
}

// ── db ────────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
struct DbQueryArgs {
    /// Server profile to run psql/mysql on (see tooler_server_list)
    server: String,
    /// SQL query (read-only: SELECT/SHOW/EXPLAIN/WITH/DESCRIBE only)
    sql: String,
    /// Remote path to a dotenv-style file (e.g. Laravel .env) to read DB_* credentials
    /// from. Preferred over passing credentials explicitly.
    env: Option<String>,
    /// DB engine when not using `env`: mysql or postgres
    engine: Option<String>,
    /// DB host as reachable from the server profile (when not using `env`)
    host: Option<String>,
    /// DB port (when not using `env`; defaults to the engine's standard port)
    port: Option<u16>,
    /// Database name (when not using `env`)
    database: Option<String>,
    /// DB username (when not using `env`)
    user: Option<String>,
    /// Cap the number of rows returned
    max_rows: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
struct DbBackupArgs {
    /// Server profile to run pg_dump/mysqldump on (see tooler_server_list)
    server: String,
    /// Local file path to write the dump to
    out: String,
    /// Remote path to a dotenv-style file (e.g. Laravel .env) to read DB_* credentials
    /// from. Preferred over passing credentials explicitly.
    env: Option<String>,
    /// DB engine when not using `env`: mysql or postgres
    engine: Option<String>,
    /// DB host as reachable from the server profile (when not using `env`)
    host: Option<String>,
    /// DB port (when not using `env`; defaults to the engine's standard port)
    port: Option<u16>,
    /// Database name (when not using `env`)
    database: Option<String>,
    /// DB username (when not using `env`)
    user: Option<String>,
    /// Skip gzip compression of the dump
    #[serde(default)]
    no_gzip: bool,
}

#[derive(Deserialize, JsonSchema)]
struct DbRestoreArgs {
    /// Server profile to run psql/mysql on (see tooler_server_list)
    server: String,
    /// Local dump file to restore (gzip-compressed input is auto-detected)
    #[serde(rename = "in")]
    input: String,
    env: Option<String>,
    engine: Option<String>,
    host: Option<String>,
    port: Option<u16>,
    database: Option<String>,
    user: Option<String>,
    /// Actually run the restore. Without this, the call only previews what would happen
    /// (bytes to send, target database) and makes no change.
    #[serde(default)]
    confirm: bool,
}

// ── ps ────────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
struct PsListArgs {
    /// Server profile (see tooler_server_list)
    server: String,
    /// Only show processes whose command line (or PID) matches this substring
    filter: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct PsKillArgs {
    server: String,
    /// Process ID to signal
    pid: u32,
    /// Signal name or number (defaults to TERM)
    signal: Option<String>,
    /// Run via sudo. A sudo password, if needed, must never be passed as a tool
    /// argument -- set TOOLER_SUDO_PASS in the MCP server's own environment instead
    /// (or rely on passwordless/NOPASSWD sudo).
    #[serde(default)]
    sudo: bool,
    /// Actually send the signal. Without this, the call only previews what would happen.
    #[serde(default)]
    confirm: bool,
}

// ── fs ────────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
struct FsCatArgs {
    /// Server profile (see tooler_server_list)
    server: String,
    /// Remote file path
    path: String,
}

#[derive(Deserialize, JsonSchema)]
struct FsWriteArgs {
    server: String,
    path: String,
    /// Local file to read the new content from (binary-safe). Exactly one of
    /// `from_file`/`content` must be set.
    from_file: Option<String>,
    /// Literal text to write. Exactly one of `from_file`/`content` must be set.
    content: Option<String>,
    /// Actually write the file. Without this, the call only previews what would happen
    /// (byte count) and makes no change.
    #[serde(default)]
    confirm: bool,
}

#[derive(Deserialize, JsonSchema)]
struct FsDiffArgs {
    server: String,
    /// Remote file path
    path: String,
    /// Local file to compare against
    local: String,
}

// ── deploy ───────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
struct DeployRunArgs {
    /// Server profile (see tooler_server_list)
    server: String,
    /// Remote path (git repo) to deploy
    path: String,
    /// Pull the latest code (git pull) in `path` before restarting
    #[serde(default)]
    pull: bool,
    /// Command to run remotely in `path` after pulling (e.g. a build step)
    build: Option<String>,
    /// Command to restart the service (e.g. "systemctl restart myapp")
    restart: Option<String>,
    /// URL to check after restarting
    health_url: Option<String>,
    /// Timeout in seconds for each health check attempt (default 5)
    health_timeout: Option<u64>,
    /// Number of health check attempts before giving up (default 3)
    health_retries: Option<u32>,
    /// Seconds to wait between health check attempts (default 2)
    health_delay: Option<u64>,
    /// Run the restart command via sudo. A sudo password, if needed, must never be
    /// passed as a tool argument -- set TOOLER_SUDO_PASS in the MCP server's own
    /// environment instead (or rely on passwordless/NOPASSWD sudo).
    #[serde(default)]
    sudo: bool,
    /// Actually run the deploy. Without this, the call only previews the steps that
    /// would run and makes no change.
    #[serde(default)]
    confirm: bool,
}

// ── fleet ─────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
struct FleetExecArgs {
    /// Comma-separated server profile names (omit if all=true)
    servers: Option<String>,
    /// Target every configured server profile
    #[serde(default)]
    all: bool,
    /// Command to run
    command: String,
    /// Run command with sudo
    #[serde(default)]
    sudo: bool,
}

#[derive(Deserialize, JsonSchema)]
struct FleetCheckArgs {
    /// Comma-separated server profile names (omit if all=true)
    servers: Option<String>,
    /// Target every configured server profile
    #[serde(default)]
    all: bool,
}

// ── stat ──────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
struct StatArgs {
    /// Server profile (see tooler_server_list)
    server: String,
}

// ── gh ────────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
struct GhPrsArgs {
    /// Repository as owner/name (defaults to the repo in the current directory)
    repo: Option<String>,
    /// Only PRs created on/after this date (YYYY-MM-DD)
    after: Option<String>,
    /// Only PRs created on/before this date (YYYY-MM-DD)
    before: Option<String>,
    /// PR state to include: open, closed, merged, or all
    state: Option<String>,
    /// Max PRs to fetch from GitHub before date filtering
    limit: Option<u32>,
    cwd: Option<String>,
}

// ── systemd ───────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
struct SystemdUnitArgs {
    /// Server profile (see tooler_server_list)
    server: String,
    /// Unit name, e.g. nginx or myapp.service
    unit: String,
}

#[derive(Deserialize, JsonSchema)]
struct SystemdRestartArgs {
    server: String,
    unit: String,
    /// Run via sudo. A sudo password, if needed, must never be passed as a tool
    /// argument -- set TOOLER_SUDO_PASS in the MCP server's own environment instead
    /// (or rely on passwordless/NOPASSWD sudo).
    #[serde(default)]
    sudo: bool,
}

#[derive(Deserialize, JsonSchema)]
struct SystemdLogsArgs {
    server: String,
    unit: String,
    /// Number of lines
    lines: Option<u32>,
    /// Run via sudo (some systems restrict journal access to root)
    #[serde(default)]
    sudo: bool,
}

// ── cron ──────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
struct CronServerArgs {
    server: String,
}

#[derive(Deserialize, JsonSchema)]
struct CronAddArgs {
    server: String,
    /// Full crontab line, e.g. "0 3 * * * /path/to/backup.sh"
    line: String,
}

#[derive(Deserialize, JsonSchema)]
struct CronRemoveArgs {
    server: String,
    /// Fixed substring to match (not a regex) -- matching lines are dropped
    pattern: String,
}

// ── logs ──────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
struct LogsTailArgs {
    server: String,
    /// Remote file path
    path: String,
    /// Number of lines
    lines: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
struct LogsGrepArgs {
    server: String,
    path: String,
    /// Fixed substring to match (not a regex)
    pattern: String,
    /// Cap the number of matching lines returned
    max_lines: Option<usize>,
}

// ── server profiles ───────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
struct ServerNameArgs {
    name: String,
}

#[derive(Deserialize, JsonSchema)]
struct ServerAddArgs {
    name: String,
    host: String,
    user: Option<String>,
    port: Option<u16>,
    /// Path to private key, e.g. ~/.ssh/id_rsa
    key: Option<String>,
    /// Remote SSL certificate directory (default /etc/nginx/ssl)
    ssl_dir: Option<String>,
}

// ── ssh ───────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
struct SshCheckArgs {
    /// Server profile name
    server: String,
}

#[derive(Deserialize, JsonSchema)]
struct SshExecArgs {
    /// Server profile name
    server: String,
    /// Command to run on the remote server
    command: String,
    /// Run the command with sudo
    #[serde(default)]
    sudo: bool,
}

#[derive(Deserialize, JsonSchema)]
struct SshCopyArgs {
    /// Local file path
    local: String,
    /// Destination as server:path (e.g. gdn:/tmp/file)
    remote: String,
}

#[derive(Deserialize, JsonSchema)]
struct SshSslArgs {
    /// Server profile name
    server: String,
    /// Local .pfx certificate file
    pfx: String,
    /// Local private key file
    key: String,
    /// Remote SSL directory (defaults to the server profile's ssl_dir or /etc/nginx/ssl)
    remote_dir: Option<String>,
    /// Certificate filename on the server (defaults to wildcard.crt)
    cert_name: Option<String>,
    /// Key filename on the server (defaults to wildcard.key)
    key_name: Option<String>,
}

// ── server ────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct ToolerMcp {
    tool_router: ToolRouter<ToolerMcp>,
    prompt_router: PromptRouter<ToolerMcp>,
    audit_log: Option<std::path::PathBuf>,
}

impl ToolerMcp {
    pub fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
            prompt_router: Self::prompt_router(),
            audit_log: None,
        }
    }

    pub fn with_audit_log(audit_log: Option<std::path::PathBuf>) -> Self {
        Self {
            audit_log,
            ..Self::new()
        }
    }
}

impl Default for ToolerMcp {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Deserialize, JsonSchema)]
struct DeployCheckArgs {
    /// Config profile name (matches profile.<name> / server.<name> in tooler config)
    profile: String,
}

#[derive(Deserialize, JsonSchema)]
struct EnvParityArgs {
    /// First .env file path
    file_a: String,
    /// Second .env file path
    file_b: String,
}

#[prompt_router]
impl ToolerMcp {
    #[prompt(
        name = "deploy_check",
        description = "Guide a pre-deploy check: env parity, URL health, and git status for a profile"
    )]
    async fn deploy_check(
        &self,
        Parameters(args): Parameters<DeployCheckArgs>,
    ) -> Vec<PromptMessage> {
        vec![PromptMessage::new_text(
            Role::User,
            format!(
                "Run a deploy readiness check for the '{p}' profile:\n\
                 1. Call tooler_env_diff to compare the local .env against .env.example.\n\
                 2. Call tooler_check_url against the profile's base_url to confirm it's reachable.\n\
                 3. Call tooler_git_summary to confirm the working tree is clean.\n\
                 Summarize pass/fail for each step at the end. If a Playwright MCP server is \
                 also configured, consider opening the URL with its browser tools to visually \
                 confirm the page renders correctly.",
                p = args.profile
            ),
        )]
    }

    #[prompt(
        name = "env_parity",
        description = "Guide a diff between two .env files and summarize drift"
    )]
    async fn env_parity(&self, Parameters(args): Parameters<EnvParityArgs>) -> Vec<PromptMessage> {
        vec![PromptMessage::new_text(
            Role::User,
            format!(
                "Call tooler_env_diff with file_a=\"{a}\" and file_b=\"{b}\", then summarize which \
                 keys are missing on each side and flag anything that looks like a required \
                 variable.",
                a = args.file_a,
                b = args.file_b
            ),
        )]
    }
}

#[tool_router]
impl ToolerMcp {
    #[tool(
        description = "Show system information: working directory and environment variables",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn tooler_info(
        &self,
        Parameters(args): Parameters<InfoArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec!["info".to_string()];
        push_flag(&mut argv, "--env", args.env);
        push_flag(&mut argv, "--dir", args.dir);
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Run environment/health checks: git, OS keychain, SSH key files, \
                        self-exe resolution, config summary",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn tooler_doctor(&self) -> Result<CallToolResult, McpError> {
        self.exec_self(vec!["doctor".to_string()], &None).await
    }

    #[tool(
        description = "Echo text with optional color/uppercase/repeat formatting",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn tooler_echo(
        &self,
        Parameters(args): Parameters<EchoArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec!["echo".to_string()];
        argv.extend(args.text.clone());
        push_flag(&mut argv, "--upper", args.upper);
        argv.push("--color".to_string());
        argv.push(args.color.clone());
        argv.push("--repeat".to_string());
        argv.push(args.repeat.to_string());
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Pretty-print and query a JSON file by dot-notation key",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn tooler_json(
        &self,
        Parameters(args): Parameters<JsonQueryArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec!["json".to_string(), args.file.clone()];
        push_opt(&mut argv, "--key", &args.key);
        push_flag(&mut argv, "--compact", args.compact);
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Show variables from a .env file (values masked by default)",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn tooler_env_show(
        &self,
        Parameters(args): Parameters<EnvShowArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec!["env".to_string(), "show".to_string()];
        if let Some(f) = &args.file {
            argv.push(f.clone());
        }
        push_flag(&mut argv, "--reveal", args.reveal);
        self.exec_self(argv, &args.cwd).await
    }

    #[tool(
        description = "List variable names in a .env file",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn tooler_env_list(
        &self,
        Parameters(args): Parameters<EnvListArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec!["env".to_string(), "list".to_string()];
        if let Some(f) = &args.file {
            argv.push(f.clone());
        }
        self.exec_self(argv, &args.cwd).await
    }

    #[tool(
        description = "Get a single variable's value from a .env file",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn tooler_env_get(
        &self,
        Parameters(args): Parameters<EnvGetArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec!["env".to_string(), "get".to_string(), args.key.clone()];
        if let Some(f) = &args.file {
            argv.push(f.clone());
        }
        self.exec_self(argv, &args.cwd).await
    }

    #[tool(
        description = "Show keys present in one .env file but missing in the other",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn tooler_env_diff(
        &self,
        Parameters(args): Parameters<EnvDiffArgs>,
    ) -> Result<CallToolResult, McpError> {
        let argv = vec![
            "env".to_string(),
            "diff".to_string(),
            args.file_a.clone(),
            args.file_b.clone(),
        ];
        self.exec_self(argv, &args.cwd).await
    }

    #[tool(
        description = "Verify a .env file has all keys from a reference file",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn tooler_env_check(
        &self,
        Parameters(args): Parameters<EnvCheckArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec![
            "env".to_string(),
            "check".to_string(),
            args.reference.clone(),
        ];
        if let Some(t) = &args.target {
            argv.push(t.clone());
        }
        self.exec_self(argv, &args.cwd).await
    }

    #[tool(
        description = "Perform an HTTP GET request, with optional profile-based auth. Never \
                        accepts a bearer token as a tool argument -- set TOOLER_HTTP_TOKEN in \
                        the MCP server's own environment for ad hoc auth, or use --profile for \
                        a token stored in the OS keychain (only sent to that profile's host).",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = true)
    )]
    async fn tooler_http_get(
        &self,
        Parameters(args): Parameters<HttpGetArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec!["http".to_string(), "get".to_string(), args.url.clone()];
        push_repeated(&mut argv, "--header", &args.headers);
        push_opt_num(&mut argv, "--timeout", args.timeout);
        push_opt(&mut argv, "--profile", &args.profile);
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Perform an HTTP POST request with a JSON body. Never accepts a bearer \
                        token as a tool argument -- set TOOLER_HTTP_TOKEN in the MCP server's \
                        own environment for ad hoc auth, or use --profile for a token stored in \
                        the OS keychain (only sent to that profile's host).",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn tooler_http_post(
        &self,
        Parameters(args): Parameters<HttpPostArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec!["http".to_string(), "post".to_string(), args.url.clone()];
        push_opt(&mut argv, "--body", &args.body);
        push_repeated(&mut argv, "--header", &args.headers);
        push_opt_num(&mut argv, "--timeout", args.timeout);
        push_opt(&mut argv, "--profile", &args.profile);
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Check whether a URL returns a 2xx response",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = true)
    )]
    async fn tooler_check_url(
        &self,
        Parameters(args): Parameters<CheckUrlArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec!["check".to_string(), "url".to_string(), args.url.clone()];
        push_opt_num(&mut argv, "--timeout", args.timeout);
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Check whether a TCP port is open on a host",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = true)
    )]
    async fn tooler_check_port(
        &self,
        Parameters(args): Parameters<CheckPortArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec![
            "check".to_string(),
            "port".to_string(),
            args.host.clone(),
            args.port.to_string(),
        ];
        push_opt_num(&mut argv, "--timeout", args.timeout);
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Show tooler's full configuration",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn tooler_config_show(&self) -> Result<CallToolResult, McpError> {
        self.exec_self(vec!["config".to_string(), "show".to_string()], &None)
            .await
    }

    #[tool(
        description = "Get a tooler config value by key, e.g. default.output",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn tooler_config_get(
        &self,
        Parameters(args): Parameters<ConfigGetArgs>,
    ) -> Result<CallToolResult, McpError> {
        if is_profile_token_key(&args.key) {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "Refusing to read a profile token over MCP -- it would end up in plaintext in \
                 the conversation. Run `tooler config get \"..\"` directly in a terminal instead.",
            )]));
        }
        let argv = vec!["config".to_string(), "get".to_string(), args.key.clone()];
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Set a tooler config value by key, e.g. default.output json. Refuses \
                        profile.<name>.token (set that directly in a terminal instead, so the \
                        secret never enters the conversation).",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn tooler_config_set(
        &self,
        Parameters(args): Parameters<ConfigSetArgs>,
    ) -> Result<CallToolResult, McpError> {
        if is_profile_token_key(&args.key) {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "Refusing to set a profile token over MCP -- it would sit in plaintext in the \
                 conversation/tool-call history. Run `tooler config set profile.<name>.token ..` \
                 directly in a terminal instead; it's stored encrypted in the OS keychain.",
            )]));
        }
        let argv = vec![
            "config".to_string(),
            "set".to_string(),
            args.key.clone(),
            args.value.clone(),
        ];
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "List configured tooler profiles",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn tooler_config_profiles(&self) -> Result<CallToolResult, McpError> {
        self.exec_self(vec!["config".to_string(), "profiles".to_string()], &None)
            .await
    }

    #[tool(
        description = "Print tooler's config file path",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn tooler_config_path(&self) -> Result<CallToolResult, McpError> {
        self.exec_self(vec!["config".to_string(), "path".to_string()], &None)
            .await
    }

    #[tool(
        description = "Unset a tooler config value by key, e.g. profile.staging.token \
                        (safe to use over MCP -- it only removes the stored value)",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn tooler_config_unset(
        &self,
        Parameters(args): Parameters<ConfigUnsetArgs>,
    ) -> Result<CallToolResult, McpError> {
        let argv = vec!["config".to_string(), "unset".to_string(), args.key.clone()];
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Run a named script defined in .tooler.toml",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn tooler_run(
        &self,
        Parameters(args): Parameters<RunMcpArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec!["run".to_string()];
        if let Some(script) = &args.script {
            argv.push(script.clone());
        }
        push_flag(&mut argv, "--dry", args.dry);
        if !args.extra.is_empty() {
            argv.push("--".to_string());
            argv.extend(args.extra.clone());
        }
        self.exec_self(argv, &args.cwd).await
    }

    #[tool(
        description = "Run a YAML playbook (tasks, vars, health checks) or generate a sample",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn tooler_play(
        &self,
        Parameters(args): Parameters<PlayMcpArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec!["play".to_string()];
        if let Some(file) = &args.file {
            argv.push(file.clone());
        }
        push_flag(&mut argv, "--dry", args.dry);
        push_repeated(&mut argv, "--var", &args.vars);
        push_opt(&mut argv, "--tags", &args.tags);
        push_flag(&mut argv, "--init", args.init);
        self.exec_self(argv, &args.cwd).await
    }

    #[tool(
        description = "Compact git repo summary: branch, tag, status, recent commits",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn tooler_git_summary(
        &self,
        Parameters(args): Parameters<GitCwdArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.exec_self(vec!["git".to_string(), "summary".to_string()], &args.cwd)
            .await
    }

    #[tool(
        description = "Delete branches already merged into the current branch, or (with \
                        after/before) any local branch with a trailing DDMMYY date suffix \
                        in the given range regardless of merge status",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn tooler_git_clean(
        &self,
        Parameters(args): Parameters<GitCleanArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec!["git".to_string(), "clean".to_string()];
        push_flag(&mut argv, "--remote", args.remote);
        push_flag(&mut argv, "--confirm", args.confirm);
        push_opt(&mut argv, "--after", &args.after);
        push_opt(&mut argv, "--before", &args.before);
        self.exec_self(argv, &args.cwd).await
    }

    #[tool(
        description = "Generate a changelog from commits since the last tag",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn tooler_git_changelog(
        &self,
        Parameters(args): Parameters<GitChangelogArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec!["git".to_string(), "changelog".to_string()];
        push_opt(&mut argv, "--from", &args.from);
        self.exec_self(argv, &args.cwd).await
    }

    #[tool(
        description = "List available scaffold templates",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn tooler_scaffold_list(&self) -> Result<CallToolResult, McpError> {
        self.exec_self(vec!["scaffold".to_string(), "list".to_string()], &None)
            .await
    }

    #[tool(
        description = "Create a new project from a scaffold template",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn tooler_scaffold_new(
        &self,
        Parameters(args): Parameters<ScaffoldNewArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec![
            "scaffold".to_string(),
            "new".to_string(),
            args.template.clone(),
            args.name.clone(),
        ];
        push_opt(&mut argv, "--dir", &args.dir);
        self.exec_self(argv, &args.cwd).await
    }

    #[tool(
        description = "Generate a multi-page PDF report (cover page, per-source sections, \
                        tables, and embedded bar charts) from one or more JSON files, typically \
                        the --output json result of another tooler command",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn tooler_report_pdf(
        &self,
        Parameters(args): Parameters<ReportArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec!["report".to_string(), "pdf".to_string()];
        push_repeated(&mut argv, "--in", &args.input);
        argv.push("--out".to_string());
        argv.push(args.out.clone());
        if let Some(title) = &args.title {
            argv.push("--title".to_string());
            argv.push(title.clone());
        }
        self.exec_self(argv, &args.cwd).await
    }

    #[tool(
        description = "Generate a multi-sheet Excel (.xlsx) report (one sheet per source, with \
                        formatted tables and native charts) from one or more JSON files, \
                        typically the --output json result of another tooler command",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn tooler_report_excel(
        &self,
        Parameters(args): Parameters<ReportArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec!["report".to_string(), "excel".to_string()];
        push_repeated(&mut argv, "--in", &args.input);
        argv.push("--out".to_string());
        argv.push(args.out.clone());
        if let Some(title) = &args.title {
            argv.push("--title".to_string());
            argv.push(title.clone());
        }
        self.exec_self(argv, &args.cwd).await
    }

    #[tool(
        description = "Run a read-only SQL query (SELECT/SHOW/EXPLAIN/WITH/DESCRIBE) against a \
                        remote database by running psql/mysql directly on a server profile over \
                        SSH, returning rows as JSON — feed the result straight into \
                        tooler_report_pdf/excel. \
                        Prefer `env` (a remote dotenv-style file, e.g. Laravel .env) to supply \
                        DB_* credentials rather than passing them explicitly; a DB password can \
                        never be passed as a tool argument — set TOOLER_DB_PASSWORD in the \
                        environment the tooler MCP server itself runs in instead.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn tooler_db_query(
        &self,
        Parameters(args): Parameters<DbQueryArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec![
            "db".to_string(),
            "query".to_string(),
            args.server.clone(),
            args.sql.clone(),
        ];
        push_opt(&mut argv, "--env", &args.env);
        push_opt(&mut argv, "--engine", &args.engine);
        push_opt(&mut argv, "--host", &args.host);
        push_opt_num(&mut argv, "--port", args.port);
        push_opt(&mut argv, "--database", &args.database);
        push_opt(&mut argv, "--user", &args.user);
        push_opt_num(&mut argv, "--max-rows", args.max_rows);
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Dump a remote database (pg_dump/mysqldump) over SSH to a local file, \
                        gzip-compressed by default. Prefer `env` (a remote dotenv-style file) \
                        to supply DB_* credentials rather than passing them explicitly; a DB \
                        password can never be passed as a tool argument — set \
                        TOOLER_DB_PASSWORD in the environment the tooler MCP server itself \
                        runs in instead.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn tooler_db_backup(
        &self,
        Parameters(args): Parameters<DbBackupArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec![
            "db".to_string(),
            "backup".to_string(),
            args.server.clone(),
            "--out".to_string(),
            args.out.clone(),
        ];
        push_opt(&mut argv, "--env", &args.env);
        push_opt(&mut argv, "--engine", &args.engine);
        push_opt(&mut argv, "--host", &args.host);
        push_opt_num(&mut argv, "--port", args.port);
        push_opt(&mut argv, "--database", &args.database);
        push_opt(&mut argv, "--user", &args.user);
        push_flag(&mut argv, "--no-gzip", args.no_gzip);
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Restore a local dump file into a remote database (psql/mysql) over SSH. \
                        Without `confirm`, this only previews what would run (byte count, \
                        target database) and makes no change — pass `confirm: true` to actually \
                        apply it. Prefer `env` for credentials; a DB password can never be \
                        passed as a tool argument — set TOOLER_DB_PASSWORD in the MCP server's \
                        own environment instead.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn tooler_db_restore(
        &self,
        Parameters(args): Parameters<DbRestoreArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec![
            "db".to_string(),
            "restore".to_string(),
            args.server.clone(),
            "--in".to_string(),
            args.input.clone(),
        ];
        push_opt(&mut argv, "--env", &args.env);
        push_opt(&mut argv, "--engine", &args.engine);
        push_opt(&mut argv, "--host", &args.host);
        push_opt_num(&mut argv, "--port", args.port);
        push_opt(&mut argv, "--database", &args.database);
        push_opt(&mut argv, "--user", &args.user);
        push_flag(&mut argv, "--confirm", args.confirm);
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "List pull requests (title, labels, author, dates) via the `gh` CLI, \
                        optionally filtered to a created-date range. Requires `gh` installed \
                        and authenticated in the environment the tooler MCP server runs in. \
                        Feed the JSON result straight into tooler_report_pdf/excel.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = true)
    )]
    async fn tooler_gh_prs(
        &self,
        Parameters(args): Parameters<GhPrsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec!["gh".to_string(), "prs".to_string()];
        push_opt(&mut argv, "--repo", &args.repo);
        push_opt(&mut argv, "--after", &args.after);
        push_opt(&mut argv, "--before", &args.before);
        push_opt(&mut argv, "--state", &args.state);
        push_opt_num(&mut argv, "--limit", args.limit);
        self.exec_self(argv, &args.cwd).await
    }

    #[tool(
        description = "Show a systemd unit's status on a remote server over SSH \
                        (systemctl status). `active` in the result reflects the exit code \
                        (0 = active); a non-zero exit (e.g. a stopped or unknown unit) is \
                        returned as informative output, not a tool error.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = true)
    )]
    async fn tooler_systemd_status(
        &self,
        Parameters(args): Parameters<SystemdUnitArgs>,
    ) -> Result<CallToolResult, McpError> {
        let argv = vec![
            "systemd".to_string(),
            "status".to_string(),
            args.server.clone(),
            args.unit.clone(),
        ];
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Restart a systemd unit on a remote server over SSH. A sudo password, if \
                        needed, must never be passed as a tool argument -- set TOOLER_SUDO_PASS \
                        in the MCP server's own environment instead (or rely on \
                        passwordless/NOPASSWD sudo).",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn tooler_systemd_restart(
        &self,
        Parameters(args): Parameters<SystemdRestartArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec![
            "systemd".to_string(),
            "restart".to_string(),
            args.server.clone(),
            args.unit.clone(),
        ];
        push_flag(&mut argv, "--sudo", args.sudo);
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Show recent journal entries for a systemd unit on a remote server \
                        (journalctl -u)",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = true)
    )]
    async fn tooler_systemd_logs(
        &self,
        Parameters(args): Parameters<SystemdLogsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec![
            "systemd".to_string(),
            "logs".to_string(),
            args.server.clone(),
            args.unit.clone(),
        ];
        push_opt_num(&mut argv, "--lines", args.lines);
        push_flag(&mut argv, "--sudo", args.sudo);
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "List a remote server's crontab entries (crontab -l)",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = true)
    )]
    async fn tooler_cron_list(
        &self,
        Parameters(args): Parameters<CronServerArgs>,
    ) -> Result<CallToolResult, McpError> {
        let argv = vec!["cron".to_string(), "list".to_string(), args.server.clone()];
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Append a line to a remote server's crontab",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn tooler_cron_add(
        &self,
        Parameters(args): Parameters<CronAddArgs>,
    ) -> Result<CallToolResult, McpError> {
        let argv = vec![
            "cron".to_string(),
            "add".to_string(),
            args.server.clone(),
            args.line.clone(),
        ];
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Remove crontab lines containing a fixed substring, on a remote server",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn tooler_cron_remove(
        &self,
        Parameters(args): Parameters<CronRemoveArgs>,
    ) -> Result<CallToolResult, McpError> {
        let argv = vec![
            "cron".to_string(),
            "remove".to_string(),
            args.server.clone(),
            args.pattern.clone(),
        ];
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Show the last N lines of a remote file over SSH (tail -n)",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = true)
    )]
    async fn tooler_logs_tail(
        &self,
        Parameters(args): Parameters<LogsTailArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec![
            "logs".to_string(),
            "tail".to_string(),
            args.server.clone(),
            args.path.clone(),
        ];
        push_opt_num(&mut argv, "--lines", args.lines);
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Search a remote file over SSH for a fixed substring (grep -F)",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = true)
    )]
    async fn tooler_logs_grep(
        &self,
        Parameters(args): Parameters<LogsGrepArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec![
            "logs".to_string(),
            "grep".to_string(),
            args.server.clone(),
            args.path.clone(),
            args.pattern.clone(),
        ];
        push_opt_num(&mut argv, "--max-lines", args.max_lines);
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "List running processes on a remote server over SSH (ps aux), optionally \
                        filtered by a substring of the command line or an exact PID",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = true)
    )]
    async fn tooler_ps_list(
        &self,
        Parameters(args): Parameters<PsListArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec!["ps".to_string(), "list".to_string(), args.server.clone()];
        push_opt(&mut argv, "--filter", &args.filter);
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Send a signal to a process on a remote server over SSH (default: TERM). \
                        Without `confirm`, this only previews what would happen and sends \
                        nothing — pass `confirm: true` to actually apply it. A sudo password, \
                        if needed, must never be passed as a tool argument -- set \
                        TOOLER_SUDO_PASS in the MCP server's own environment instead.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn tooler_ps_kill(
        &self,
        Parameters(args): Parameters<PsKillArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec![
            "ps".to_string(),
            "kill".to_string(),
            args.server.clone(),
            args.pid.to_string(),
        ];
        push_opt(&mut argv, "--signal", &args.signal);
        push_flag(&mut argv, "--sudo", args.sudo);
        push_flag(&mut argv, "--confirm", args.confirm);
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Print a remote file's contents over SSH",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = true)
    )]
    async fn tooler_fs_cat(
        &self,
        Parameters(args): Parameters<FsCatArgs>,
    ) -> Result<CallToolResult, McpError> {
        let argv = vec![
            "fs".to_string(),
            "cat".to_string(),
            args.server.clone(),
            args.path.clone(),
        ];
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Overwrite a remote file over SSH with local content, from either \
                        `from_file` (a local path) or `content` (literal text) -- exactly one \
                        must be set. Without `confirm`, this only previews what would happen \
                        (byte count) and makes no change — pass `confirm: true` to actually \
                        apply it.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn tooler_fs_write(
        &self,
        Parameters(args): Parameters<FsWriteArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec![
            "fs".to_string(),
            "write".to_string(),
            args.server.clone(),
            args.path.clone(),
        ];
        push_opt(&mut argv, "--from-file", &args.from_file);
        push_opt(&mut argv, "--content", &args.content);
        push_flag(&mut argv, "--confirm", args.confirm);
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Diff a remote file against a local file over SSH (unified diff)",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = true)
    )]
    async fn tooler_fs_diff(
        &self,
        Parameters(args): Parameters<FsDiffArgs>,
    ) -> Result<CallToolResult, McpError> {
        let argv = vec![
            "fs".to_string(),
            "diff".to_string(),
            args.server.clone(),
            args.path.clone(),
            "--local".to_string(),
            args.local.clone(),
        ];
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Orchestrate a remote deploy over SSH: optional git pull, optional build \
                        command, optional restart command, then an optional HTTP health check \
                        -- run in that order, failing fast on the first error. Without `confirm`, \
                        this only previews the steps that would run and makes no change — pass \
                        `confirm: true` to actually apply it. A sudo password, if needed, must \
                        never be passed as a tool argument -- set TOOLER_SUDO_PASS in the MCP \
                        server's own environment instead.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn tooler_deploy_run(
        &self,
        Parameters(args): Parameters<DeployRunArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec![
            "deploy".to_string(),
            args.server.clone(),
            "--path".to_string(),
            args.path.clone(),
        ];
        push_flag(&mut argv, "--pull", args.pull);
        push_opt(&mut argv, "--build", &args.build);
        push_opt(&mut argv, "--restart", &args.restart);
        push_opt(&mut argv, "--health-url", &args.health_url);
        push_opt_num(&mut argv, "--health-timeout", args.health_timeout);
        push_opt_num(&mut argv, "--health-retries", args.health_retries);
        push_opt_num(&mut argv, "--health-delay", args.health_delay);
        push_flag(&mut argv, "--sudo", args.sudo);
        push_flag(&mut argv, "--confirm", args.confirm);
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Run a command on multiple servers over SSH at once (pass `servers` as a \
                        comma-separated list of profile names, or `all: true` for every \
                        configured profile). Continues past a failing server and reports \
                        per-server results rather than aborting the whole batch.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn tooler_fleet_exec(
        &self,
        Parameters(args): Parameters<FleetExecArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec!["fleet".to_string(), "exec".to_string()];
        push_opt(&mut argv, "--servers", &args.servers);
        push_flag(&mut argv, "--all", args.all);
        argv.push(args.command.clone());
        push_flag(&mut argv, "--sudo", args.sudo);
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Check SSH connectivity to multiple servers at once (pass `servers` as a \
                        comma-separated list of profile names, or `all: true` for every \
                        configured profile)",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = true)
    )]
    async fn tooler_fleet_check(
        &self,
        Parameters(args): Parameters<FleetCheckArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec!["fleet".to_string(), "check".to_string()];
        push_opt(&mut argv, "--servers", &args.servers);
        push_flag(&mut argv, "--all", args.all);
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Get a resource snapshot (uptime/load average, memory, disk usage) for a \
                        remote server over SSH",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = true)
    )]
    async fn tooler_stat(
        &self,
        Parameters(args): Parameters<StatArgs>,
    ) -> Result<CallToolResult, McpError> {
        let argv = vec!["stat".to_string(), args.server.clone()];
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "List configured server profiles (host, user, SSH key)",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn tooler_server_list(&self) -> Result<CallToolResult, McpError> {
        self.exec_self(vec!["server".to_string(), "list".to_string()], &None)
            .await
    }

    #[tool(
        description = "Add or update a server profile",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn tooler_server_add(
        &self,
        Parameters(args): Parameters<ServerAddArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec![
            "server".to_string(),
            "add".to_string(),
            args.name.clone(),
            "--host".to_string(),
            args.host.clone(),
        ];
        push_opt(&mut argv, "--user", &args.user);
        push_opt_num(&mut argv, "--port", args.port);
        push_opt(&mut argv, "--key", &args.key);
        push_opt(&mut argv, "--ssl-dir", &args.ssl_dir);
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Show details of a server profile",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn tooler_server_show(
        &self,
        Parameters(args): Parameters<ServerNameArgs>,
    ) -> Result<CallToolResult, McpError> {
        let argv = vec!["server".to_string(), "show".to_string(), args.name.clone()];
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Remove a server profile",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn tooler_server_remove(
        &self,
        Parameters(args): Parameters<ServerNameArgs>,
    ) -> Result<CallToolResult, McpError> {
        let argv = vec![
            "server".to_string(),
            "remove".to_string(),
            args.name.clone(),
        ];
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Test SSH connectivity to a configured server",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = true)
    )]
    async fn tooler_ssh_check(
        &self,
        Parameters(args): Parameters<SshCheckArgs>,
    ) -> Result<CallToolResult, McpError> {
        let argv = vec!["ssh".to_string(), "check".to_string(), args.server.clone()];
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Execute a command on a remote server over SSH",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn tooler_ssh_exec(
        &self,
        Parameters(args): Parameters<SshExecArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec![
            "ssh".to_string(),
            "exec".to_string(),
            args.server.clone(),
            args.command.clone(),
        ];
        push_flag(&mut argv, "--sudo", args.sudo);
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Upload a local file to a remote server via scp",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn tooler_ssh_copy(
        &self,
        Parameters(args): Parameters<SshCopyArgs>,
    ) -> Result<CallToolResult, McpError> {
        let argv = vec![
            "ssh".to_string(),
            "copy".to_string(),
            args.local.clone(),
            args.remote.clone(),
        ];
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Deploy SSL certificates to a server and reload nginx. PFX/sudo passwords \
                        are never passed as tool arguments -- set TOOLER_PFX_PASS / \
                        TOOLER_SUDO_PASS in the MCP server's own environment (e.g. in .mcp.json's \
                        \"env\" block) and they'll be picked up automatically.",
        annotations(
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn tooler_ssh_ssl(
        &self,
        Parameters(args): Parameters<SshSslArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec![
            "ssh".to_string(),
            "ssl".to_string(),
            args.server.clone(),
            "--pfx".to_string(),
            args.pfx.clone(),
            "--key".to_string(),
            args.key.clone(),
        ];
        push_opt(&mut argv, "--remote-dir", &args.remote_dir);
        push_opt(&mut argv, "--cert-name", &args.cert_name);
        push_opt(&mut argv, "--key-name", &args.key_name);
        self.exec_self(argv, &None).await
    }
}

#[tool_handler(router = self.tool_router)]
#[prompt_handler(router = self.prompt_router)]
impl ServerHandler for ToolerMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .enable_prompts()
                .build(),
        )
        .with_server_info(Implementation::new("tooler", env!("CARGO_PKG_VERSION")))
        .with_instructions(
            "Tooler: a devops CLI toolkit. Tools mirror the `tooler` subcommands 1:1 \
             (env, http, check, git, ssh, server profiles, run/play automation). \
             Most tools accept an optional `cwd` to target a specific project directory.",
        )
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        Ok(ListResourcesResult::with_all_items(vec![
            Resource::new("tooler://config/profiles", "config_profiles")
                .with_description("Configured tooler profiles (same as tooler_config_profiles)")
                .with_mime_type("application/json"),
            Resource::new("tooler://config/servers", "config_servers")
                .with_description("Configured server profiles (same as tooler_server_list)")
                .with_mime_type("application/json"),
            Resource::new("tooler://config/show", "config_show")
                .with_description("Full tooler configuration (same as tooler_config_show)")
                .with_mime_type("application/json"),
        ]))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, McpError> {
        let argv = match request.uri.as_str() {
            "tooler://config/profiles" => vec!["config".to_string(), "profiles".to_string()],
            "tooler://config/servers" => vec!["server".to_string(), "list".to_string()],
            "tooler://config/show" => vec!["config".to_string(), "show".to_string()],
            other => {
                return Err(McpError::resource_not_found(
                    format!("no such resource: {other}"),
                    None,
                ));
            }
        };
        let result = self.exec_self(argv, &None).await?;
        let text = result
            .content
            .into_iter()
            .find_map(|c| match c {
                ContentBlock::Text(t) => Some(t.text),
                _ => None,
            })
            .unwrap_or_default();
        if result.is_error == Some(true) {
            return Err(McpError::internal_error(text, None));
        }
        Ok(ReadResourceResult::new(vec![
            ResourceContents::text(text, request.uri).with_mime_type("application/json"),
        ]))
    }
}

/// Guards against the CLI and the MCP tool surface drifting apart: every top-level
/// `tooler` command should have at least one `tooler_<command>[_*]` MCP tool, unless
/// explicitly exempted below.
#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn every_cli_command_has_a_matching_mcp_tool() {
        // Commands with no MCP tool on purpose: `mcp` is the server itself, and
        // `completions` (shell completion scripts) has no meaningful use from an LLM caller.
        let exempt = ["mcp", "completions"];

        let cli = crate::cli::Cli::command();
        let tool_names: Vec<String> = ToolerMcp::new()
            .tool_router
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();

        for sub in cli.get_subcommands() {
            let name = sub.get_name();
            if exempt.contains(&name) {
                continue;
            }
            let prefix = format!("tooler_{name}");
            let has_match = tool_names
                .iter()
                .any(|t| *t == prefix || t.starts_with(&format!("{prefix}_")));
            assert!(
                has_match,
                "CLI command `{name}` has no matching MCP tool (expected `{prefix}` or `{prefix}_*`) \
                 -- add one in src/commands/mcp.rs, or add `{name}` to the `exempt` list above"
            );
        }
    }
}
