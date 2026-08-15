use crate::config::Server;
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Engine {
    MySql,
    Postgres,
}

impl Engine {
    pub fn default_port(self) -> u16 {
        match self {
            Engine::MySql => 3306,
            Engine::Postgres => 5432,
        }
    }
}

#[derive(Debug)]
pub struct Credentials {
    pub engine: Engine,
    pub host: String,
    pub port: u16,
    pub database: String,
    pub user: String,
    pub password: String,
}

/// Single-quotes `s` for safe interpolation into a remote shell command line
/// (escapes embedded `'` via the standard `'\''` trick). Shared by every
/// command that builds a remote command line from user-supplied values
/// (db, systemd, cron, logs) rather than each reimplementing it.
pub(crate) fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Runs `command` on `server` over SSH and returns (stdout, stderr, exit_success)
/// without bailing on a non-zero exit. Used where a non-zero exit is itself
/// meaningful output rather than a hard failure -- e.g. `systemctl status` on a
/// stopped unit, or `crontab -l` for a user with no crontab (exit 1, informative
/// stderr, not an error worth surfacing as one).
pub(crate) fn ssh_exec_capture_lenient(
    server: &Server,
    command: &str,
) -> Result<(String, String, bool)> {
    let output = std::process::Command::new("ssh")
        .args(server.ssh_args())
        .arg(server.host_target())
        .arg(command)
        .output()
        .context("Failed to launch ssh — is it installed?")?;
    Ok((
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
        output.status.success(),
    ))
}

/// Runs `command` on `server` over SSH and returns its stdout. Used for
/// reading a remote `.env`, invoking `psql`/`mysql` remotely, and by the
/// systemd/cron/logs commands, since this codebase's target hosting (shared
/// hosts like serv00.com) commonly disables `AllowTcpForwarding`, ruling out
/// an SSH tunnel + local DB driver in favor of running everything directly
/// on the remote host over a plain SSH exec.
pub(crate) fn ssh_exec_capture(server: &Server, command: &str) -> Result<String> {
    let (stdout, stderr, success) = ssh_exec_capture_lenient(server, command)?;
    if !success {
        bail!(
            "remote command failed on {}: {}",
            server.host_target(),
            stderr.trim()
        );
    }
    Ok(stdout)
}

/// Fetches a remote file's contents over SSH (`cat <path>`), e.g. a project's `.env`.
pub fn fetch_remote_file(server: &Server, path: &str) -> Result<String> {
    ssh_exec_capture(server, &format!("cat {}", shell_quote(path)))
}

/// Builds a sudo-invocation prefix for a remote command: empty when not using sudo,
/// `sudo ` when no password is supplied (relies on passwordless/NOPASSWD sudo), or a
/// piped `echo <pw> | sudo -S ` when a password is supplied. Shared by every command
/// that can run its remote action via sudo (systemd, ps).
pub(crate) fn sudo_prefix(sudo: bool, sudo_pass: Option<&str>) -> String {
    if !sudo {
        return String::new();
    }
    match sudo_pass {
        Some(pass) => format!("echo {} | sudo -S ", shell_quote(pass)),
        None => "sudo ".to_string(),
    }
}

/// Runs `command` on `server` over SSH and returns its raw stdout bytes, without lossy
/// UTF-8 conversion -- used for database dumps, which may be gzip-compressed binary
/// data rather than text.
pub(crate) fn ssh_exec_capture_bytes(server: &Server, command: &str) -> Result<Vec<u8>> {
    let output = std::process::Command::new("ssh")
        .args(server.ssh_args())
        .arg(server.host_target())
        .arg(command)
        .output()
        .context("Failed to launch ssh — is it installed?")?;
    if !output.status.success() {
        bail!(
            "remote command failed on {}: {}",
            server.host_target(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

/// Runs `command` on `server` over SSH, writing `input` to the remote command's stdin
/// (e.g. piping a local dump file into `psql`/`mysql` for a restore), and returns
/// (stdout, stderr, exit_success) without bailing on a non-zero exit -- mirrors
/// [`ssh_exec_capture_lenient`] so callers decide what a failure means.
pub(crate) fn ssh_exec_with_stdin(
    server: &Server,
    command: &str,
    input: &[u8],
) -> Result<(String, String, bool)> {
    use std::io::Write;
    use std::process::Stdio;

    let mut child = std::process::Command::new("ssh")
        .args(server.ssh_args())
        .arg(server.host_target())
        .arg(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to launch ssh — is it installed?")?;

    child
        .stdin
        .take()
        .expect("stdin was piped")
        .write_all(input)
        .context("writing dump data to ssh stdin")?;

    let output = child
        .wait_with_output()
        .context("waiting for ssh to finish")?;
    Ok((
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
        output.status.success(),
    ))
}

/// Builds the remote dump command for a database backup (`pg_dump`/`mysqldump`),
/// optionally piped through `gzip -c` so the transferred bytes are compressed.
pub fn dump_command(creds: &Credentials, gzip: bool) -> String {
    let dump = match creds.engine {
        Engine::Postgres => format!(
            "PGPASSWORD={} PGCONNECT_TIMEOUT=10 pg_dump -h {} -p {} -U {} -d {}",
            shell_quote(&creds.password),
            shell_quote(&creds.host),
            creds.port,
            shell_quote(&creds.user),
            shell_quote(&creds.database),
        ),
        Engine::MySql => format!(
            // mysqldump has no --connect-timeout flag (unlike the `mysql` client used by
            // restore_command below) — passing one makes it exit immediately with
            // "unknown variable", which the `| gzip -c` pipe then silently turns into an
            // empty-but-"successful" dump.
            "MYSQL_PWD={} mysqldump --single-transaction -h {} -P {} -u {} {}",
            shell_quote(&creds.password),
            shell_quote(&creds.host),
            creds.port,
            shell_quote(&creds.user),
            shell_quote(&creds.database),
        ),
    };
    if gzip {
        format!("{dump} | gzip -c")
    } else {
        dump
    }
}

/// Builds the remote restore command (`psql`/`mysql`) that reads a dump from stdin.
/// When `gzipped_input` is set, prefixes with `gunzip -c |` to decompress the piped
/// bytes before they reach the database client.
pub fn restore_command(creds: &Credentials, gzipped_input: bool) -> String {
    let load = match creds.engine {
        Engine::Postgres => format!(
            "PGPASSWORD={} PGCONNECT_TIMEOUT=10 psql -h {} -p {} -U {} -d {}",
            shell_quote(&creds.password),
            shell_quote(&creds.host),
            creds.port,
            shell_quote(&creds.user),
            shell_quote(&creds.database),
        ),
        Engine::MySql => format!(
            "MYSQL_PWD={} mysql --connect-timeout=10 -h {} -P {} -u {} {}",
            shell_quote(&creds.password),
            shell_quote(&creds.host),
            creds.port,
            shell_quote(&creds.user),
            shell_quote(&creds.database),
        ),
    };
    if gzipped_input {
        format!("gunzip -c | {load}")
    } else {
        load
    }
}

/// Parses `KEY=value` lines (dotenv format: `#` comments, optional quotes).
pub fn parse_dotenv(content: &str) -> HashMap<String, String> {
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
    map
}

/// Extracts DB connection info from a Laravel/dotenv-style env map
/// (`DB_CONNECTION`, `DB_HOST`, `DB_PORT`, `DB_DATABASE`, `DB_USERNAME`, `DB_PASSWORD`).
pub fn credentials_from_env(env: &HashMap<String, String>) -> Result<Credentials> {
    let connection = env
        .get("DB_CONNECTION")
        .map(|s| s.to_lowercase())
        .unwrap_or_default();
    let engine = match connection.as_str() {
        "mysql" | "mariadb" => Engine::MySql,
        "pgsql" | "postgres" | "postgresql" => Engine::Postgres,
        other => bail!(
            "Unrecognized or missing DB_CONNECTION ('{other}'); expected mysql/mariadb or pgsql/postgres"
        ),
    };

    let host = env
        .get("DB_HOST")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("DB_HOST not found in env file"))?;
    let port = env
        .get("DB_PORT")
        .and_then(|p| p.parse().ok())
        .unwrap_or_else(|| engine.default_port());
    let database = env
        .get("DB_DATABASE")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("DB_DATABASE not found in env file"))?;
    let user = env
        .get("DB_USERNAME")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("DB_USERNAME not found in env file"))?;
    let password = env.get("DB_PASSWORD").cloned().unwrap_or_default();

    Ok(Credentials {
        engine,
        host,
        port,
        database,
        user,
        password,
    })
}

/// Reads a remote env file over SSH and extracts DB credentials from it.
pub fn credentials_from_remote_env(server: &Server, path: &str) -> Result<Credentials> {
    let content = fetch_remote_file(server, path)?;
    let env = parse_dotenv(&content);
    credentials_from_env(&env).with_context(|| {
        format!(
            "parsing DB credentials from {path} on {}",
            server.host_target()
        )
    })
}

/// Rejects anything but a single read-only statement, since query results are meant
/// for reporting, not for driving arbitrary writes against a production database
/// (especially when this is invoked by an LLM through the MCP tool). Returns the
/// statement with any single trailing `;` stripped.
fn ensure_read_only(sql: &str) -> Result<&str> {
    let trimmed = sql.trim();
    let body = trimmed.strip_suffix(';').unwrap_or(trimmed).trim();
    if body.contains(';') {
        bail!("Only a single statement is allowed");
    }
    let first_word = body
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_uppercase();
    const ALLOWED: &[&str] = &["SELECT", "SHOW", "EXPLAIN", "WITH", "DESCRIBE", "DESC"];
    if !ALLOWED.contains(&first_word.as_str()) {
        bail!(
            "Only read-only queries are allowed ({}); got '{first_word}'",
            ALLOWED.join("/")
        );
    }
    Ok(body)
}

fn truncate(rows: Vec<Value>, max_rows: usize) -> (Vec<Value>, bool) {
    let truncated = rows.len() > max_rows;
    (rows.into_iter().take(max_rows).collect(), truncated)
}

/// Runs `body` on Postgres via `psql`, having Postgres itself aggregate the result
/// into one JSON array (`row_to_json`/`json_agg`) so parsing is exact — no
/// TSV/NULL-convention guessing.
fn run_postgres_query(server: &Server, creds: &Credentials, body: &str) -> Result<Vec<Value>> {
    let wrapped =
        format!("SELECT COALESCE((SELECT json_agg(row_to_json(t)) FROM ({body}) t), '[]'::json)");
    let command = format!(
        "PGPASSWORD={} PGCONNECT_TIMEOUT=10 psql -h {} -p {} -U {} -d {} -tAX -c {}",
        shell_quote(&creds.password),
        shell_quote(&creds.host),
        creds.port,
        shell_quote(&creds.user),
        shell_quote(&creds.database),
        shell_quote(&wrapped),
    );
    let output = ssh_exec_capture(server, &command)?;
    let value: Value = serde_json::from_str(output.trim())
        .with_context(|| format!("parsing psql JSON output: {}", output.trim()))?;
    Ok(value.as_array().cloned().unwrap_or_default())
}

/// Runs `body` on MySQL/MariaDB via the `mysql` CLI in tab-separated `--batch` mode
/// and parses rows from the header + data lines. Every value comes back as a JSON
/// string (except NULL) rather than guessing numeric types from text, since the
/// `mysql` client gives us no server-side type info here — downstream consumers
/// (tooler report) treat cell values as text either way.
fn run_mysql_query(server: &Server, creds: &Credentials, body: &str) -> Result<Vec<Value>> {
    let command = format!(
        "MYSQL_PWD={} mysql --batch --raw --connect-timeout=10 -h {} -P {} -u {} -D {} -e {}",
        shell_quote(&creds.password),
        shell_quote(&creds.host),
        creds.port,
        shell_quote(&creds.user),
        shell_quote(&creds.database),
        shell_quote(body),
    );
    let output = ssh_exec_capture(server, &command)?;
    Ok(parse_mysql_tsv(&output))
}

fn parse_mysql_tsv(output: &str) -> Vec<Value> {
    let mut lines = output.lines();
    let Some(header) = lines.next() else {
        return Vec::new();
    };
    let columns: Vec<&str> = header.split('\t').collect();
    lines
        .map(|line| {
            let cells: Vec<&str> = line.split('\t').collect();
            let mut obj = serde_json::Map::new();
            for (i, col) in columns.iter().enumerate() {
                let raw = cells.get(i).copied().unwrap_or("");
                let value = if raw == "NULL" || raw == "\\N" {
                    Value::Null
                } else {
                    Value::String(raw.to_string())
                };
                obj.insert((*col).to_string(), value);
            }
            Value::Object(obj)
        })
        .collect()
}

/// Runs a single read-only query against `creds`'s database by invoking `psql`
/// or `mysql` directly on `server` over SSH, returning up to `max_rows` rows as
/// JSON objects plus whether the result was truncated.
pub fn run_query(
    server: &Server,
    creds: &Credentials,
    sql: &str,
    max_rows: usize,
) -> Result<(Vec<Value>, bool)> {
    let body = ensure_read_only(sql)?;
    let rows = match creds.engine {
        Engine::Postgres => run_postgres_query(server, creds, body)?,
        Engine::MySql => run_mysql_query(server, creds, body)?,
    };
    Ok(truncate(rows, max_rows))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_dotenv_handles_comments_quotes_and_blank_lines() {
        let content = "\n# a comment\nDB_HOST=localhost\nDB_DATABASE=\"my_db\"\nDB_USER='bob'\n";
        let env = parse_dotenv(content);
        assert_eq!(env.get("DB_HOST"), Some(&"localhost".to_string()));
        assert_eq!(env.get("DB_DATABASE"), Some(&"my_db".to_string()));
        assert_eq!(env.get("DB_USER"), Some(&"bob".to_string()));
        assert_eq!(env.len(), 3);
    }

    fn laravel_env(connection: &str) -> HashMap<String, String> {
        HashMap::from([
            ("DB_CONNECTION".to_string(), connection.to_string()),
            ("DB_HOST".to_string(), "db.internal".to_string()),
            ("DB_DATABASE".to_string(), "shop".to_string()),
            ("DB_USERNAME".to_string(), "app".to_string()),
            ("DB_PASSWORD".to_string(), "secret".to_string()),
        ])
    }

    #[test]
    fn credentials_from_env_maps_mysql_and_default_port() {
        let creds = credentials_from_env(&laravel_env("mysql")).unwrap();
        assert_eq!(creds.engine, Engine::MySql);
        assert_eq!(creds.port, 3306);
        assert_eq!(creds.database, "shop");
    }

    #[test]
    fn credentials_from_env_maps_pgsql_and_default_port() {
        let creds = credentials_from_env(&laravel_env("pgsql")).unwrap();
        assert_eq!(creds.engine, Engine::Postgres);
        assert_eq!(creds.port, 5432);
    }

    #[test]
    fn credentials_from_env_respects_explicit_port() {
        let mut env = laravel_env("pgsql");
        env.insert("DB_PORT".to_string(), "6543".to_string());
        let creds = credentials_from_env(&env).unwrap();
        assert_eq!(creds.port, 6543);
    }

    #[test]
    fn credentials_from_env_rejects_unknown_connection() {
        let err = credentials_from_env(&laravel_env("sqlsrv")).unwrap_err();
        assert!(err.to_string().contains("sqlsrv"));
    }

    #[test]
    fn credentials_from_env_requires_host() {
        let mut env = laravel_env("mysql");
        env.remove("DB_HOST");
        assert!(credentials_from_env(&env).is_err());
    }

    #[test]
    fn ensure_read_only_accepts_select_and_strips_semicolon() {
        assert_eq!(ensure_read_only("SELECT 1;").unwrap(), "SELECT 1");
        assert_eq!(
            ensure_read_only("  select * from t  ").unwrap(),
            "select * from t"
        );
    }

    #[test]
    fn ensure_read_only_accepts_all_allowed_verbs() {
        for verb in [
            "SELECT 1",
            "SHOW TABLES",
            "EXPLAIN SELECT 1",
            "WITH x AS (SELECT 1) SELECT * FROM x",
            "DESCRIBE users",
            "DESC users",
        ] {
            assert!(
                ensure_read_only(verb).is_ok(),
                "expected {verb} to be allowed"
            );
        }
    }

    #[test]
    fn ensure_read_only_rejects_writes() {
        for verb in [
            "INSERT INTO t VALUES (1)",
            "UPDATE t SET x=1",
            "DELETE FROM t",
            "DROP TABLE t",
        ] {
            assert!(
                ensure_read_only(verb).is_err(),
                "expected {verb} to be rejected"
            );
        }
    }

    #[test]
    fn ensure_read_only_rejects_multiple_statements() {
        assert!(ensure_read_only("SELECT 1; DROP TABLE t;").is_err());
    }

    #[test]
    fn parse_mysql_tsv_handles_null_and_multiple_rows() {
        let output = "id\tname\n1\tAlice\n2\t\\N\n3\tNULL\n";
        let rows = parse_mysql_tsv(output);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0]["id"], Value::String("1".to_string()));
        assert_eq!(rows[0]["name"], Value::String("Alice".to_string()));
        assert_eq!(rows[1]["name"], Value::Null);
        assert_eq!(rows[2]["name"], Value::Null);
    }

    #[test]
    fn parse_mysql_tsv_empty_output_yields_no_rows() {
        assert!(parse_mysql_tsv("").is_empty());
    }

    #[test]
    fn truncate_reports_when_capped() {
        let rows: Vec<Value> = (0..5).map(Value::from).collect();
        let (kept, truncated) = truncate(rows, 3);
        assert_eq!(kept.len(), 3);
        assert!(truncated);

        let rows: Vec<Value> = (0..2).map(Value::from).collect();
        let (kept, truncated) = truncate(rows, 3);
        assert_eq!(kept.len(), 2);
        assert!(!truncated);
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote("plain"), "'plain'");
    }

    #[test]
    fn sudo_prefix_empty_when_not_sudo() {
        assert_eq!(sudo_prefix(false, Some("pw")), "");
    }

    #[test]
    fn sudo_prefix_plain_sudo_without_password() {
        assert_eq!(sudo_prefix(true, None), "sudo ");
    }

    #[test]
    fn sudo_prefix_pipes_quoted_password() {
        assert_eq!(
            sudo_prefix(true, Some("it's")),
            "echo 'it'\\''s' | sudo -S "
        );
    }

    fn pg_creds() -> Credentials {
        Credentials {
            engine: Engine::Postgres,
            host: "db.internal".to_string(),
            port: 5432,
            database: "shop".to_string(),
            user: "app".to_string(),
            password: "secret".to_string(),
        }
    }

    fn mysql_creds() -> Credentials {
        Credentials {
            engine: Engine::MySql,
            host: "db.internal".to_string(),
            port: 3306,
            database: "shop".to_string(),
            user: "app".to_string(),
            password: "secret".to_string(),
        }
    }

    #[test]
    fn dump_command_postgres_with_gzip() {
        let cmd = dump_command(&pg_creds(), true);
        assert!(cmd.starts_with("PGPASSWORD='secret' PGCONNECT_TIMEOUT=10 pg_dump"));
        assert!(cmd.ends_with("| gzip -c"));
    }

    #[test]
    fn dump_command_mysql_without_gzip() {
        let cmd = dump_command(&mysql_creds(), false);
        assert!(cmd.starts_with("MYSQL_PWD='secret' mysqldump"));
        assert!(!cmd.contains("gzip"));
    }

    #[test]
    fn restore_command_postgres_with_gzipped_input() {
        let cmd = restore_command(&pg_creds(), true);
        assert!(cmd.starts_with("gunzip -c | PGPASSWORD='secret'"));
        assert!(cmd.contains("psql"));
    }

    #[test]
    fn restore_command_mysql_without_gzipped_input() {
        let cmd = restore_command(&mysql_creds(), false);
        assert!(cmd.starts_with("MYSQL_PWD='secret' mysql"));
        assert!(!cmd.contains("gunzip"));
    }
}
