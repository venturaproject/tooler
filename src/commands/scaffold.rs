use crate::context::Context;
use anyhow::{Result, bail};
use clap::{Args, Subcommand};
use colored::Colorize;
use std::collections::HashMap;
use std::path::Path;

#[derive(Args)]
pub struct ScaffoldArgs {
    #[command(subcommand)]
    pub subcommand: ScaffoldSubcommand,
}

#[derive(Subcommand)]
pub enum ScaffoldSubcommand {
    /// List available templates
    List,
    /// Create a new project from a template
    New {
        /// Template name (see: tooler scaffold list)
        template: String,
        /// Project name
        name: String,
        /// Destination directory [default: ./<name>]
        #[arg(short, long)]
        dir: Option<String>,
    },
}

struct Template {
    name: &'static str,
    description: &'static str,
    files: Vec<(&'static str, String)>,
}

fn templates() -> Vec<Template> {
    vec![
        Template {
            name: "rust-cli",
            description: "Rust CLI application with clap and anyhow",
            files: vec![
                (
                    "Cargo.toml",
                    r#"[package]
name = "{{name_snake}}"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "{{name_snake}}"
path = "src/main.rs"

[dependencies]
clap = { version = "4", features = ["derive"] }
anyhow = "1"
"#
                    .into(),
                ),
                (
                    "src/main.rs",
                    r#"use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "{{name_snake}}", version, about = "{{name}}")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Greet someone
    Hello {
        name: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Hello { name } => println!("Hello, {name}!"),
    }
    Ok(())
}
"#
                    .into(),
                ),
                (".gitignore", "/target\n".into()),
                (
                    "README.md",
                    "# {{name}}\n\n```sh\ncargo run -- hello world\n```\n".into(),
                ),
            ],
        },
        Template {
            name: "node-api",
            description: "Node.js REST API with Express",
            files: vec![
                (
                    "package.json",
                    r#"{
  "name": "{{name_snake}}",
  "version": "0.1.0",
  "description": "{{name}}",
  "main": "src/index.js",
  "scripts": {
    "start": "node src/index.js",
    "dev": "node --watch src/index.js"
  },
  "dependencies": {
    "express": "^4.18.0"
  }
}
"#
                    .into(),
                ),
                (
                    "src/index.js",
                    r#"const express = require('express')
const app = express()
const PORT = process.env.PORT || 3000

app.use(express.json())

app.get('/health', (_req, res) => {
  res.json({ status: 'ok' })
})

app.listen(PORT, () => {
  console.log(`{{name}} running on http://localhost:${PORT}`)
})
"#
                    .into(),
                ),
                (".gitignore", "node_modules/\n.env\n".into()),
                (
                    "README.md",
                    "# {{name}}\n\n```sh\nnpm install\nnpm run dev\n```\n".into(),
                ),
            ],
        },
        Template {
            name: "python-cli",
            description: "Python CLI script with argparse",
            files: vec![
                (
                    "main.py",
                    r#"import argparse
import sys


def main():
    parser = argparse.ArgumentParser(description='{{name}}')
    subparsers = parser.add_subparsers(dest='command', required=True)

    hello = subparsers.add_parser('hello', help='Greet someone')
    hello.add_argument('name', help='Name to greet')

    args = parser.parse_args()

    if args.command == 'hello':
        print(f'Hello, {args.name}!')


if __name__ == '__main__':
    main()
"#
                    .into(),
                ),
                ("requirements.txt", "# add dependencies here\n".into()),
                (".gitignore", "__pycache__/\n*.pyc\n.venv/\n.env\n".into()),
                (
                    "README.md",
                    "# {{name}}\n\n```sh\npython main.py hello world\n```\n".into(),
                ),
            ],
        },
    ]
}

fn to_snake(s: &str) -> String {
    s.replace(['-', ' '], "_").to_lowercase()
}

fn render(content: &str, vars: &HashMap<&str, String>) -> String {
    let mut out = content.to_string();
    for (k, v) in vars {
        out = out.replace(&format!("{{{{{k}}}}}"), v);
    }
    out
}

pub fn run(args: ScaffoldArgs, _ctx: &Context) -> Result<()> {
    match args.subcommand {
        ScaffoldSubcommand::List => list(),
        ScaffoldSubcommand::New {
            template,
            name,
            dir,
        } => new(&template, &name, dir),
    }
}

fn list() -> Result<()> {
    println!("{}", "available templates:".bold().cyan());
    println!("{}", "─".repeat(40).dimmed());
    for t in templates() {
        println!("  {:20} {}", t.name.bold(), t.description.dimmed());
    }
    println!();
    println!(
        "{}",
        "Usage: tooler scaffold new <template> <name>".dimmed()
    );
    Ok(())
}

fn new(template_name: &str, name: &str, output: Option<String>) -> Result<()> {
    let all = templates();
    let tmpl = all
        .iter()
        .find(|t| t.name == template_name)
        .ok_or_else(|| {
            let names: Vec<&str> = all.iter().map(|t| t.name).collect();
            anyhow::anyhow!(
                "Template '{}' not found. Available: {}",
                template_name,
                names.join(", ")
            )
        })?;

    let dest = output.unwrap_or_else(|| name.to_string());
    let dest_path = Path::new(&dest);

    if dest_path.exists() {
        bail!("Directory '{}' already exists.", dest);
    }

    let author = std::process::Command::new("git")
        .args(["config", "user.name"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default()
        .trim()
        .to_string();

    let year = chrono::Local::now().format("%Y").to_string();

    let mut vars: HashMap<&str, String> = HashMap::new();
    vars.insert("name", name.to_string());
    vars.insert("name_snake", to_snake(name));
    vars.insert("author", author);
    vars.insert("year", year);

    println!("{} {}", "scaffolding".bold().cyan(), name.green());
    println!("{}", "─".repeat(40).dimmed());

    for (rel_path, content) in &tmpl.files {
        let rendered_content = render(content, &vars);
        let full_path = dest_path.join(rel_path);

        if let Some(parent) = full_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        std::fs::write(&full_path, rendered_content)?;
        println!("  {} {}", "created".green(), full_path.display());
    }

    println!();
    println!("{} {}", "done!".bold().green(), format!("cd {dest}").cyan());
    Ok(())
}
