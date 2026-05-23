mod cli;
mod commands;
mod config;
mod context;
mod error;
mod output;
mod project;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Commands};
use context::Context;
use output::OutputFormat;

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg = config::load()?;

    let output = cli.output.unwrap_or(match cfg.default.output.as_str() {
        "json"  => OutputFormat::Json,
        "table" => OutputFormat::Table,
        _       => OutputFormat::Plain,
    });

    let ctx = Context::new(output, cli.profile, cfg);

    match cli.command {
        Commands::Info(args)        => commands::info::run(args, &ctx),
        Commands::Echo(args)        => commands::echo::run(args, &ctx),
        Commands::Json(args)        => commands::json::run(args, &ctx),
        Commands::Env(args)         => commands::env::run(args, &ctx),
        Commands::Http(args)        => commands::http::run(args, &ctx),
        Commands::Check(args)       => commands::check::run(args, &ctx),
        Commands::Run(args)         => commands::run::run(args, &ctx),
        Commands::Git(args)         => commands::git::run(args, &ctx),
        Commands::Scaffold(args)    => commands::scaffold::run(args, &ctx),
        Commands::Config(args)      => commands::config::run(args, &ctx),
        Commands::Completions(args) => commands::completions::run(args, &ctx),
    }
}
