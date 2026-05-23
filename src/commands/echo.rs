use crate::context::Context;
use anyhow::Result;
use clap::Args;
use colored::Colorize;

#[derive(Args)]
pub struct EchoArgs {
    /// Text to echo
    pub text: Vec<String>,

    /// Print in uppercase
    #[arg(short, long)]
    pub upper: bool,

    /// Color: red, green, blue, yellow, cyan, magenta
    #[arg(short, long, default_value = "white")]
    pub color: String,

    /// Repeat N times
    #[arg(short, long, default_value_t = 1)]
    pub repeat: u32,
}

pub fn run(args: EchoArgs, _ctx: &Context) -> Result<()> {
    let text = args.text.join(" ");
    let text = if args.upper {
        text.to_uppercase()
    } else {
        text
    };

    for _ in 0..args.repeat {
        let line = match args.color.as_str() {
            "red" => text.red().to_string(),
            "green" => text.green().to_string(),
            "blue" => text.blue().to_string(),
            "yellow" => text.yellow().to_string(),
            "cyan" => text.cyan().to_string(),
            "magenta" => text.magenta().to_string(),
            _ => text.normal().to_string(),
        };
        println!("{line}");
    }

    Ok(())
}
