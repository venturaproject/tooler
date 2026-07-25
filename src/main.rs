mod cli;
mod commands;
mod config;
mod context;
mod db;
mod output;
mod project;
mod report;
mod secrets;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Commands};
use context::Context;
use output::OutputFormat;

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg = config::load()?;

    let output = cli.output.unwrap_or(match cfg.default.output.as_str() {
        "json" => OutputFormat::Json,
        "table" => OutputFormat::Table,
        _ => OutputFormat::Plain,
    });

    let ctx = Context::new(output, cli.profile, cfg);

    match cli.command {
        Commands::Info(args) => commands::info::run(args, &ctx),
        Commands::Echo(args) => commands::echo::run(args, &ctx),
        Commands::Json(args) => commands::json::run(args, &ctx),
        Commands::Env(args) => commands::env::run(args, &ctx),
        Commands::Http(args) => commands::http::run(args, &ctx),
        Commands::Check(args) => commands::check::run(args, &ctx),
        Commands::Run(args) => commands::run::run(args, &ctx),
        Commands::Play(args) => commands::play::run(args, &ctx),
        Commands::Git(args) => commands::git::run(args, &ctx),
        Commands::Scaffold(args) => commands::scaffold::run(args, &ctx),
        Commands::Config(args) => commands::config::run(args, &ctx),
        Commands::Completions(args) => commands::completions::run(args, &ctx),
        Commands::Server(args) => commands::server::run(args, &ctx),
        Commands::Ssh(args) => commands::ssh::run(args, &ctx),
        Commands::Mcp(args) => commands::mcp::run(args, &ctx),
        Commands::Doctor(args) => commands::doctor::run(args, &ctx),
        Commands::Report(args) => commands::report::run(args, &ctx),
        Commands::Db(args) => commands::db::run(args, &ctx),
        Commands::Gh(args) => commands::gh::run(args, &ctx),
        Commands::Systemd(args) => commands::systemd::run(args, &ctx),
        Commands::Cron(args) => commands::cron::run(args, &ctx),
        Commands::Logs(args) => commands::logs::run(args, &ctx),
    }
}
