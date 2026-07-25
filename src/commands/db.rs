use crate::{
    commands::ssh::resolve_server,
    config::Server,
    context::Context,
    db::{self, Credentials, Engine},
    output::OutputFormat,
};
use anyhow::{Result, bail};
use clap::{Args, Subcommand};
use colored::Colorize;

#[derive(Args)]
pub struct DbArgs {
    #[command(subcommand)]
    pub subcommand: DbSubcommand,
}

#[derive(Subcommand)]
pub enum DbSubcommand {
    /// Run a read-only SQL query against a remote database by invoking psql/mysql
    /// directly on the server over SSH, printing rows as JSON (feed straight into
    /// `tooler report`)
    Query {
        /// Server profile to run the query through (see: tooler server list)
        server: String,
        /// SQL query (SELECT/SHOW/EXPLAIN/WITH/DESCRIBE only)
        sql: String,
        /// Remote path to a dotenv-style file (e.g. Laravel .env) to read DB_* credentials from
        #[arg(long)]
        env: Option<String>,
        /// DB engine when not using --env: mysql or postgres
        #[arg(long)]
        engine: Option<String>,
        /// DB host as reachable from the server profile (when not using --env)
        #[arg(long)]
        host: Option<String>,
        /// DB port (when not using --env; defaults to the engine's standard port)
        #[arg(long)]
        port: Option<u16>,
        /// Database name (when not using --env)
        #[arg(long)]
        database: Option<String>,
        /// DB username (when not using --env)
        #[arg(long)]
        user: Option<String>,
        /// DB password [env: TOOLER_DB_PASSWORD] (when not using --env)
        #[arg(long, env = "TOOLER_DB_PASSWORD")]
        password: Option<String>,
        /// Cap the number of rows returned
        #[arg(long, default_value_t = 1000)]
        max_rows: usize,
    },
}

pub fn run(args: DbArgs, ctx: &Context) -> Result<()> {
    match args.subcommand {
        DbSubcommand::Query {
            server,
            sql,
            env,
            engine,
            host,
            port,
            database,
            user,
            password,
            max_rows,
        } => query(
            ctx,
            &server,
            &sql,
            QueryOpts {
                env: env.as_deref(),
                engine: engine.as_deref(),
                host: host.as_deref(),
                port,
                database: database.as_deref(),
                user: user.as_deref(),
                password: password.as_deref(),
                max_rows,
            },
        ),
    }
}

struct QueryOpts<'a> {
    env: Option<&'a str>,
    engine: Option<&'a str>,
    host: Option<&'a str>,
    port: Option<u16>,
    database: Option<&'a str>,
    user: Option<&'a str>,
    password: Option<&'a str>,
    max_rows: usize,
}

fn fail(json: bool, message: String) -> Result<()> {
    if json {
        println!("{}", serde_json::json!({ "error": message }));
        std::process::exit(1);
    }
    bail!(message);
}

fn resolve_credentials(server: &Server, opts: &QueryOpts) -> Result<Credentials> {
    if let Some(path) = opts.env {
        return db::credentials_from_remote_env(server, path);
    }

    let engine = match opts.engine.map(str::to_lowercase).as_deref() {
        Some("mysql" | "mariadb") => Engine::MySql,
        Some("postgres" | "postgresql" | "pgsql") => Engine::Postgres,
        Some(other) => bail!("Unknown --engine '{other}'; expected mysql or postgres"),
        None => bail!(
            "Provide --env <remote .env path>, or --engine/--host/--database/--user explicitly"
        ),
    };
    let host = opts
        .host
        .ok_or_else(|| anyhow::anyhow!("--host is required without --env"))?
        .to_string();
    let database = opts
        .database
        .ok_or_else(|| anyhow::anyhow!("--database is required without --env"))?
        .to_string();
    let user = opts
        .user
        .ok_or_else(|| anyhow::anyhow!("--user is required without --env"))?
        .to_string();
    let password = opts.password.unwrap_or_default().to_string();
    let port = opts.port.unwrap_or_else(|| engine.default_port());

    Ok(Credentials {
        engine,
        host,
        port,
        database,
        user,
        password,
    })
}

fn print_table(rows: &[serde_json::Value]) {
    if rows.is_empty() {
        println!("{}", "(no rows)".dimmed());
        return;
    }
    let mut columns: Vec<String> = Vec::new();
    for row in rows {
        if let serde_json::Value::Object(map) = row {
            for k in map.keys() {
                if !columns.contains(k) {
                    columns.push(k.clone());
                }
            }
        }
    }

    let cell = |row: &serde_json::Value, c: &str| -> String {
        match row.get(c) {
            Some(serde_json::Value::Null) | None => String::new(),
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
        }
    };

    let mut widths: Vec<usize> = columns.iter().map(|c| c.len()).collect();
    for row in rows {
        for (i, c) in columns.iter().enumerate() {
            widths[i] = widths[i].max(cell(row, c).len());
        }
    }

    let header: Vec<String> = columns
        .iter()
        .enumerate()
        .map(|(i, c)| format!("{c:width$}", width = widths[i]))
        .collect();
    println!("{}", header.join("  ").bold());
    for row in rows {
        let line: Vec<String> = columns
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{:width$}", cell(row, c), width = widths[i]))
            .collect();
        println!("{}", line.join("  "));
    }
}

fn query(ctx: &Context, server_name: &str, sql: &str, opts: QueryOpts) -> Result<()> {
    let json = ctx.output == OutputFormat::Json;

    let server = resolve_server(ctx, server_name)?;
    let creds = match resolve_credentials(&server, &opts) {
        Ok(c) => c,
        Err(e) => return fail(json, format!("{e:#}")),
    };

    let (rows, truncated) = match db::run_query(&server, &creds, sql, opts.max_rows) {
        Ok(r) => r,
        Err(e) => return fail(json, format!("{e:#}")),
    };

    if json {
        println!(
            "{}",
            serde_json::json!({
                "server": server_name,
                "rows": rows,
                "row_count": rows.len(),
                "truncated": truncated,
            })
        );
        return Ok(());
    }

    let suffix = if truncated { ", truncated" } else { "" };
    println!(
        "{} {} {}",
        "query on".bold(),
        server_name.cyan(),
        format!("({} rows{suffix})", rows.len()).dimmed()
    );
    println!("{}", "─".repeat(40).dimmed());
    print_table(&rows);
    Ok(())
}
