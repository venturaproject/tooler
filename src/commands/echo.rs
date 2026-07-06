use crate::context::Context;
use anyhow::Result;
use clap::Args;
use colored::Colorize;

#[derive(Args, serde::Deserialize, schemars::JsonSchema)]
pub struct EchoArgs {
    /// Text to echo
    #[serde(default)]
    pub text: Vec<String>,

    /// Print in uppercase
    #[arg(short, long)]
    #[serde(default)]
    pub upper: bool,

    /// Color: red, green, blue, yellow, cyan, magenta
    #[arg(short, long, default_value = "white")]
    #[serde(default = "default_echo_color")]
    pub color: String,

    /// Repeat N times
    #[arg(short, long, default_value_t = 1)]
    #[serde(default = "default_echo_repeat")]
    pub repeat: u32,
}

fn default_echo_color() -> String {
    "white".to_string()
}

fn default_echo_repeat() -> u32 {
    1
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
