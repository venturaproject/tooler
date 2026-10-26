use crate::{context::Context, history};
use anyhow::Result;
use clap::Args;

#[derive(Args)]
pub struct HistoryArgs {
    /// Maximum number of records to show
    #[arg(short, long, default_value_t = 20, value_parser = clap::value_parser!(u32).range(1..=1000))]
    pub limit: u32,
    /// Show records for one command family (task, play, deploy, monitor, etc.)
    #[arg(long)]
    pub command: Option<String>,
    /// Show only failed command invocations
    #[arg(long)]
    pub failed: bool,
}

pub fn run(args: HistoryArgs, ctx: &Context) -> Result<()> {
    history::list(
        history::HistoryArgs {
            limit: args.limit,
            command: args.command,
            failed: args.failed,
        },
        ctx,
    )
}
