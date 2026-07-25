use crate::{context::Context, output::OutputFormat, report};
use anyhow::{Result, bail};
use clap::{Args, Subcommand};
use colored::Colorize;

#[derive(Args)]
pub struct ReportArgs {
    #[command(subcommand)]
    pub subcommand: ReportSubcommand,
}

#[derive(Subcommand)]
pub enum ReportSubcommand {
    /// Generate a PDF report from one or more JSON sources
    Pdf {
        /// Named JSON input: NAME=PATH (repeatable, `-` for stdin). Omit to read one JSON
        /// document from stdin.
        #[arg(short = 'i', long = "in")]
        input: Vec<String>,
        /// Output PDF path
        #[arg(short, long)]
        out: String,
        /// Report title
        #[arg(short, long, default_value = "Tooler Report")]
        title: String,
    },
    /// Generate an Excel (.xlsx) report from one or more JSON sources
    Excel {
        /// Named JSON input: NAME=PATH (repeatable, `-` for stdin). Omit to read one JSON
        /// document from stdin.
        #[arg(short = 'i', long = "in")]
        input: Vec<String>,
        /// Output .xlsx path
        #[arg(short, long)]
        out: String,
        /// Report title
        #[arg(short, long, default_value = "Tooler Report")]
        title: String,
    },
}

#[derive(Clone, Copy)]
enum Format {
    Pdf,
    Excel,
}

impl Format {
    fn as_str(self) -> &'static str {
        match self {
            Format::Pdf => "pdf",
            Format::Excel => "excel",
        }
    }
}

pub fn run(args: ReportArgs, ctx: &Context) -> Result<()> {
    match args.subcommand {
        ReportSubcommand::Pdf { input, out, title } => {
            generate(Format::Pdf, &input, &out, &title, ctx)
        }
        ReportSubcommand::Excel { input, out, title } => {
            generate(Format::Excel, &input, &out, &title, ctx)
        }
    }
}

fn fail(json: bool, message: String) -> Result<()> {
    if json {
        println!("{}", serde_json::json!({ "error": message }));
        std::process::exit(1);
    }
    bail!(message);
}

fn generate(format: Format, input: &[String], out: &str, title: &str, ctx: &Context) -> Result<()> {
    let json = ctx.output == OutputFormat::Json;

    let sources = match report::load_sources(input) {
        Ok(s) => s,
        Err(e) => return fail(json, format!("{e:#}")),
    };
    let names: Vec<&str> = sources.iter().map(|s| s.name.as_str()).collect();

    let bytes = match format {
        Format::Pdf => report::pdf::build(title, &sources),
        Format::Excel => report::excel::build(title, &sources),
    };
    let bytes = match bytes {
        Ok(b) => b,
        Err(e) => return fail(json, format!("{e:#}")),
    };

    if let Err(e) = std::fs::write(out, &bytes) {
        return fail(json, format!("writing {out}: {e}"));
    }

    if json {
        println!(
            "{}",
            serde_json::json!({
                "format": format.as_str(),
                "title": title,
                "sources": names,
                "out": out,
                "bytes": bytes.len(),
            })
        );
        return Ok(());
    }

    println!("{} {}", "report generated:".bold().green(), out.cyan());
    println!("  {} {}", "format:".dimmed(), format.as_str());
    println!("  {} {}", "sections:".dimmed(), names.join(", "));
    println!("  {} {} bytes", "size:".dimmed(), bytes.len());
    Ok(())
}
