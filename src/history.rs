use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use std::{fs, path::PathBuf, time::Duration};

fn database_path() -> PathBuf {
    crate::config::tooler_dir().join("history.sqlite3")
}

fn open() -> Result<Connection> {
    let path = database_path();
    fs::create_dir_all(path.parent().expect("history database parent"))?;
    let connection = Connection::open(&path)
        .with_context(|| format!("opening history database {}", path.display()))?;
    connection.busy_timeout(Duration::from_secs(5))?;
    let schema_version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if schema_version > 1 {
        anyhow::bail!(
            "history database schema version {schema_version} is newer than this tooler supports"
        );
    }
    connection.execute_batch(
        "PRAGMA journal_mode = WAL;
         CREATE TABLE IF NOT EXISTS runs (
             id INTEGER PRIMARY KEY,
             started_at TEXT NOT NULL,
             command TEXT NOT NULL,
             outcome TEXT NOT NULL CHECK (outcome IN ('success', 'failed')),
             duration_ms INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS runs_started_at_idx ON runs(started_at DESC);",
    )?;
    if schema_version == 0 {
        connection.pragma_update(None, "user_version", 1)?;
    }
    set_private_permissions(&path)?;
    Ok(connection)
}

#[cfg(unix)]
fn set_private_permissions(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_permissions(_path: &std::path::Path) -> Result<()> {
    Ok(())
}

pub fn record(command: &str, success: bool, duration: Duration) -> Result<()> {
    let connection = open()?;
    connection.execute(
        "INSERT INTO runs (started_at, command, outcome, duration_ms) VALUES (?1, ?2, ?3, ?4)",
        params![
            chrono::Utc::now().to_rfc3339(),
            command,
            if success { "success" } else { "failed" },
            duration.as_millis().min(i64::MAX as u128) as i64,
        ],
    )?;
    connection.execute(
        "DELETE FROM runs WHERE id NOT IN (SELECT id FROM runs ORDER BY id DESC LIMIT 10000)",
        [],
    )?;
    Ok(())
}

pub struct HistoryArgs {
    pub limit: u32,
    pub command: Option<String>,
    pub failed: bool,
}

pub fn list(args: HistoryArgs, ctx: &crate::context::Context) -> Result<()> {
    let connection = open()?;
    let mut statement = connection.prepare(
        "SELECT id, started_at, command, outcome, duration_ms FROM runs
         WHERE (?1 IS NULL OR command = ?1) AND (?2 = 0 OR outcome = 'failed')
         ORDER BY id DESC LIMIT ?3",
    )?;
    let rows = statement.query_map(params![args.command, args.failed, args.limit], |row| {
        Ok(serde_json::json!({
            "id": row.get::<_, i64>(0)?,
            "started_at": row.get::<_, String>(1)?,
            "command": row.get::<_, String>(2)?,
            "outcome": row.get::<_, String>(3)?,
            "duration_ms": row.get::<_, i64>(4)?,
        }))
    })?;
    let runs = rows.collect::<std::result::Result<Vec<_>, _>>()?;
    if ctx.output == crate::output::OutputFormat::Json {
        println!("{}", serde_json::json!({"runs": runs}));
    } else if runs.is_empty() {
        println!("No command history yet.");
    } else {
        println!(
            "{:>5}  {:25} {:10} {:9} {:>10}",
            "ID", "STARTED", "COMMAND", "OUTCOME", "DURATION"
        );
        for run in runs {
            println!(
                "{:>5}  {:25} {:10} {:9} {:>7}ms",
                run["id"].as_i64().unwrap_or_default(),
                run["started_at"].as_str().unwrap_or(""),
                run["command"].as_str().unwrap_or(""),
                run["outcome"].as_str().unwrap_or(""),
                run["duration_ms"].as_i64().unwrap_or_default(),
            );
        }
    }
    Ok(())
}
