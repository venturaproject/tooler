use crate::{context::Context, output::OutputFormat, secrets};
use anyhow::{Context as _, Result, bail};
use clap::{Args, Subcommand};
use colored::Colorize;
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Args)]
pub struct JobsArgs {
    #[command(subcommand)]
    pub subcommand: JobsSubcommand,
}

#[derive(Subcommand)]
pub enum JobsSubcommand {
    /// Search job listings (Adzuna)
    Search {
        /// Role/keywords to search for (Spanish terms match Spain listings much better than
        /// English ones, e.g. "desarrollador" over "developer")
        #[arg(long, default_value = "desarrollador")]
        what: String,
        /// Location to search in
        #[arg(long, default_value = "madrid")]
        r#where: String,
        /// Adzuna country code (es, gb, us, de, fr, ...)
        #[arg(long, default_value = "es")]
        country: String,
        /// Sector/category tag (e.g. it-jobs, engineering-jobs) — see: tooler jobs categories
        #[arg(long)]
        category: Option<String>,
        /// Result page, 1-indexed
        #[arg(long, default_value_t = 1)]
        page: u32,
        /// Results per page (Adzuna max: 50)
        #[arg(long, default_value_t = 20)]
        results: u32,
        /// Adzuna app_id (overrides stored/env credential) [env: TOOLER_ADZUNA_APP_ID]
        #[arg(long, env = "TOOLER_ADZUNA_APP_ID")]
        app_id: Option<String>,
        /// Adzuna app_key (overrides stored/env credential) [env: TOOLER_ADZUNA_APP_KEY]
        #[arg(long, env = "TOOLER_ADZUNA_APP_KEY")]
        app_key: Option<String>,
    },
    /// List valid sector/category tags for a country (use with `search --category`)
    Categories {
        /// Adzuna country code (es, gb, us, de, fr, ...)
        #[arg(long, default_value = "es")]
        country: String,
        /// Adzuna app_id (overrides stored/env credential) [env: TOOLER_ADZUNA_APP_ID]
        #[arg(long, env = "TOOLER_ADZUNA_APP_ID")]
        app_id: Option<String>,
        /// Adzuna app_key (overrides stored/env credential) [env: TOOLER_ADZUNA_APP_KEY]
        #[arg(long, env = "TOOLER_ADZUNA_APP_KEY")]
        app_key: Option<String>,
    },
    /// Store an Adzuna app_id/app_key in the OS keychain for the current profile
    Configure {
        /// Free credentials: https://developer.adzuna.com/
        #[arg(long)]
        app_id: String,
        #[arg(long)]
        app_key: String,
    },
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct Listing {
    title: String,
    #[serde(default, deserialize_with = "company_name")]
    company: String,
    #[serde(default, deserialize_with = "location_name")]
    location: String,
    #[serde(default)]
    salary_min: Option<f64>,
    #[serde(default)]
    salary_max: Option<f64>,
    #[serde(default)]
    contract_type: Option<String>,
    #[serde(default)]
    created: Option<String>,
    redirect_url: String,
}

impl Listing {
    /// "Company · Location", or just "Location" when Adzuna didn't return a company name
    /// (common for confidential/agency-posted listings) — avoids a dangling "· Location"
    /// with a blank leading space.
    fn company_location_line(&self) -> String {
        if self.company.is_empty() {
            self.location.clone()
        } else {
            format!("{} · {}", self.company, self.location)
        }
    }
}

fn company_name<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    #[derive(Deserialize)]
    struct Company {
        #[serde(default)]
        display_name: String,
    }
    Ok(Company::deserialize(d)
        .unwrap_or(Company {
            display_name: String::new(),
        })
        .display_name)
}

fn location_name<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    #[derive(Deserialize)]
    struct Location {
        #[serde(default)]
        display_name: String,
    }
    Ok(Location::deserialize(d)
        .unwrap_or(Location {
            display_name: String::new(),
        })
        .display_name)
}

/// Explicit flag beats an env-backed clap default, which beats the profile's stored
/// keychain secret (same precedence shape as `http.rs::active_token`).
fn resolve_adzuna_creds(
    app_id: Option<String>,
    app_key: Option<String>,
    ctx: &Context,
) -> Result<(String, String)> {
    let id = app_id.or_else(|| {
        secrets::get_secret(&ctx.profile, "adzuna_app_id")
            .ok()
            .flatten()
    });
    let key = app_key.or_else(|| {
        secrets::get_secret(&ctx.profile, "adzuna_app_key")
            .ok()
            .flatten()
    });
    match (id, key) {
        (Some(id), Some(key)) if !id.is_empty() && !key.is_empty() => Ok((id, key)),
        _ => bail!(
            "No Adzuna credentials. Set TOOLER_ADZUNA_APP_ID/TOOLER_ADZUNA_APP_KEY, pass \
             --app-id/--app-key, or run: tooler jobs configure --app-id <id> --app-key <key>\n\
             Get free credentials at https://developer.adzuna.com/"
        ),
    }
}

pub fn run(args: JobsArgs, ctx: &Context) -> Result<()> {
    match args.subcommand {
        JobsSubcommand::Search {
            what,
            r#where,
            country,
            category,
            page,
            results,
            app_id,
            app_key,
        } => {
            let (app_id, app_key) = resolve_adzuna_creds(app_id, app_key, ctx)?;
            search(
                &what,
                &r#where,
                &country,
                category.as_deref(),
                page,
                results,
                &app_id,
                &app_key,
                ctx,
            )
        }
        JobsSubcommand::Categories {
            country,
            app_id,
            app_key,
        } => {
            let (app_id, app_key) = resolve_adzuna_creds(app_id, app_key, ctx)?;
            categories(&country, &app_id, &app_key, ctx)
        }
        JobsSubcommand::Configure { app_id, app_key } => configure(&app_id, &app_key, ctx),
    }
}

fn configure(app_id: &str, app_key: &str, ctx: &Context) -> Result<()> {
    secrets::set_secret(&ctx.profile, "adzuna_app_id", app_id)?;
    secrets::set_secret(&ctx.profile, "adzuna_app_key", app_key)?;
    println!(
        "{} Adzuna credentials stored for profile '{}'.",
        "✓".green(),
        ctx.profile
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn search(
    what: &str,
    r#where: &str,
    country: &str,
    category: Option<&str>,
    page: u32,
    results: u32,
    app_id: &str,
    app_key: &str,
    ctx: &Context,
) -> Result<()> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;
    let url = format!("https://api.adzuna.com/v1/api/jobs/{country}/search/{page}");

    let mut query = vec![
        ("app_id".to_string(), app_id.to_string()),
        ("app_key".to_string(), app_key.to_string()),
        ("what".to_string(), what.to_string()),
        ("where".to_string(), r#where.to_string()),
        ("results_per_page".to_string(), results.to_string()),
        ("content-type".to_string(), "application/json".to_string()),
    ];
    if let Some(category) = category {
        query.push(("category".to_string(), category.to_string()));
    }

    let resp = client
        .get(&url)
        .query(&query)
        .send()
        .with_context(|| format!("Adzuna request failed: {url}"))?;

    let status = resp.status();
    let body: serde_json::Value = resp.json().context("Adzuna returned non-JSON")?;
    if !status.is_success() {
        bail!("Adzuna returned HTTP {}: {}", status.as_u16(), body);
    }

    let listings: Vec<Listing> = serde_json::from_value(
        body.get("results")
            .cloned()
            .unwrap_or(serde_json::Value::Array(vec![])),
    )
    .context("Failed to parse Adzuna results")?;

    if ctx.output == OutputFormat::Json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "count": listings.len(),
                "what": what,
                "where": r#where,
                "results": listings,
            }))?
        );
        return Ok(());
    }

    if listings.is_empty() {
        println!("{}", "No listings found.".dimmed());
        return Ok(());
    }

    println!(
        "{}",
        format!("{} results for '{}' in '{}'", listings.len(), what, r#where)
            .bold()
            .cyan()
    );
    println!("{}", "─".repeat(50).dimmed());
    for job in &listings {
        println!("{}", job.title.bold());
        println!("  {}", job.company_location_line().green());
        if let (Some(min), Some(max)) = (job.salary_min, job.salary_max) {
            println!("  {}", format!("€{min:.0} – €{max:.0}").yellow());
        }
        if let Some(ct) = &job.contract_type {
            println!("  {}", ct.dimmed());
        }
        println!("  {}", job.redirect_url.dimmed());
        println!();
    }

    Ok(())
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct Category {
    tag: String,
    label: String,
}

fn categories(country: &str, app_id: &str, app_key: &str, ctx: &Context) -> Result<()> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;
    let url = format!("https://api.adzuna.com/v1/api/jobs/{country}/categories");

    let resp = client
        .get(&url)
        .query(&[
            ("app_id", app_id),
            ("app_key", app_key),
            ("content-type", "application/json"),
        ])
        .send()
        .with_context(|| format!("Adzuna request failed: {url}"))?;

    let status = resp.status();
    let body: serde_json::Value = resp.json().context("Adzuna returned non-JSON")?;
    if !status.is_success() {
        bail!("Adzuna returned HTTP {}: {}", status.as_u16(), body);
    }

    let cats: Vec<Category> = serde_json::from_value(
        body.get("results")
            .cloned()
            .unwrap_or(serde_json::Value::Array(vec![])),
    )
    .context("Failed to parse Adzuna categories")?;

    if ctx.output == OutputFormat::Json {
        println!("{}", serde_json::to_string_pretty(&cats)?);
        return Ok(());
    }

    println!(
        "{}",
        format!("{} categories for '{}'", cats.len(), country)
            .bold()
            .cyan()
    );
    for c in &cats {
        println!("  {:<28} {}", c.tag.green(), c.label.dimmed());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::sync::Mutex;

    /// The real macOS Keychain (unlike config files) isn't sandboxed per-test, and
    /// concurrent access from multiple #[test] threads is flaky in practice even across
    /// distinct accounts — serialize every test that touches it behind this lock.
    static KEYCHAIN_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn test_ctx(profile: &str) -> Context {
        Context {
            config: Config::default(),
            profile: profile.to_string(),
            output: OutputFormat::Plain,
        }
    }

    #[test]
    fn resolve_adzuna_creds_prefers_explicit_then_falls_back_to_stored_secret() {
        let _guard = KEYCHAIN_TEST_LOCK.lock().unwrap();
        let ctx = test_ctx("jobs_test_creds");
        secrets::set_secret(&ctx.profile, "adzuna_app_id", "stored-id").unwrap();
        secrets::set_secret(&ctx.profile, "adzuna_app_key", "stored-key").unwrap();

        let (id, key) = resolve_adzuna_creds(
            Some("explicit-id".to_string()),
            Some("explicit-key".to_string()),
            &ctx,
        )
        .unwrap();
        assert_eq!(id, "explicit-id");
        assert_eq!(key, "explicit-key");

        let (id, key) = resolve_adzuna_creds(None, None, &ctx).unwrap();
        assert_eq!(id, "stored-id");
        assert_eq!(key, "stored-key");

        secrets::delete_secret(&ctx.profile, "adzuna_app_id").unwrap();
        secrets::delete_secret(&ctx.profile, "adzuna_app_key").unwrap();
    }

    #[test]
    fn resolve_adzuna_creds_errors_with_setup_hint_when_missing() {
        let _guard = KEYCHAIN_TEST_LOCK.lock().unwrap();
        let ctx = test_ctx("jobs_test_missing");
        let err = resolve_adzuna_creds(None, None, &ctx).unwrap_err();
        assert!(err.to_string().contains("tooler jobs configure"));
    }

    #[test]
    fn listing_deserializes_with_missing_salary() {
        let raw = serde_json::json!({
            "title": "Senior Rust Developer",
            "company": {"display_name": "Acme"},
            "location": {"display_name": "Madrid"},
            "redirect_url": "https://example.com/job/1"
        });
        let listing: Listing = serde_json::from_value(raw).unwrap();
        assert_eq!(listing.title, "Senior Rust Developer");
        assert_eq!(listing.company, "Acme");
        assert_eq!(listing.location, "Madrid");
        assert_eq!(listing.salary_min, None);
        assert_eq!(listing.salary_max, None);
    }

    #[test]
    fn company_location_line_omits_separator_when_company_is_empty() {
        let raw = serde_json::json!({
            "title": "Desarrollador",
            "location": {"display_name": "Madrid"},
            "redirect_url": "https://example.com/job/3"
        });
        let listing: Listing = serde_json::from_value(raw).unwrap();
        assert_eq!(listing.company_location_line(), "Madrid");
    }

    #[test]
    fn company_location_line_joins_company_and_location_when_both_present() {
        let raw = serde_json::json!({
            "title": "Desarrollador",
            "company": {"display_name": "Acme"},
            "location": {"display_name": "Madrid"},
            "redirect_url": "https://example.com/job/4"
        });
        let listing: Listing = serde_json::from_value(raw).unwrap();
        assert_eq!(listing.company_location_line(), "Acme · Madrid");
    }

    #[test]
    fn listing_deserializes_with_full_fields() {
        let raw = serde_json::json!({
            "title": "Backend Developer",
            "company": {"display_name": "Acme"},
            "location": {"display_name": "Madrid, Spain"},
            "salary_min": 35000.0,
            "salary_max": 45000.0,
            "contract_type": "permanent",
            "created": "2026-08-01T10:00:00Z",
            "redirect_url": "https://example.com/job/2"
        });
        let listing: Listing = serde_json::from_value(raw).unwrap();
        assert_eq!(listing.salary_min, Some(35000.0));
        assert_eq!(listing.salary_max, Some(45000.0));
        assert_eq!(listing.contract_type.as_deref(), Some("permanent"));
    }

    #[test]
    fn category_deserializes_tag_and_label() {
        let raw = serde_json::json!({"label": "IT Jobs", "tag": "it-jobs"});
        let cat: Category = serde_json::from_value(raw).unwrap();
        assert_eq!(cat.tag, "it-jobs");
        assert_eq!(cat.label, "IT Jobs");
    }
}
