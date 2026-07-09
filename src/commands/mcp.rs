use crate::{
    commands::{echo::EchoArgs, info::InfoArgs},
    context::Context,
};
use anyhow::Result;
use clap::Args;
use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
    transport::stdio,
};
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Args)]
pub struct McpArgs {}

pub fn run(_args: McpArgs, _ctx: &Context) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let service = ToolerMcp::new().serve(stdio()).await?;
        service.waiting().await?;
        anyhow::Ok(())
    })
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
    key.strip_prefix("profile.")
        .and_then(|rest| rest.split_once('.'))
        .is_some_and(|(_, field)| field == "token")
}

fn push_repeated(argv: &mut Vec<String>, flag: &str, values: &[String]) {
    for v in values {
        argv.push(flag.to_string());
        argv.push(v.clone());
    }
}

/// Self-invokes the current `tooler` binary as a subprocess with the given
/// argv, and returns its output as an MCP tool result. Always appends
/// `--output json` (a no-op for commands that don't branch on it) and sets
/// `NO_COLOR=1` so plain-text responses come back without ANSI escapes.
async fn exec_self(
    mut argv: Vec<String>,
    cwd: &Option<String>,
) -> Result<CallToolResult, McpError> {
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

    if output.status.success() {
        Ok(CallToolResult::success(vec![ContentBlock::text(stdout)]))
    } else {
        let message = if stderr.trim().is_empty() {
            stdout
        } else {
            stderr
        };
        Ok(CallToolResult::error(vec![ContentBlock::text(message)]))
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
    /// Bearer token for the Authorization header
    token: Option<String>,
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
    token: Option<String>,
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
}

impl ToolerMcp {
    pub fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }
}

impl Default for ToolerMcp {
    fn default() -> Self {
        Self::new()
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
        exec_self(argv, &None).await
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
        exec_self(argv, &None).await
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
        exec_self(argv, &None).await
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
        exec_self(argv, &args.cwd).await
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
        exec_self(argv, &args.cwd).await
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
        exec_self(argv, &args.cwd).await
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
        exec_self(argv, &args.cwd).await
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
        exec_self(argv, &args.cwd).await
    }

    #[tool(
        description = "Perform an HTTP GET request, with optional profile-based auth",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = true)
    )]
    async fn tooler_http_get(
        &self,
        Parameters(args): Parameters<HttpGetArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec!["http".to_string(), "get".to_string(), args.url.clone()];
        push_opt(&mut argv, "--token", &args.token);
        push_repeated(&mut argv, "--header", &args.headers);
        push_opt_num(&mut argv, "--timeout", args.timeout);
        push_opt(&mut argv, "--profile", &args.profile);
        exec_self(argv, &None).await
    }

    #[tool(
        description = "Perform an HTTP POST request with a JSON body",
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
        push_opt(&mut argv, "--token", &args.token);
        push_repeated(&mut argv, "--header", &args.headers);
        push_opt_num(&mut argv, "--timeout", args.timeout);
        push_opt(&mut argv, "--profile", &args.profile);
        exec_self(argv, &None).await
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
        exec_self(argv, &None).await
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
        exec_self(argv, &None).await
    }

    #[tool(
        description = "Show tooler's full configuration",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn tooler_config_show(&self) -> Result<CallToolResult, McpError> {
        exec_self(vec!["config".to_string(), "show".to_string()], &None).await
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
        exec_self(argv, &None).await
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
        exec_self(argv, &None).await
    }

    #[tool(
        description = "List configured tooler profiles",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn tooler_config_profiles(&self) -> Result<CallToolResult, McpError> {
        exec_self(vec!["config".to_string(), "profiles".to_string()], &None).await
    }

    #[tool(
        description = "Print tooler's config file path",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn tooler_config_path(&self) -> Result<CallToolResult, McpError> {
        exec_self(vec!["config".to_string(), "path".to_string()], &None).await
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
        exec_self(argv, &None).await
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
        exec_self(argv, &args.cwd).await
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
        exec_self(argv, &args.cwd).await
    }

    #[tool(
        description = "Compact git repo summary: branch, tag, status, recent commits",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn tooler_git_summary(
        &self,
        Parameters(args): Parameters<GitCwdArgs>,
    ) -> Result<CallToolResult, McpError> {
        exec_self(vec!["git".to_string(), "summary".to_string()], &args.cwd).await
    }

    #[tool(
        description = "Delete branches already merged into the current branch",
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
        exec_self(argv, &args.cwd).await
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
        exec_self(argv, &args.cwd).await
    }

    #[tool(
        description = "List available scaffold templates",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn tooler_scaffold_list(&self) -> Result<CallToolResult, McpError> {
        exec_self(vec!["scaffold".to_string(), "list".to_string()], &None).await
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
        exec_self(argv, &args.cwd).await
    }

    #[tool(
        description = "List configured server profiles (host, user, SSH key)",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn tooler_server_list(&self) -> Result<CallToolResult, McpError> {
        exec_self(vec!["server".to_string(), "list".to_string()], &None).await
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
        exec_self(argv, &None).await
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
        exec_self(argv, &None).await
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
        exec_self(argv, &None).await
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
        exec_self(argv, &None).await
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
        exec_self(argv, &None).await
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
        exec_self(argv, &None).await
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
        exec_self(argv, &None).await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for ToolerMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("tooler", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Tooler: a devops CLI toolkit. Tools mirror the `tooler` subcommands 1:1 \
                 (env, http, check, git, ssh, server profiles, run/play automation). \
                 Most tools accept an optional `cwd` to target a specific project directory.",
            )
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
