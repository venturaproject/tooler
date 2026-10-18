use crate::{
    commands::{echo::EchoArgs, info::InfoArgs},
    config::McpPolicy,
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
use std::path::PathBuf;

const EXEC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
const MAX_EXEC_OUTPUT_BYTES: usize = 256 * 1024;

async fn read_limited<R: tokio::io::AsyncRead + Unpin>(mut pipe: R) -> std::io::Result<String> {
    use tokio::io::AsyncReadExt;
    let mut output = Vec::new();
    let mut buffer = [0u8; 8192];
    let mut truncated = false;
    loop {
        let read = pipe.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let remaining = MAX_EXEC_OUTPUT_BYTES.saturating_sub(output.len());
        let kept = remaining.min(read);
        output.extend_from_slice(&buffer[..kept]);
        truncated |= kept < read;
    }
    let mut output = String::from_utf8_lossy(&output).into_owned();
    if truncated {
        output.push_str("\n[output truncated]");
    }
    Ok(output)
}

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
    /// Print the playbook-backed tools (`tooler_pb_*`, from `./playbooks/*.yml` declaring
    /// `mcp_tool:`) this server would expose, then exit without starting it.
    #[arg(long)]
    pub list_playbook_tools: bool,
}

pub fn run(args: McpArgs, _ctx: &Context) -> Result<()> {
    if args.list_playbook_tools {
        let tools = discover_playbook_tools();
        let list: Vec<_> = tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "name": t.tool.name,
                    "description": t.tool.description,
                    "file": t.file.display().to_string(),
                    "input_schema": &*t.tool.input_schema,
                })
            })
            .collect();
        println!("{}", serde_json::json!({ "playbook_tools": list }));
        return Ok(());
    }

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

fn push_repeated(argv: &mut Vec<String>, flag: &str, values: &[String]) {
    for v in values {
        argv.push(flag.to_string());
        argv.push(v.clone());
    }
}

/// Extracts explicit server operands from command wrappers that target one server.
fn server_arguments(argv: &[String]) -> Vec<&str> {
    let Some(command) = argv.first().map(String::as_str) else {
        return Vec::new();
    };
    let positional_server = match command {
        "deploy" | "stat" => argv.get(1),
        "db" => argv.get(2),
        "ssh" | "systemd" | "ps" | "logs" | "fs" | "cron" => argv.get(2),
        _ => None,
    };
    let mut servers: Vec<&str> = positional_server.into_iter().map(String::as_str).collect();
    // `ssh copy <local> <server>:<path>` has no standalone server argument.
    if command == "ssh" && argv.get(1).is_some_and(|subcommand| subcommand == "copy") {
        servers.clear();
        if let Some(destination) = argv.get(3)
            && let Some((server, _)) = destination.split_once(':')
        {
            servers.push(server);
        }
    }
    for pair in argv.windows(2) {
        // `mail --server` selects an SMTP profile, not an SSH server profile.
        if command != "mail" && pair[0] == "--server" {
            servers.push(&pair[1]);
        }
        if pair[0] == "--servers" {
            servers.extend(pair[1].split(',').map(str::trim).filter(|s| !s.is_empty()));
        }
    }
    servers
}

/// Defense-in-depth redaction before an argv is written to `--audit-log`: the tool
/// guards (`tooler_config_set`/`tooler_config_get` refuse keychain-backed keys outright,
/// see `commands::config::is_secret_backed_key`) already stop the intended secret paths.
/// Audit logs must make the safer tradeoff: redact all variable values and payload-like
/// arguments rather than trying to infer whether arbitrary command or file content is safe.
fn redact_argv(argv: &[String]) -> Vec<String> {
    let mut out: Vec<String> = argv.to_vec();
    if let [cmd, sub, key, _value] = out.as_slice()
        && cmd == "config"
        && sub == "set"
        && crate::commands::config::is_secret_backed_key(key)
    {
        out[3] = "***".to_string();
        return out;
    }
    for i in 0..out.len().saturating_sub(1) {
        if matches!(out[i].as_str(), "--var" | "-e") {
            let name = out[i + 1]
                .split_once('=')
                .map(|(name, _)| name)
                .unwrap_or("var");
            out[i + 1] = format!("{name}=***");
        }
        if matches!(
            out[i].as_str(),
            "--header" | "--body" | "--content" | "--query" | "--build" | "--restart"
        ) {
            out[i + 1] = "***".to_string();
        }
    }
    match out.first().map(String::as_str) {
        Some("db") if out.len() > 3 => out[3] = "***".to_string(),
        Some("ssh")
            if out.get(1).is_some_and(|subcommand| subcommand == "exec") && out.len() > 3 =>
        {
            out[3] = "***".to_string()
        }
        Some("fleet") if out.get(1).is_some_and(|subcommand| subcommand == "exec") => {
            let mut index = 2;
            while index < out.len() {
                match out[index].as_str() {
                    "--servers" | "--group" => index += 2,
                    "--all" | "--parallel" | "--sudo" => index += 1,
                    _ => {
                        out[index] = "***".to_string();
                        break;
                    }
                }
            }
        }
        Some("run") => {
            if let Some(separator) = out.iter().position(|arg| arg == "--") {
                for argument in &mut out[separator + 1..] {
                    *argument = "***".to_string();
                }
            }
        }
        Some("mail") if out.get(1).is_some_and(|subcommand| subcommand == "send") => {
            // The send command's positional body follows its recipient fields.
            for arg in out.iter_mut().skip(2) {
                if !arg.starts_with('-') && arg.contains('\n') {
                    *arg = "***".to_string();
                }
            }
        }
        Some("cron")
            if matches!(out.get(1).map(String::as_str), Some("add") | Some("remove"))
                && out.len() > 3 =>
        {
            out[3] = "***".to_string()
        }
        _ => {}
    }
    out
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

        if let Some(message) = self.policy_error(&argv) {
            self.write_audit(&logged_argv, cwd, false, 0);
            return Err(McpError::invalid_params(message, None));
        }

        let exe = std::env::current_exe().map_err(|e| {
            McpError::internal_error(format!("cannot resolve tooler binary: {e}"), None)
        })?;

        // Global flags must precede `--`, otherwise clap forwards them to `tooler run`'s
        // script arguments rather than applying the JSON MCP response contract.
        let separator = argv
            .iter()
            .position(|arg| arg == "--")
            .unwrap_or(argv.len());
        argv.splice(
            separator..separator,
            ["--output".to_string(), "json".to_string()],
        );

        let mut cmd = tokio::process::Command::new(exe);
        cmd.args(&argv).env("NO_COLOR", "1");
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }

        use std::process::Stdio;
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .map_err(|e| McpError::internal_error(format!("failed to launch tooler: {e}"), None))?;
        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");
        let stdout_task = tokio::spawn(read_limited(stdout));
        let stderr_task = tokio::spawn(read_limited(stderr));
        let status = match tokio::time::timeout(EXEC_TIMEOUT, child.wait()).await {
            Ok(status) => status.map_err(|e| {
                McpError::internal_error(format!("failed to wait for tooler: {e}"), None)
            })?,
            Err(_) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                self.write_audit(&logged_argv, cwd, false, started.elapsed().as_millis());
                return Err(McpError::internal_error(
                    "tooler execution timed out after 60 seconds",
                    None,
                ));
            }
        };
        let output_text = |task: tokio::task::JoinHandle<Result<String, std::io::Error>>| async move {
            task.await
                .map_err(|e| {
                    McpError::internal_error(format!("failed to collect tooler output: {e}"), None)
                })?
                .map_err(|e| {
                    McpError::internal_error(format!("failed to read tooler output: {e}"), None)
                })
        };
        let stdout = output_text(stdout_task).await?;
        let stderr = output_text(stderr_task).await?;

        let result = if status.success() {
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
                message = format!("tooler exited with status {status}");
            }
            CallToolResult::error(vec![ContentBlock::text(message)])
        };

        self.write_audit(
            &logged_argv,
            cwd,
            status.success(),
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
        let argv = redact_argv(argv);
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

mod args;
#[cfg(test)]
mod tests;
mod tools_admin;
mod tools_core;
mod tools_infra;
mod tools_ops;
mod tools_play;
mod tools_task;

pub(crate) use args::*;

// ── server ────────────────────────────────────────────────────────────────

/// A playbook exposed as its own MCP tool (via `mcp_tool:` in the YAML) — discovered
/// once at startup from `./playbooks/`. See `commands::play::playbook_tool_defs`.
#[derive(Clone)]
struct PlaybookTool {
    tool: rmcp::model::Tool,
    file: PathBuf,
}

#[derive(Clone)]
pub struct ToolerMcp {
    tool_router: ToolRouter<ToolerMcp>,
    prompt_router: PromptRouter<ToolerMcp>,
    audit_log: Option<std::path::PathBuf>,
    policy: McpPolicy,
    policy_load_error: Option<String>,
    playbook_tools: Vec<PlaybookTool>,
}

/// Scans `./playbooks/` (relative to the process's launch cwd) for playbooks declaring
/// `mcp_tool:` and turns each into a `PlaybookTool`. Fixed for the life of the server.
fn discover_playbook_tools() -> Vec<PlaybookTool> {
    let dir = std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("playbooks");
    crate::commands::play::playbook_tool_defs(&dir)
        .into_iter()
        .map(|def| PlaybookTool {
            tool: rmcp::model::Tool::new(
                def.name,
                def.description,
                std::sync::Arc::new(def.input_schema.as_object().cloned().unwrap_or_default()),
            ),
            file: def.file,
        })
        .collect()
}

fn validate_playbook_arguments(
    schema: &serde_json::Map<String, serde_json::Value>,
    arguments: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), String> {
    let properties = schema
        .get("properties")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| "playbook tool has an invalid input schema".to_string())?;
    for name in arguments.keys() {
        if !properties.contains_key(name) {
            return Err(format!("unknown playbook argument '{name}'"));
        }
    }
    for required in schema
        .get("required")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
    {
        if !arguments.contains_key(required) {
            return Err(format!("missing required playbook argument '{required}'"));
        }
    }
    for (name, value) in arguments {
        let property = &properties[name];
        let type_matches = match property.get("type").and_then(serde_json::Value::as_str) {
            Some("string") => value.is_string(),
            Some("number") => value.is_number(),
            Some("boolean") => value.is_boolean(),
            _ => false,
        };
        if !type_matches {
            return Err(format!("playbook argument '{name}' has the wrong type"));
        }
        if let Some(values) = property.get("enum").and_then(serde_json::Value::as_array)
            && !values.contains(value)
        {
            return Err(format!(
                "playbook argument '{name}' is not an allowed value"
            ));
        }
    }
    Ok(())
}

impl ToolerMcp {
    fn policy_error(&self, argv: &[String]) -> Option<String> {
        if let Some(error) = &self.policy_load_error {
            return Some(format!(
                "MCP policy configuration could not be loaded; refusing execution: {error}"
            ));
        }
        let command = argv.first()?;
        // Secret-bearing local files are intentionally a CLI-only capability. An MCP client
        // can still use explicitly allowed operational tools, but it cannot accidentally
        // stream a .env value or decrypted vault directly into an agent conversation.
        if (command == "env" && matches!(argv.get(1).map(String::as_str), Some("get")))
            || (command == "env"
                && argv.get(1).is_some_and(|subcommand| subcommand == "show")
                && argv.iter().any(|argument| argument == "--reveal"))
            || (command == "vault" && argv.get(1).is_some_and(|subcommand| subcommand == "view"))
        {
            return Some(
                "MCP does not expose plaintext local secrets; use the local CLI directly"
                    .to_string(),
            );
        }
        if !self.policy.allowed_commands.is_empty()
            && !self
                .policy
                .allowed_commands
                .iter()
                .any(|allowed| allowed == command)
        {
            return Some(format!(
                "MCP policy denies the '{command}' command; add it to mcp.allowed_commands to permit it"
            ));
        }
        if self.policy.allowed_servers.is_empty() {
            return None;
        }
        if command == "server"
            || (command == "config"
                && matches!(argv.get(1).map(String::as_str), Some("set") | Some("unset"))
                && argv.get(2).is_some_and(|key| key.starts_with("server.")))
        {
            return Some(
                "MCP policy denies server-profile mutation while mcp.allowed_servers is configured"
                    .to_string(),
            );
        }
        // Playbook task targets may be templated, included, or supplied by dynamic loops.
        // Do not claim an allowlist protects execution that cannot be fully authorized here.
        if command == "play" {
            return Some(
                "MCP policy denies playbooks while mcp.allowed_servers is configured; use explicit allowlisted commands instead".to_string(),
            );
        }
        if command == "fleet"
            && argv.iter().any(|argument| {
                argument == "--all" || argument == "--group" || argument.starts_with("--group=")
            })
        {
            return Some(
                "MCP policy denies fleet --all/--group while mcp.allowed_servers is configured; use --servers with allowlisted names".to_string(),
            );
        }
        for server in server_arguments(argv) {
            if !self
                .policy
                .allowed_servers
                .iter()
                .any(|allowed| allowed == server)
            {
                return Some(format!(
                    "MCP policy denies server '{server}'; add it to mcp.allowed_servers to permit it"
                ));
            }
        }
        None
    }

    pub fn new() -> Self {
        let (policy, policy_load_error) = match crate::config::load() {
            Ok(config) => (config.mcp, None),
            Err(error) => (McpPolicy::default(), Some(error.to_string())),
        };
        Self {
            tool_router: Self::tool_router_core()
                + Self::tool_router_play()
                + Self::tool_router_task()
                + Self::tool_router_ops()
                + Self::tool_router_infra()
                + Self::tool_router_admin(),
            prompt_router: Self::prompt_router(),
            audit_log: None,
            policy,
            policy_load_error,
            playbook_tools: discover_playbook_tools(),
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

#[tool_handler(router = self.tool_router)]
#[prompt_handler(router = self.prompt_router)]
impl ServerHandler for ToolerMcp {
    // Hand-written (the #[tool_handler] macro only generates these when absent) so the
    // static #[tool] router and the dynamically-discovered playbook tools (tooler_pb_*)
    // are merged into one surface.
    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::ListToolsResult, McpError> {
        let mut tools = self.tool_router.list_all();
        tools.extend(self.playbook_tools.iter().map(|p| p.tool.clone()));
        Ok(rmcp::model::ListToolsResult {
            tools,
            meta: None,
            next_cursor: None,
        })
    }

    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        if request.name.starts_with("tooler_pb_")
            && let Some(pb) = self
                .playbook_tools
                .iter()
                .find(|p| p.tool.name == request.name)
        {
            let mut argv = vec!["play".to_string(), pb.file.to_string_lossy().to_string()];
            let args = request.arguments.unwrap_or_default();
            validate_playbook_arguments(&pb.tool.input_schema, &args)
                .map_err(|message| McpError::invalid_params(message, None))?;
            let mut allow_confirm = false;
            for (k, v) in &args {
                if k == "confirm" {
                    allow_confirm = v.as_bool().unwrap_or(false);
                    continue;
                }
                let val = match v {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                argv.push("--var".to_string());
                argv.push(format!("{k}={val}"));
            }
            if allow_confirm {
                argv.push("--yes".to_string());
            }
            return self.exec_self(argv, &None).await;
        }
        let tcc = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
        self.tool_router.call(tcc).await
    }

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
