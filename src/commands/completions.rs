use crate::{cli::Cli, context::Context};
use anyhow::Result;
use clap::{Args, CommandFactory};
use clap_complete::{Shell, generate};
use std::io;

#[derive(Args)]
pub struct CompletionsArgs {
    /// Shell to generate completions for
    pub shell: Shell,
}

pub fn run(args: CompletionsArgs, _ctx: &Context) -> Result<()> {
    let mut cmd = Cli::command();
    generate(args.shell, &mut cmd, "tooler", &mut io::stdout());
    Ok(())
}
