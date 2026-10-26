use crate::{context::Context, output::OutputFormat};
use anyhow::{Context as _, Result, bail};
use clap::{Args, Subcommand};
use colored::Colorize;
use std::collections::{BTreeMap, HashMap};

#[derive(Args)]
pub struct EnvArgs {
    #[command(subcommand)]
    pub subcommand: EnvSubcommand,
}

#[derive(Subcommand)]
pub enum EnvSubcommand {
    /// Show variables from a .env file (values masked by default)
    Show {
        #[arg(default_value = ".env")]
        file: String,
        /// Show real values instead of masking them
        #[arg(long)]
        reveal: bool,
    },
    /// List only the variable names
    List {
        #[arg(default_value = ".env")]
        file: String,
    },
    /// Get a specific variable value
    Get {
        key: String,
        #[arg(default_value = ".env")]
        file: String,
    },
    /// Show keys present in one file but missing in the other
    Diff { file_a: String, file_b: String },
    /// Verify all keys from a reference file exist in the target
    Check {
        /// Reference file (e.g. .env.example)
        reference: String,
        /// File to check [default: .env]
        #[arg(default_value = ".env")]
        target: String,
    },
    /// Validate values using an optional YAML schema (never prints values)
    Validate {
        schema: String,
        /// File to validate [default: .env]
        #[arg(default_value = ".env")]
        file: String,
    },
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct EnvSchema {
    variables: BTreeMap<String, VariableSpec>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct VariableSpec {
    #[serde(rename = "type", default = "default_type")]
    value_type: String,
    #[serde(default = "default_required")]
    required: bool,
    #[serde(rename = "enum", default)]
    allowed: Vec<String>,
    min: Option<f64>,
    max: Option<f64>,
    #[serde(default)]
    non_empty: bool,
}

fn default_type() -> String {
    "string".into()
}
fn default_required() -> bool {
    true
}

fn validate_env(schema_path: &str, file: &str, ctx: &Context) -> Result<()> {
    let schema: EnvSchema = serde_yaml::from_str(
        &std::fs::read_to_string(schema_path)
            .with_context(|| format!("Cannot read schema: {schema_path}"))?,
    )
    .with_context(|| format!("parsing env schema {schema_path}"))?;
    for (key, spec) in &schema.variables {
        if !matches!(
            spec.value_type.as_str(),
            "string" | "integer" | "number" | "boolean" | "url"
        ) {
            bail!("unsupported type '{}' for {key}", spec.value_type);
        }
        if spec.min.is_some() || spec.max.is_some() {
            if !matches!(spec.value_type.as_str(), "integer" | "number") {
                bail!("min/max are only supported for numeric variable {key}");
            }
            if spec.min.zip(spec.max).is_some_and(|(min, max)| min > max) {
                bail!("minimum exceeds maximum for {key}");
            }
            if spec.min.is_some_and(|value| !value.is_finite())
                || spec.max.is_some_and(|value| !value.is_finite())
            {
                bail!("min/max must be finite numbers for {key}");
            }
        }
    }
    let vars = parse_env_file(file)?;
    let mut errors = Vec::new();
    for (key, spec) in &schema.variables {
        let Some(value) = vars.get(key) else {
            if spec.required {
                errors.push((key.clone(), "missing".to_string()));
            }
            continue;
        };
        let issue = if spec.non_empty && value.trim().is_empty() {
            Some("must not be empty")
        } else {
            match spec.value_type.as_str() {
                "string" => None,
                "integer" => match value.parse::<i64>() {
                    Ok(n) => range_issue(n as f64, spec),
                    Err(_) => Some("must be an integer"),
                },
                "number" => match value.parse::<f64>() {
                    Ok(n) if n.is_finite() => range_issue(n, spec),
                    _ => Some("must be a number"),
                },
                "boolean"
                    if !matches!(
                        value.to_ascii_lowercase().as_str(),
                        "true" | "false" | "1" | "0"
                    ) =>
                {
                    Some("must be a boolean (true/false/1/0)")
                }
                "boolean" => None,
                "url" if url::Url::parse(value).is_err() => Some("must be a valid absolute URL"),
                "url" => None,
                _ => unreachable!("schema types were validated"),
            }
        };
        if let Some(issue) = issue {
            errors.push((key.clone(), issue.to_string()));
        }
        if !spec.allowed.is_empty() && !spec.allowed.iter().any(|candidate| candidate == value) {
            errors.push((key.clone(), "is not one of the allowed values".to_string()));
        }
    }
    let ok = errors.is_empty();
    if ctx.output == OutputFormat::Json {
        println!(
            "{}",
            serde_json::json!({"ok": ok, "errors": errors.iter().map(|(key, issue)| serde_json::json!({"key": key, "issue": issue})).collect::<Vec<_>>()})
        );
    } else if ok {
        println!("{} validated {}", "✓".green().bold(), file);
    } else {
        for (key, issue) in &errors {
            eprintln!("{} {key}: {issue}", "✗".red().bold());
        }
    }
    if !ok {
        bail!("{} env validation error(s)", errors.len());
    }
    Ok(())
}

fn range_issue(value: f64, spec: &VariableSpec) -> Option<&'static str> {
    if spec.min.is_some_and(|min| value < min) {
        Some("is below the minimum")
    } else if spec.max.is_some_and(|max| value > max) {
        Some("is above the maximum")
    } else {
        None
    }
}

fn parse_env_file(path: &str) -> Result<HashMap<String, String>> {
    let content =
        std::fs::read_to_string(path).with_context(|| format!("Cannot read file: {path}"))?;

    let mut map = HashMap::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let value = value.trim().trim_matches('"').trim_matches('\'');
            map.insert(key.trim().to_string(), value.to_string());
        }
    }
    Ok(map)
}

fn mask(value: &str) -> String {
    if value.is_empty() {
        return "(empty)".dimmed().to_string();
    }
    let visible = value.len().min(3);
    format!(
        "{}{}",
        &value[..visible],
        "*".repeat(value.len().saturating_sub(visible))
    )
}

pub fn run(args: EnvArgs, ctx: &Context) -> Result<()> {
    match args.subcommand {
        EnvSubcommand::Show { file, reveal } => {
            let vars = parse_env_file(&file)?;
            let mut pairs: Vec<(String, String)> = vars
                .into_iter()
                .map(|(k, v)| (k, if reveal { v } else { mask(&v) }))
                .collect();
            pairs.sort_by(|a, b| a.0.cmp(&b.0));

            let kv_refs: Vec<(&str, &str)> = pairs
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();

            if ctx.output == OutputFormat::Plain {
                println!("{} {}", "env:".bold(), file.cyan());
                println!("{}", "─".repeat(40).dimmed());
            }
            ctx.output.print_kv(&kv_refs);
        }

        EnvSubcommand::List { file } => {
            let vars = parse_env_file(&file)?;
            let mut keys: Vec<String> = vars.into_keys().collect();
            keys.sort();
            if ctx.output == OutputFormat::Json {
                println!("{}", serde_json::json!({"keys": keys}));
                return Ok(());
            }
            for key in &keys {
                println!("{key}");
            }
        }

        EnvSubcommand::Get { key, file } => {
            let vars = parse_env_file(&file)?;
            match vars.get(&key) {
                Some(val) => {
                    if ctx.output == OutputFormat::Json {
                        println!("{}", serde_json::json!({"key": key, "value": val}));
                    } else {
                        println!("{val}");
                    }
                }
                None => {
                    if ctx.output == OutputFormat::Json {
                        println!(
                            "{}",
                            serde_json::json!({"key": key, "error": format!("Key '{key}' not found in {file}")})
                        );
                        std::process::exit(1);
                    }
                    bail!("Key '{}' not found in {}", key, file)
                }
            }
        }

        EnvSubcommand::Diff { file_a, file_b } => {
            let vars_a = parse_env_file(&file_a)?;
            let vars_b = parse_env_file(&file_b)?;

            let mut all_keys: Vec<String> = vars_a.keys().chain(vars_b.keys()).cloned().collect();
            all_keys.sort();
            all_keys.dedup();

            if ctx.output == OutputFormat::Json {
                let mut only_in_a = vec![];
                let mut only_in_b = vec![];
                for key in &all_keys {
                    match (vars_a.contains_key(key), vars_b.contains_key(key)) {
                        (true, false) => only_in_a.push(key.clone()),
                        (false, true) => only_in_b.push(key.clone()),
                        _ => {}
                    }
                }
                println!(
                    "{}",
                    serde_json::json!({"only_in_a": only_in_a, "only_in_b": only_in_b})
                );
                return Ok(());
            }

            let mut diffs = false;
            for key in &all_keys {
                match (vars_a.contains_key(key), vars_b.contains_key(key)) {
                    (true, false) => {
                        println!(
                            "{} {} {}",
                            "−".red().bold(),
                            key,
                            format!("(only in {file_a})").dimmed()
                        );
                        diffs = true;
                    }
                    (false, true) => {
                        println!(
                            "{} {} {}",
                            "+".green().bold(),
                            key,
                            format!("(only in {file_b})").dimmed()
                        );
                        diffs = true;
                    }
                    _ => {}
                }
            }
            if !diffs {
                println!("{}", "No differences in keys.".green());
            }
        }

        EnvSubcommand::Check { reference, target } => {
            let ref_vars = parse_env_file(&reference)?;
            let target_vars = parse_env_file(&target)?;

            let mut missing: Vec<String> = ref_vars
                .into_keys()
                .filter(|k| !target_vars.contains_key(k))
                .collect();
            missing.sort();

            if ctx.output == OutputFormat::Json {
                let ok = missing.is_empty();
                println!("{}", serde_json::json!({"missing": missing, "ok": ok}));
                if !ok {
                    std::process::exit(1);
                }
                return Ok(());
            }

            if missing.is_empty() {
                println!(
                    "{} all keys from {} present in {}",
                    "✓".green().bold(),
                    reference,
                    target
                );
            } else {
                eprintln!("{} missing keys in {}:", "✗".red().bold(), target);
                for key in &missing {
                    eprintln!("  {}", key.yellow());
                }
                bail!("{} missing key(s)", missing.len());
            }
        }

        EnvSubcommand::Validate { schema, file } => validate_env(&schema, &file, ctx)?,
    }
    Ok(())
}
