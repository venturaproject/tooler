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
    /// Run a single INSERT/UPDATE/DELETE statement against a remote database. Preview-only
    /// unless --confirm is passed -- deliberately narrower than `query`: no SELECT, no DDL
    /// (DROP/TRUNCATE/ALTER/CREATE), exactly what marking a row processed or logging an
    /// event needs, not general-purpose SQL execution.
    Exec {
        /// Server profile to run the statement through (see: tooler server list)
        server: String,
        /// SQL statement (INSERT/UPDATE/DELETE only)
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
        /// Actually run the statement (default is preview-only: shows what would run)
        #[arg(long)]
        confirm: bool,
    },
    /// Dump a remote database (pg_dump/mysqldump) to a local file, gzip-compressed by default
    Backup {
        /// Server profile to run pg_dump/mysqldump through (see: tooler server list)
        server: String,
        /// Local file path to write the dump to
        #[arg(long)]
        out: String,
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
        /// Skip gzip compression of the dump
        #[arg(long)]
        no_gzip: bool,
    },
    /// Restore a local dump file into a remote database (psql/mysql). Preview-only unless
    /// --confirm is passed
    Restore {
        /// Server profile to run psql/mysql through (see: tooler server list)
        server: String,
        /// Local dump file to restore (gzip-compressed input is auto-detected)
        #[arg(long = "in")]
        input: String,
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
        /// Actually run the restore (default is preview-only: shows what would run)
        #[arg(long)]
        confirm: bool,
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
            ConnOpts {
                env: env.as_deref(),
                engine: engine.as_deref(),
                host: host.as_deref(),
                port,
                database: database.as_deref(),
                user: user.as_deref(),
                password: password.as_deref(),
            },
            max_rows,
        ),
        DbSubcommand::Exec {
            server,
            sql,
            env,
            engine,
            host,
            port,
            database,
            user,
            password,
            confirm,
        } => exec(
            ctx,
            &server,
            &sql,
            ConnOpts {
                env: env.as_deref(),
                engine: engine.as_deref(),
                host: host.as_deref(),
                port,
                database: database.as_deref(),
                user: user.as_deref(),
                password: password.as_deref(),
            },
            confirm,
        ),
        DbSubcommand::Backup {
            server,
            out,
            env,
            engine,
            host,
            port,
            database,
            user,
            password,
            no_gzip,
        } => backup(
            ctx,
            &server,
            &out,
            ConnOpts {
                env: env.as_deref(),
                engine: engine.as_deref(),
                host: host.as_deref(),
                port,
                database: database.as_deref(),
                user: user.as_deref(),
                password: password.as_deref(),
            },
            !no_gzip,
        ),
        DbSubcommand::Restore {
            server,
            input,
            env,
            engine,
            host,
            port,
            database,
            user,
            password,
            confirm,
        } => restore(
            ctx,
            &server,
            &input,
            ConnOpts {
                env: env.as_deref(),
                engine: engine.as_deref(),
                host: host.as_deref(),
                port,
                database: database.as_deref(),
                user: user.as_deref(),
                password: password.as_deref(),
            },
            confirm,
        ),
    }
}

/// Database connection parameters shared by `query`, `backup`, and `restore`: either
/// `env` (a remote dotenv-style file to read DB_* credentials from) or the explicit
/// engine/host/port/database/user/password fields. `pub(crate)` so `tooler play`'s
/// `sync_db:` task type can reuse the same credential-resolution logic.
pub(crate) struct ConnOpts<'a> {
    pub(crate) env: Option<&'a str>,
    pub(crate) engine: Option<&'a str>,
    pub(crate) host: Option<&'a str>,
    pub(crate) port: Option<u16>,
    pub(crate) database: Option<&'a str>,
    pub(crate) user: Option<&'a str>,
    pub(crate) password: Option<&'a str>,
}

fn fail(json: bool, message: String) -> Result<()> {
    if json {
        println!("{}", serde_json::json!({ "error": message }));
        std::process::exit(1);
    }
    bail!(message);
}

pub(crate) fn resolve_credentials(server: &Server, opts: &ConnOpts) -> Result<Credentials> {
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

fn query(
    ctx: &Context,
    server_name: &str,
    sql: &str,
    opts: ConnOpts,
    max_rows: usize,
) -> Result<()> {
    let json = ctx.output == OutputFormat::Json;

    let server = resolve_server(ctx, server_name)?;
    let creds = match resolve_credentials(&server, &opts) {
        Ok(c) => c,
        Err(e) => return fail(json, format!("{e:#}")),
    };

    let (rows, truncated) = match db::run_query(&server, &creds, sql, max_rows) {
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

fn exec(ctx: &Context, server_name: &str, sql: &str, opts: ConnOpts, confirm: bool) -> Result<()> {
    let json = ctx.output == OutputFormat::Json;

    let server = resolve_server(ctx, server_name)?;
    let creds = match resolve_credentials(&server, &opts) {
        Ok(c) => c,
        Err(e) => return fail(json, format!("{e:#}")),
    };

    if !confirm {
        if json {
            println!(
                "{}",
                serde_json::json!({
                    "server": server_name,
                    "database": creds.database,
                    "sql": sql,
                    "confirmed": false,
                })
            );
            return Ok(());
        }
        println!(
            "Would run {} on {}@{} (database {}). Re-run with --confirm to apply.",
            sql.dimmed(),
            creds.user,
            server.host_target().cyan(),
            creds.database.bold(),
        );
        return Ok(());
    }

    let output = match db::run_exec(&server, &creds, sql) {
        Ok(o) => o,
        Err(e) => return fail(json, format!("{e:#}")),
    };

    if json {
        println!(
            "{}",
            serde_json::json!({
                "server": server_name,
                "database": creds.database,
                "sql": sql,
                "output": output,
            })
        );
        return Ok(());
    }
    println!("{} {}", "✓ ok".green().bold(), output.dimmed());
    Ok(())
}

fn backup(ctx: &Context, server_name: &str, out: &str, opts: ConnOpts, gzip: bool) -> Result<()> {
    let json = ctx.output == OutputFormat::Json;

    let server = resolve_server(ctx, server_name)?;
    let creds = match resolve_credentials(&server, &opts) {
        Ok(c) => c,
        Err(e) => return fail(json, format!("{e:#}")),
    };

    let command = db::dump_command(&creds, gzip);
    let bytes = match db::ssh_exec_capture_bytes(&server, &command) {
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
                "server": server_name,
                "database": creds.database,
                "out": out,
                "bytes": bytes.len(),
                "gzip": gzip,
            })
        );
        return Ok(());
    }
    println!(
        "{} dumped {} ({} bytes{}) from {} to {}",
        "✓".green().bold(),
        creds.database.cyan(),
        bytes.len(),
        if gzip { ", gzipped" } else { "" },
        server_name.cyan(),
        out.dimmed()
    );
    Ok(())
}

fn restore(
    ctx: &Context,
    server_name: &str,
    input: &str,
    opts: ConnOpts,
    confirm: bool,
) -> Result<()> {
    let json = ctx.output == OutputFormat::Json;

    let server = resolve_server(ctx, server_name)?;
    let creds = match resolve_credentials(&server, &opts) {
        Ok(c) => c,
        Err(e) => return fail(json, format!("{e:#}")),
    };
    let bytes = match std::fs::read(input) {
        Ok(b) => b,
        Err(e) => return fail(json, format!("reading {input}: {e}")),
    };
    let gzipped = is_gzip(&bytes);

    if !confirm {
        if json {
            println!(
                "{}",
                serde_json::json!({
                    "server": server_name,
                    "database": creds.database,
                    "in": input,
                    "bytes": bytes.len(),
                    "gzip": gzipped,
                    "confirmed": false,
                })
            );
            return Ok(());
        }
        println!(
            "Would restore {} bytes from {} into {}@{} (database {}{}). Re-run with --confirm to apply.",
            bytes.len(),
            input.dimmed(),
            creds.user,
            server.host_target().cyan(),
            creds.database.bold(),
            if gzipped { ", gzip-compressed" } else { "" }
        );
        return Ok(());
    }

    let command = db::restore_command(&creds, gzipped);
    let (_, stderr, success) = match db::ssh_exec_with_stdin(&server, &command, &bytes) {
        Ok(r) => r,
        Err(e) => return fail(json, format!("{e:#}")),
    };
    if !success {
        return fail(json, format!("restore failed: {}", stderr.trim()));
    }

    if json {
        println!(
            "{}",
            serde_json::json!({
                "server": server_name,
                "database": creds.database,
                "in": input,
                "restored": true,
            })
        );
        return Ok(());
    }
    println!(
        "{} restored {} into {} on {}",
        "✓".green().bold(),
        input.dimmed(),
        creds.database.cyan(),
        server_name.cyan()
    );
    Ok(())
}

/// Detects a gzip stream by its two-byte magic number (`1f 8b`), so restore doesn't
/// have to rely on the input file's extension.
fn is_gzip(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0x1f, 0x8b])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_gzip_detects_magic_bytes() {
        assert!(is_gzip(&[0x1f, 0x8b, 0x08, 0x00]));
        assert!(!is_gzip(b"-- SQL dump\n"));
        assert!(!is_gzip(&[]));
    }
}
