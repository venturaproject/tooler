use crate::{
    commands::{
        check::CheckArgs, completions::CompletionsArgs, config::ConfigArgs, cron::CronArgs,
        db::DbArgs, deploy::DeployArgs, doctor::DoctorArgs, echo::EchoArgs, env::EnvArgs,
        fleet::FleetArgs, fs::FsArgs, gh::GhArgs, git::GitArgs, group::GroupArgs, http::HttpArgs,
        info::InfoArgs, jobs::JobsArgs, json::JsonArgs, logs::LogsArgs, mail::MailArgs,
        mcp::McpArgs, play::PlayArgs, ps::PsArgs, report::ReportArgs, run::RunArgs,
        scaffold::ScaffoldArgs, server::ServerArgs, ssh::SshArgs, stat::StatArgs,
        systemd::SystemdArgs, vault::VaultArgs,
    },
    output::OutputFormat,
};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "tooler",
    version,
    about = "A modular CLI toolkit",
    long_about = None,
    propagate_version = true,
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,

    /// Output format (overrides config default)
    #[arg(long, global = true, value_enum)]
    pub output: Option<OutputFormat>,

    /// Profile to use from config
    #[arg(long, global = true, default_value = "default")]
    pub profile: String,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Show system information
    Info(InfoArgs),

    /// Echo text with optional formatting
    Echo(EchoArgs),

    /// Pretty-print and query JSON
    Json(JsonArgs),

    /// Manage .env files (show, diff, check, get)
    Env(EnvArgs),

    /// Make HTTP requests (GET, POST) with profile auth
    Http(HttpArgs),

    /// Search job listings (Adzuna)
    Jobs(JobsArgs),

    /// Health-check URLs and TCP ports
    Check(CheckArgs),

    /// Manage tooler configuration
    Config(ConfigArgs),

    /// Run a script defined in .tooler.toml
    Run(RunArgs),

    /// Run a YAML playbook (tasks, vars, health checks)
    Play(PlayArgs),

    /// Git utilities: summary, clean branches, changelog
    Git(GitArgs),

    /// Create new projects from templates
    Scaffold(ScaffoldArgs),

    /// Generate shell completion scripts
    Completions(CompletionsArgs),

    /// Manage server profiles (host, user, SSH key)
    Server(ServerArgs),

    /// SSH operations: check, exec, copy, ssl deploy
    Ssh(SshArgs),

    /// Run tooler as an MCP server (stdio) for use with Claude and other MCP clients
    Mcp(McpArgs),

    /// Run environment/health checks (git, OS keychain, SSH keys, self-exe)
    Doctor(DoctorArgs),

    /// Generate PDF/Excel reports from the JSON output of other tooler commands
    Report(ReportArgs),

    /// Query a remote database over SSH (read-only)
    Db(DbArgs),

    /// Pull request data via the `gh` CLI (title, labels, dates)
    Gh(GhArgs),

    /// Manage systemd units on a remote server over SSH (status, restart, logs)
    Systemd(SystemdArgs),

    /// Manage a remote server's crontab over SSH (list, add, remove)
    Cron(CronArgs),

    /// Read remote log files over SSH (tail, grep)
    Logs(LogsArgs),

    /// Manage remote processes over SSH (list, kill)
    Ps(PsArgs),

    /// Read, write, and diff remote files over SSH
    Fs(FsArgs),

    /// Orchestrate a remote deploy: pull, build, restart, health check
    Deploy(DeployArgs),

    /// Run a command or check SSH connectivity against multiple server profiles at once
    Fleet(FleetArgs),

    /// Resource snapshot (uptime/load, memory, disk) for a remote server over SSH
    Stat(StatArgs),

    /// Manage named groups of server profiles (used by tooler fleet and playbook tasks)
    Group(GroupArgs),

    /// Send email over SMTP (config.mail.<name> profiles or inline host/user/password)
    Mail(MailArgs),

    /// Encrypt/decrypt/view a file in place with a passphrase (AES-256-GCM) — for
    /// committing secrets a playbook's vars_files:/--vars-file can read encrypted at rest
    Vault(VaultArgs),
}
