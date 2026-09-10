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

/// Runs `command` on `server` over SSH and returns (stdout, stderr, exit_success,
/// exit_code) without bailing on a non-zero exit. Used where a non-zero exit is itself
/// meaningful output rather than a hard failure -- e.g. `systemctl status` on a
/// stopped unit, or `crontab -l` for a user with no crontab (exit 1, informative
/// stderr, not an error worth surfacing as one). `exit_code` is `None` only when the
/// process was killed by a signal rather than exiting normally (Unix-only distinction --
/// see `std::process::ExitStatus::code`).
pub(crate) fn ssh_exec_capture_lenient(
    server: &Server,
    command: &str,
) -> Result<(String, String, bool, Option<i32>)> {
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
        output.status.code(),
    ))
}

/// Puts `cmd` in its own process group on Unix (a no-op elsewhere) so a later
/// `kill_process_group` can take out the whole subtree, not just the immediate child --
/// e.g. `sh -c "sleep 5"`, where the shell doesn't always exec-replace itself, leaving
/// `sleep` as its own child rather than the same process. Killing only the shell's pid
/// would otherwise orphan `sleep`, which keeps any inherited stdout/stderr pipe open --
/// a piped caller (`assert_cmd`, or this crate's own `capture: true` path) then blocks
/// on EOF until the orphan finishes on its own, defeating the whole point of a timeout.
pub(crate) fn isolate_process_group(cmd: &mut std::process::Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
}

/// Kills `child`'s entire process tree: the whole process group via `kill(2)` on Unix
/// (see `isolate_process_group`), or Windows' own built-in recursive process-tree killer
/// (`taskkill /T /F`, present on every Windows install — no new dependency, the same
/// "shell out to an OS-provided binary" pattern this crate already uses for `ssh`) on
/// Windows — confirmed necessary there too: the same "kill only cuts the immediate `sh`/
/// `cmd` child, orphaning a grandchild that keeps the inherited output pipe open" shape
/// this fixes on Linux was observed intermittently on Windows CI as well. Falls back to
/// the plain single-process `Child::kill()` on any other target. Declares `kill(2)`
/// itself on Unix rather than adding a `libc` dependency for one syscall.
pub(crate) fn kill_process_group(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        unsafe extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        const SIGKILL: i32 = 9;
        unsafe {
            kill(-(child.id() as i32), SIGKILL);
        }
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/T", "/F", "/PID", &child.id().to_string()])
            .output();
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = child.kill();
    }
}

/// Spawns `cmd`, killing it if `timeout` (seconds) elapses first, and returns
/// (stdout, stderr, exit_success) — the same poll+kill mechanism `run:`'s own
/// `commands::play::run_with_timeout` uses, generalized here (both streams captured via
/// their own reader thread, so neither can block on a full pipe buffer while the main
/// thread polls) so `ssh_exec_*`'s timeout-aware variants can share it. `None` just
/// waits for the command to finish, same as a plain `.output()` call.
fn run_with_deadline(
    mut cmd: std::process::Command,
    timeout: Option<u64>,
) -> Result<(String, String, bool, Option<i32>)> {
    use std::time::{Duration, Instant};
    isolate_process_group(&mut cmd);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let mut child = cmd
        .spawn()
        .context("Failed to launch ssh — is it installed?")?;
    let mut stdout_pipe = child.stdout.take().expect("stdout was piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr was piped");
    let stdout_reader = std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = String::new();
        let _ = stdout_pipe.read_to_string(&mut buf);
        buf
    });
    let stderr_reader = std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = String::new();
        let _ = stderr_pipe.read_to_string(&mut buf);
        buf
    });

    let deadline = timeout.map(|secs| Instant::now() + Duration::from_secs(secs));
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if let Some(dl) = deadline
            && Instant::now() >= dl
        {
            kill_process_group(&mut child);
            let _ = child.wait();
            bail!("ssh command timed out after {}s", timeout.unwrap());
        }
        std::thread::sleep(Duration::from_millis(100));
    };

    Ok((
        stdout_reader.join().unwrap_or_default(),
        stderr_reader.join().unwrap_or_default(),
        status.success(),
        status.code(),
    ))
}

/// Same as `ssh_exec_capture_lenient`, but kills the remote command if `timeout`
/// (seconds) elapses first — see `run_with_deadline`. `None` behaves identically to
/// `ssh_exec_capture_lenient` itself.
pub(crate) fn ssh_exec_capture_lenient_with_timeout(
    server: &Server,
    command: &str,
    timeout: Option<u64>,
) -> Result<(String, String, bool, Option<i32>)> {
    let mut cmd = std::process::Command::new("ssh");
    cmd.args(server.ssh_args())
        .arg(server.host_target())
        .arg(command);
    run_with_deadline(cmd, timeout)
}

/// Runs `command` on `server` over SSH and returns its stdout. Used for
/// reading a remote `.env`, invoking `psql`/`mysql` remotely, and by the
/// systemd/cron/logs commands, since this codebase's target hosting (shared
/// hosts like serv00.com) commonly disables `AllowTcpForwarding`, ruling out
/// an SSH tunnel + local DB driver in favor of running everything directly
/// on the remote host over a plain SSH exec.
pub(crate) fn ssh_exec_capture(server: &Server, command: &str) -> Result<String> {
    let (stdout, stderr, success, _) = ssh_exec_capture_lenient(server, command)?;
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

/// Like `run_with_deadline`, but captures stdout as raw bytes (a `sync_db:` dump may be
/// gzip-compressed binary, not text) and optionally writes `stdin_data` to the child's
/// stdin right after spawn, on its own thread -- so a child that doesn't promptly read
/// stdin can't deadlock the write against a full pipe buffer, same reasoning the
/// stdout/stderr reader threads below already rely on. Shared by
/// `ssh_exec_capture_bytes_with_timeout`/`ssh_exec_with_stdin_with_timeout`.
fn run_with_deadline_bytes(
    mut cmd: std::process::Command,
    timeout: Option<u64>,
    stdin_data: Option<&[u8]>,
) -> Result<(Vec<u8>, String, bool, Option<i32>)> {
    use std::time::{Duration, Instant};
    isolate_process_group(&mut cmd);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    if stdin_data.is_some() {
        cmd.stdin(std::process::Stdio::piped());
    }
    let mut child = cmd
        .spawn()
        .context("Failed to launch ssh — is it installed?")?;

    if let Some(data) = stdin_data {
        let mut stdin_pipe = child.stdin.take().expect("stdin was piped");
        let data = data.to_vec();
        std::thread::spawn(move || {
            use std::io::Write;
            let _ = stdin_pipe.write_all(&data);
            // stdin_pipe drops here, closing it -- signals EOF to the child.
        });
    }

    let mut stdout_pipe = child.stdout.take().expect("stdout was piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr was piped");
    let stdout_reader = std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buf);
        buf
    });
    let stderr_reader = std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = String::new();
        let _ = stderr_pipe.read_to_string(&mut buf);
        buf
    });

    let deadline = timeout.map(|secs| Instant::now() + Duration::from_secs(secs));
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if let Some(dl) = deadline
            && Instant::now() >= dl
        {
            kill_process_group(&mut child);
            let _ = child.wait();
            bail!("ssh command timed out after {}s", timeout.unwrap());
        }
        std::thread::sleep(Duration::from_millis(100));
    };

    Ok((
        stdout_reader.join().unwrap_or_default(),
        stderr_reader.join().unwrap_or_default(),
        status.success(),
        status.code(),
    ))
}

/// Same as `ssh_exec_capture_bytes`, but kills the remote command if `timeout`
/// (seconds) elapses first — see `run_with_deadline_bytes`. `None` behaves identically
/// to `ssh_exec_capture_bytes` itself.
pub(crate) fn ssh_exec_capture_bytes_with_timeout(
    server: &Server,
    command: &str,
    timeout: Option<u64>,
) -> Result<Vec<u8>> {
    let mut cmd = std::process::Command::new("ssh");
    cmd.args(server.ssh_args())
        .arg(server.host_target())
        .arg(command);
    let (bytes, stderr, success, _) = run_with_deadline_bytes(cmd, timeout, None)?;
    if !success {
        bail!(
            "remote command failed on {}: {}",
            server.host_target(),
            stderr.trim()
        );
    }
    Ok(bytes)
}

/// Same as `ssh_exec_with_stdin`, but kills the remote command if `timeout` (seconds)
/// elapses first — see `run_with_deadline_bytes`. `None` behaves identically to
/// `ssh_exec_with_stdin` itself.
pub(crate) fn ssh_exec_with_stdin_with_timeout(
    server: &Server,
    command: &str,
    input: &[u8],
    timeout: Option<u64>,
) -> Result<(String, String, bool)> {
    let mut cmd = std::process::Command::new("ssh");
    cmd.args(server.ssh_args())
        .arg(server.host_target())
        .arg(command);
    let (stdout_bytes, stderr, success, _) = run_with_deadline_bytes(cmd, timeout, Some(input))?;
    Ok((
        String::from_utf8_lossy(&stdout_bytes).into_owned(),
        stderr,
        success,
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

/// Options for a `db_load:` bulk load — see `load_command`/`run_load`.
pub struct LoadOpts<'a> {
    pub table: &'a str,
    pub columns: Option<&'a [String]>,
    /// `TRUNCATE` the table before loading (a separate statement, same connection).
    pub truncate: bool,
    /// MySQL only: `LOAD DATA REPLACE` — rows with a duplicate PK/unique key overwrite
    /// the existing row instead of erroring. Postgres has no equivalent for `\copy`.
    pub replace: bool,
    /// The input's first line is a header row (skipped on load).
    pub has_header: bool,
    /// Field delimiter — `','` (CSV) or `'\t'` (TSV).
    pub delimiter: char,
}

/// A SQL identifier (table or column name) — letters, digits, `_`, and `.` (for
/// `schema.table`), starting with a letter or `_`. These can't be parameterized in
/// `\copy`/`LOAD DATA` and must never come from untrusted data, so anything else is
/// rejected outright.
fn validate_identifier(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.');
    if !ok {
        bail!(
            "invalid SQL identifier '{name}' (letters/digits/_/. only, must start with a letter or _)"
        );
    }
    Ok(())
}

/// Validates `opts`'s identifiers and returns the ` (col, col)` suffix (empty when no
/// explicit column list). Shared by both engines' statement builders.
fn load_col_list(opts: &LoadOpts) -> Result<String> {
    validate_identifier(opts.table)?;
    match opts.columns {
        None => Ok(String::new()),
        Some(cols) => {
            for c in cols {
                validate_identifier(c)?;
            }
            Ok(format!(" ({})", cols.join(", ")))
        }
    }
}

/// The raw `psql -c` argument(s) for a Postgres load — a `TRUNCATE` first (when asked),
/// then the `\copy ... FROM STDIN` itself. Not shell-quoted; `load_command` does that.
pub(crate) fn pg_load_statements(opts: &LoadOpts) -> Result<Vec<String>> {
    if opts.replace {
        bail!(
            "db_load: mode upsert isn't supported for Postgres yet — load into a staging \
             table and MERGE by hand"
        );
    }
    let col_list = load_col_list(opts)?;
    let delim = if opts.delimiter == '\t' {
        "E'\\t'".to_string()
    } else {
        format!("'{}'", opts.delimiter)
    };
    let copy = format!(
        "\\copy {}{} FROM STDIN WITH (FORMAT csv, HEADER {}, DELIMITER {})",
        opts.table, col_list, opts.has_header, delim,
    );
    let mut out = Vec::new();
    if opts.truncate {
        out.push(format!("TRUNCATE {}", opts.table));
    }
    out.push(copy);
    Ok(out)
}

/// The raw `mysql -e` statement for a MySQL load. Not shell-quoted.
pub(crate) fn mysql_load_statement(opts: &LoadOpts) -> Result<String> {
    let col_list = load_col_list(opts)?;
    let delim = if opts.delimiter == '\t' { "\\t" } else { "," };
    let mut stmt = String::new();
    if opts.truncate {
        stmt.push_str(&format!("TRUNCATE {}; ", opts.table));
    }
    stmt.push_str("LOAD DATA LOCAL INFILE '/dev/stdin' ");
    if opts.replace {
        stmt.push_str("REPLACE ");
    }
    stmt.push_str(&format!(
        "INTO TABLE {} FIELDS TERMINATED BY '{}' OPTIONALLY ENCLOSED BY '\"' \
         LINES TERMINATED BY '\\n'",
        opts.table, delim
    ));
    if opts.has_header {
        stmt.push_str(" IGNORE 1 LINES");
    }
    stmt.push_str(&col_list);
    Ok(stmt)
}

/// Builds the remote command that reads CSV/TSV bytes from stdin and bulk-loads them
/// into `opts.table`. Postgres: `psql -c "TRUNCATE ..." -c "\copy ... FROM STDIN"`.
/// MySQL: `mysql --local-infile=1 -e "[TRUNCATE ...;] LOAD DATA LOCAL INFILE
/// '/dev/stdin' [REPLACE] INTO TABLE ..."`. Errors on an invalid identifier or
/// `replace: true` against Postgres.
pub fn load_command(creds: &Credentials, opts: &LoadOpts) -> Result<String> {
    match creds.engine {
        Engine::Postgres => {
            let mut cmd = format!(
                "PGPASSWORD={} PGCONNECT_TIMEOUT=10 psql -v ON_ERROR_STOP=1 -h {} -p {} -U {} -d {}",
                shell_quote(&creds.password),
                shell_quote(&creds.host),
                creds.port,
                shell_quote(&creds.user),
                shell_quote(&creds.database),
            );
            for stmt in pg_load_statements(opts)? {
                cmd.push_str(&format!(" -c {}", shell_quote(&stmt)));
            }
            Ok(cmd)
        }
        Engine::MySql => Ok(format!(
            "MYSQL_PWD={} mysql --local-infile=1 --connect-timeout=10 -h {} -P {} -u {} -D {} -e {}",
            shell_quote(&creds.password),
            shell_quote(&creds.host),
            creds.port,
            shell_quote(&creds.user),
            shell_quote(&creds.database),
            shell_quote(&mysql_load_statement(opts)?),
        )),
    }
}

/// Streams `data` (CSV/TSV bytes) into `opts.table` on `creds`'s database over one SSH
/// connection (`ssh_exec_with_stdin`), via `load_command`. Returns the client's summary
/// output trimmed (`COPY 42` on Postgres; MySQL's non-batch client usually prints
/// nothing here, so an empty string means "succeeded" — follow up with a `db_query:` on
/// `ROW_COUNT()` if the exact count matters).
pub fn run_load(
    server: &Server,
    creds: &Credentials,
    data: &[u8],
    opts: &LoadOpts,
) -> Result<String> {
    let command = load_command(creds, opts)?;
    let (stdout, stderr, ok) = ssh_exec_with_stdin(server, &command, data)?;
    if !ok {
        bail!("db_load failed: {}", stderr.trim());
    }
    Ok(stdout.trim().to_string())
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
    // MySQL's `SELECT ... INTO OUTFILE '/path'`/`INTO DUMPFILE` still starts with
    // SELECT (so passes the check above) but writes an arbitrary file on the server
    // when the connecting user has the FILE privilege -- exactly the kind of write
    // this function exists to block. No legitimate reporting query needs `INTO
    // OUTFILE`/`INTO DUMPFILE`, so reject it outright rather than trying to allow-list
    // safe uses of `INTO`.
    let upper = body.to_uppercase();
    if upper.contains("INTO OUTFILE") || upper.contains("INTO DUMPFILE") {
        bail!("INTO OUTFILE/DUMPFILE is not allowed (writes a file on the database server)");
    }
    Ok(body)
}

/// Rejects anything but a single INSERT/UPDATE/DELETE statement -- deliberately narrower
/// than "any SQL": `db_exec:`/`tooler db exec` exist for exactly what an RPA-style
/// process needs (mark a row processed, update a status, insert a log entry), not
/// general-purpose SQL execution. DDL (DROP/TRUNCATE/ALTER/CREATE) and everything else
/// stays out of reach here, same as it's out of reach of `ensure_read_only`.
fn ensure_write_only(sql: &str) -> Result<&str> {
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
    const ALLOWED: &[&str] = &["INSERT", "UPDATE", "DELETE"];
    if !ALLOWED.contains(&first_word.as_str()) {
        bail!(
            "Only INSERT/UPDATE/DELETE are allowed by db_exec: ({}); got '{first_word}'. \
             Use db_query: for reads, or run this by hand if it's really DDL.",
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

/// Runs a single INSERT/UPDATE/DELETE statement (see `ensure_write_only`) against
/// `creds`'s database, invoking `psql`/`mysql` directly on `server` over SSH exactly
/// like `run_query` does, but without the `json_agg` wrapping (that's SELECT-only) --
/// returns whatever the client itself printed, trimmed. Postgres's `psql -c` prints a
/// command-tag line by default (`UPDATE 3`, `INSERT 0 1`, `DELETE 2`), so that comes
/// back usable as-is; MySQL's non-batch `mysql -e` prints a "Query OK, N rows affected"
/// line interactively, but that specific line is a client message, not query output, so
/// it may come back empty depending on the exact mysql client build -- treat a MySQL
/// result as "statement succeeded" and use a follow-up `db_query: {sql: "SELECT
/// ROW_COUNT()"}` task if the exact affected-row count matters.
pub fn run_exec(server: &Server, creds: &Credentials, sql: &str) -> Result<String> {
    let body = ensure_write_only(sql)?;
    let output = match creds.engine {
        Engine::Postgres => run_postgres_exec(server, creds, body)?,
        Engine::MySql => run_mysql_exec(server, creds, body)?,
    };
    Ok(output.trim().to_string())
}

/// Runs `body` on Postgres via `psql`, no `-tAX`/`json_agg` wrapping -- lets the normal
/// command-tag output (`UPDATE 3`, ...) through so `run_exec` can hand it back as-is.
fn run_postgres_exec(server: &Server, creds: &Credentials, body: &str) -> Result<String> {
    let command = format!(
        "PGPASSWORD={} PGCONNECT_TIMEOUT=10 psql -h {} -p {} -U {} -d {} -c {}",
        shell_quote(&creds.password),
        shell_quote(&creds.host),
        creds.port,
        shell_quote(&creds.user),
        shell_quote(&creds.database),
        shell_quote(body),
    );
    ssh_exec_capture(server, &command)
}

/// Runs `body` on MySQL/MariaDB via the `mysql` CLI -- deliberately without
/// `run_mysql_query`'s `--batch --raw` (those exist for TSV-parsing a SELECT result, not
/// relevant here).
fn run_mysql_exec(server: &Server, creds: &Credentials, body: &str) -> Result<String> {
    let command = format!(
        "MYSQL_PWD={} mysql --connect-timeout=10 -h {} -P {} -u {} -D {} -e {}",
        shell_quote(&creds.password),
        shell_quote(&creds.host),
        creds.port,
        shell_quote(&creds.user),
        shell_quote(&creds.database),
        shell_quote(body),
    );
    ssh_exec_capture(server, &command)
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
    fn run_with_deadline_kills_a_hung_command_and_bails_quickly() {
        let mut cmd = std::process::Command::new("sleep");
        cmd.arg("5");
        let start = std::time::Instant::now();
        let err = run_with_deadline(cmd, Some(1)).unwrap_err();
        let elapsed = start.elapsed();
        assert!(err.to_string().contains("timed out"), "error was: {err}");
        assert!(
            elapsed < std::time::Duration::from_secs(3),
            "expected the timeout to cut this short, took {elapsed:?}"
        );
    }

    #[test]
    fn run_with_deadline_without_a_timeout_waits_for_completion() {
        let mut cmd = std::process::Command::new("echo");
        cmd.arg("hi");
        let (stdout, _stderr, success, _code) = run_with_deadline(cmd, None).unwrap();
        assert!(success);
        assert_eq!(stdout.trim(), "hi");
    }

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
    fn ensure_read_only_rejects_into_outfile_and_dumpfile() {
        for sql in [
            "SELECT * FROM users INTO OUTFILE '/tmp/x'",
            "select * from users into outfile '/tmp/x'",
            "SELECT secret INTO DUMPFILE '/var/www/html/shell.php'",
        ] {
            assert!(
                ensure_read_only(sql).is_err(),
                "expected {sql} to be rejected"
            );
        }
    }

    #[test]
    fn ensure_read_only_rejects_multiple_statements() {
        assert!(ensure_read_only("SELECT 1; DROP TABLE t;").is_err());
    }

    #[test]
    fn ensure_write_only_accepts_insert_update_delete() {
        for sql in [
            "INSERT INTO t (x) VALUES (1)",
            "update t set x=1 where id=1",
            "DELETE FROM t WHERE id=1",
        ] {
            assert!(
                ensure_write_only(sql).is_ok(),
                "expected {sql} to be allowed"
            );
        }
        assert_eq!(
            ensure_write_only("UPDATE t SET x=1;").unwrap(),
            "UPDATE t SET x=1"
        );
    }

    #[test]
    fn ensure_write_only_rejects_reads_and_ddl() {
        for sql in [
            "SELECT 1",
            "SHOW TABLES",
            "DROP TABLE t",
            "TRUNCATE t",
            "ALTER TABLE t ADD COLUMN x int",
            "CREATE TABLE t (x int)",
        ] {
            assert!(
                ensure_write_only(sql).is_err(),
                "expected {sql} to be rejected"
            );
        }
    }

    #[test]
    fn ensure_write_only_rejects_multiple_statements() {
        assert!(ensure_write_only("UPDATE t SET x=1; DROP TABLE t;").is_err());
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

    fn load_opts(table: &'static str) -> LoadOpts<'static> {
        LoadOpts {
            table,
            columns: None,
            truncate: false,
            replace: false,
            has_header: true,
            delimiter: ',',
        }
    }

    #[test]
    fn pg_load_append_is_a_single_copy_from_stdin() {
        let stmts = pg_load_statements(&load_opts("orders")).unwrap();
        assert_eq!(stmts.len(), 1);
        assert_eq!(
            stmts[0],
            "\\copy orders FROM STDIN WITH (FORMAT csv, HEADER true, DELIMITER ',')"
        );
    }

    #[test]
    fn pg_load_truncate_comes_before_the_copy() {
        let mut opts = load_opts("orders");
        opts.truncate = true;
        let stmts = pg_load_statements(&opts).unwrap();
        assert_eq!(stmts[0], "TRUNCATE orders");
        assert!(stmts[1].starts_with("\\copy"));
        // And the full command quotes each into its own -c.
        assert!(
            load_command(&pg_creds(), &opts)
                .unwrap()
                .contains("-c 'TRUNCATE orders' -c")
        );
    }

    #[test]
    fn pg_load_rejects_replace() {
        let mut opts = load_opts("orders");
        opts.replace = true;
        assert!(pg_load_statements(&opts).is_err());
    }

    #[test]
    fn mysql_load_append_uses_load_data_local_infile() {
        let stmt = mysql_load_statement(&load_opts("orders")).unwrap();
        assert!(stmt.starts_with("LOAD DATA LOCAL INFILE '/dev/stdin' INTO TABLE orders"));
        assert!(stmt.contains("IGNORE 1 LINES"));
        assert!(!stmt.contains("REPLACE"));
        assert!(
            load_command(&mysql_creds(), &load_opts("orders"))
                .unwrap()
                .contains("mysql --local-infile=1")
        );
    }

    #[test]
    fn mysql_load_truncate_and_replace_and_columns() {
        let cols = vec!["id".to_string(), "total".to_string()];
        let mut opts = load_opts("orders");
        opts.truncate = true;
        opts.replace = true;
        opts.columns = Some(&cols);
        opts.has_header = false;
        let stmt = mysql_load_statement(&opts).unwrap();
        assert!(stmt.starts_with(
            "TRUNCATE orders; LOAD DATA LOCAL INFILE '/dev/stdin' REPLACE INTO TABLE orders"
        ));
        assert!(stmt.ends_with(" (id, total)"));
        assert!(!stmt.contains("IGNORE 1 LINES"));
    }

    #[test]
    fn load_rejects_a_bad_identifier() {
        assert!(mysql_load_statement(&load_opts("orders; DROP TABLE x")).is_err());
        let bad = vec!["id)".to_string()];
        let mut opts = load_opts("orders");
        opts.columns = Some(&bad);
        assert!(pg_load_statements(&opts).is_err());
    }
}
