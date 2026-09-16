use super::*;

// ── env ───────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub(crate) struct EnvShowArgs {
    /// Path to the .env file (defaults to ".env")
    pub(crate) file: Option<String>,
    /// Show real values instead of masking them
    #[serde(default)]
    pub(crate) reveal: bool,
    /// Working directory to resolve the file against
    pub(crate) cwd: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub(crate) struct EnvListArgs {
    /// Path to the .env file (defaults to ".env")
    pub(crate) file: Option<String>,
    pub(crate) cwd: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub(crate) struct EnvGetArgs {
    /// Variable name to look up
    pub(crate) key: String,
    /// Path to the .env file (defaults to ".env")
    pub(crate) file: Option<String>,
    pub(crate) cwd: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub(crate) struct EnvDiffArgs {
    pub(crate) file_a: String,
    pub(crate) file_b: String,
    pub(crate) cwd: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub(crate) struct EnvCheckArgs {
    /// Reference file (e.g. .env.example)
    pub(crate) reference: String,
    /// File to check (defaults to ".env")
    pub(crate) target: Option<String>,
    pub(crate) cwd: Option<String>,
}

// ── http ──────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub(crate) struct HttpGetArgs {
    /// URL or path (path uses the profile's base_url)
    pub(crate) url: String,
    /// Extra headers in "Key: Value" format
    #[serde(default)]
    pub(crate) headers: Vec<String>,
    /// Query parameters in "key=value" format
    #[serde(default)]
    pub(crate) query: Vec<String>,
    /// Timeout in seconds
    pub(crate) timeout: Option<u64>,
    /// Config profile to use for base_url/token resolution
    pub(crate) profile: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub(crate) struct HttpWriteArgs {
    pub(crate) url: String,
    /// JSON body string
    pub(crate) body: Option<String>,
    #[serde(default)]
    pub(crate) headers: Vec<String>,
    /// Query parameters in "key=value" format
    #[serde(default)]
    pub(crate) query: Vec<String>,
    pub(crate) timeout: Option<u64>,
    pub(crate) profile: Option<String>,
}

// ── jobs ──────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub(crate) struct JobsSearchArgs {
    /// Role/keywords to search for (default: desarrollador — Spanish terms match Spain
    /// listings much better than English ones)
    pub(crate) what: Option<String>,
    /// Location to search in (default: madrid)
    pub(crate) r#where: Option<String>,
    /// Adzuna country code, e.g. es, gb, us, de, fr (default: es)
    pub(crate) country: Option<String>,
    /// Sector/category tag, e.g. it-jobs, engineering-jobs — see tooler_jobs_categories
    pub(crate) category: Option<String>,
    /// Exclude listings matching this keyword (e.g. "java")
    pub(crate) exclude: Option<String>,
    /// Minimum salary (annual, in the country's currency)
    pub(crate) salary_min: Option<u32>,
    /// Only listings posted within this many days
    pub(crate) max_days_old: Option<u32>,
    /// Sort order: date, relevance, or salary (default: relevance)
    pub(crate) sort_by: Option<String>,
    /// Match `what` against the job title only, not the full description (more precise)
    pub(crate) title_only: Option<bool>,
    /// Result page, 1-indexed
    pub(crate) page: Option<u32>,
    /// Results per page (Adzuna max: 50)
    pub(crate) results: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
pub(crate) struct JobsCategoriesArgs {
    /// Adzuna country code, e.g. es, gb, us, de, fr (default: es)
    pub(crate) country: Option<String>,
}

// ── check ─────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub(crate) struct CheckUrlArgs {
    pub(crate) url: String,
    /// Timeout in seconds
    pub(crate) timeout: Option<u64>,
}

#[derive(Deserialize, JsonSchema)]
pub(crate) struct CheckPortArgs {
    pub(crate) host: String,
    pub(crate) port: u16,
    /// Timeout in seconds
    pub(crate) timeout: Option<u64>,
}

// ── config ────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub(crate) struct ConfigGetArgs {
    /// Config key, e.g. "default.output"
    pub(crate) key: String,
}

#[derive(Deserialize, JsonSchema)]
pub(crate) struct ConfigUnsetArgs {
    /// Config key, e.g. "profile.staging.token"
    pub(crate) key: String,
}

#[derive(Deserialize, JsonSchema)]
pub(crate) struct ConfigSetArgs {
    /// Config key, e.g. "default.output"
    pub(crate) key: String,
    pub(crate) value: String,
}

// ── json ──────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub(crate) struct JsonQueryArgs {
    /// JSON file path (stdin input is not available over MCP)
    pub(crate) file: String,
    /// Extract a field by dot-notation key (e.g. "user.name")
    pub(crate) key: Option<String>,
    /// Compact output instead of pretty-print
    #[serde(default)]
    pub(crate) compact: bool,
}

// ── run / play ────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub(crate) struct RunMcpArgs {
    /// Script name to run (omit to list available scripts)
    pub(crate) script: Option<String>,
    /// Show the command without executing it
    #[serde(default)]
    pub(crate) dry: bool,
    /// Extra arguments appended to the script command
    #[serde(default)]
    pub(crate) extra: Vec<String>,
    /// Working directory containing .tooler.toml
    pub(crate) cwd: Option<String>,
}

/// A short-lived temp file backing `PlayMcpArgs.content` -- deleted on drop, so an
/// inline playbook never lingers on disk past the one `tooler_play` call that used it.
/// Not backed by the `tempfile` crate (only a dev-dependency in this project): just
/// `std::env::temp_dir()` plus a name unique enough not to collide with a concurrent
/// MCP call (pid + a nanosecond timestamp).
pub(crate) struct TempPlaybookFile(pub(crate) PathBuf);

impl TempPlaybookFile {
    pub(crate) fn write(content: &str) -> std::io::Result<Self> {
        let name = format!(
            "tooler-play-inline-{}-{}.yml",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        );
        let path = std::env::temp_dir().join(name);
        std::fs::write(&path, content)?;
        Ok(Self(path))
    }
}

impl Drop for TempPlaybookFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Filename prefix every `tooler_play_repl_open`-created session file carries -- lets
/// `tooler_play_repl_exec`/`_close` reject a `session_id` that isn't one of ours (a
/// stray path a caller passed by mistake) before ever reading, writing, or deleting it.
pub(crate) const SESSION_FILE_PREFIX: &str = "tooler-play-session-";

/// Builds a new session file's path (not yet created on disk -- `tooler_play_repl_open`
/// writes it) -- pid + a nanosecond timestamp, same uniqueness scheme
/// `TempPlaybookFile` already uses, so two sessions opened in the same process tick
/// still can't collide. Unlike `TempPlaybookFile`, nothing here auto-deletes on drop --
/// a session must outlive any single tool call, so `tooler_play_repl_close` deletes it
/// explicitly instead.
pub(crate) fn new_session_path() -> PathBuf {
    let name = format!(
        "{SESSION_FILE_PREFIX}{}-{}.yml",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    );
    std::env::temp_dir().join(name)
}

/// Validates a caller-supplied `session_id` is really a path to one of our own session
/// files (right directory, right filename prefix) before `tooler_play_repl_exec`/
/// `_close` ever read, write, or delete it -- rejects a stray/foreign path outright
/// rather than trusting it.
pub(crate) fn validate_session_path(session_id: &str) -> Result<PathBuf, McpError> {
    let path = PathBuf::from(session_id);
    let in_temp_dir = path.parent() == Some(std::env::temp_dir().as_path());
    let right_prefix = path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with(SESSION_FILE_PREFIX));
    if !in_temp_dir || !right_prefix {
        return Err(McpError::invalid_params(
            "session_id is not a value returned by tooler_play_repl_open",
            None,
        ));
    }
    Ok(path)
}

#[derive(Deserialize, JsonSchema)]
pub(crate) struct PlayMcpArgs {
    /// Playbook YAML file to run (omit with init=true to generate a sample)
    pub(crate) file: Option<String>,
    /// Preview tasks without executing them
    #[serde(default)]
    pub(crate) dry: bool,
    /// Variable overrides in "key=value" form
    #[serde(default)]
    pub(crate) vars: Vec<String>,
    /// Comma-separated list of tags to run
    pub(crate) tags: Option<String>,
    /// Comma-separated list of tags to skip -- the complement of tags. A task must match
    /// tags (if given) and not match any skip_tags entry to run.
    pub(crate) skip_tags: Option<String>,
    /// Generate a sample playbook.yml instead of running one
    #[serde(default)]
    pub(crate) init: bool,
    /// Print the companion .md notes (if any) and exit without running any tasks
    #[serde(default)]
    pub(crate) notes: bool,
    /// Auto-confirm every confirm: task instead of prompting. A confirm: task always
    /// fails fast (never blocks) without this, since this MCP tool runs non-interactively.
    #[serde(default)]
    pub(crate) yes: bool,
    /// Skip ahead to this top-level task name, treating every earlier task as already
    /// done. Mutually exclusive with resume.
    pub(crate) start_at_task: Option<String>,
    /// Resume from the checkpoint left by a previous failed run of this same playbook
    /// file, restoring its vars and continuing right after the last completed task.
    /// Mutually exclusive with start_at_task. Errors if no checkpoint exists.
    #[serde(default)]
    pub(crate) resume: bool,
    /// Skip deleting the resume checkpoint after a fully-successful run -- keeps the
    /// ability to resume again later
    #[serde(default)]
    pub(crate) keep_checkpoint: bool,
    /// Local vars files to load (repeatable; a later file and vars both override an
    /// earlier one) -- for handing a whole computed set of vars to one run
    #[serde(default)]
    pub(crate) vars_file: Vec<String>,
    /// Append a JSON line per task attempt (timestamp, task, action, status, duration,
    /// error) to this file
    pub(crate) audit_log: Option<String>,
    /// Preview a colored diff of what fs_write:/write_file: are about to change, right
    /// before each applies its write
    #[serde(default)]
    pub(crate) diff: bool,
    /// List every task (name, action, tags) with zero side effects -- no vars_files:/
    /// secrets resolution, no connections, nothing run
    #[serde(default)]
    pub(crate) list_tasks: bool,
    /// List every distinct tag used anywhere in the playbook, sorted and deduplicated,
    /// with the same zero-side-effect parsing as list_tasks
    #[serde(default)]
    pub(crate) list_tags: bool,
    /// Static analysis with zero side effects: warns about a registered untrusted-source
    /// result reaching run:/ssh:/fleet: without a quote filter, and a {{var}} reference
    /// nothing earlier in the playbook defines. Heuristic, always succeeds -- findings
    /// are advisory, not a hard failure
    #[serde(default)]
    pub(crate) lint: bool,
    /// Dump the whole playbook DSL (every task action's fields, and their types) as a
    /// formal JSON Schema document and exit -- no file needed, this describes the
    /// language itself, not one playbook. For an agent about to write or validate a
    /// playbook.
    #[serde(default)]
    pub(crate) schema: bool,
    /// Resolve every task's vars and print the concrete action each would take (exact
    /// command/SQL/URL/target/path) as JSON, without running anything or connecting.
    /// Between dry (shape) and lint (problems).
    #[serde(default)]
    pub(crate) explain: bool,
    /// Inline playbook YAML instead of a file on disk -- written to a short-lived temp
    /// file for this call only, then deleted. Mutually exclusive with file. Only valid
    /// combined with dry=true, lint=true, list_tasks=true, or list_tags=true -- a real
    /// run still needs a real file, so a destructive action is never one step away
    /// from unreviewed YAML. For inspecting/validating a draft playbook before saving
    /// it, without a separate write-to-disk round trip.
    pub(crate) content: Option<String>,
    pub(crate) cwd: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub(crate) struct PlayReplOpenArgs {
    /// Shown as the playbook's name in tooler play's own output
    pub(crate) name: Option<String>,
    /// Initial variables, "key=value" (same shape as tooler_play's vars)
    #[serde(default)]
    pub(crate) vars: Vec<String>,
}

#[derive(Deserialize, JsonSchema)]
pub(crate) struct PlayReplExecArgs {
    /// The session_id returned by tooler_play_repl_open
    pub(crate) session_id: String,
    /// One task, the same YAML a line in `tooler play --repl` accepts -- e.g.
    /// "run: echo hi" or "{http: {url: \"...\"}, register: x}". Sees every var the
    /// session has registered/set so far.
    pub(crate) task: String,
    pub(crate) cwd: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub(crate) struct PlayReplCloseArgs {
    /// The session_id returned by tooler_play_repl_open
    pub(crate) session_id: String,
    /// If set, saves the session's accumulated playbook to this path (relative to cwd)
    /// before cleanup -- same as .save in the interactive REPL, so it becomes a real,
    /// standalone playbook file tooler play can run again later
    pub(crate) save_as: Option<String>,
    pub(crate) cwd: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub(crate) struct PlayReplListArgs {}

// ── git ───────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub(crate) struct GitCwdArgs {
    /// Repository directory (defaults to the MCP server's own working directory)
    pub(crate) cwd: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub(crate) struct GitCleanArgs {
    /// Also delete from origin
    #[serde(default)]
    pub(crate) remote: bool,
    /// Actually delete (default is preview-only)
    #[serde(default)]
    pub(crate) confirm: bool,
    /// Only branches with a trailing DDMMYY date suffix on/after this date (DDMMYY).
    /// When set (with `before`), targets any local branch in range regardless of
    /// merge status, instead of the default merged-only cleanup.
    pub(crate) after: Option<String>,
    /// Only branches with a trailing DDMMYY date suffix on/before this date (DDMMYY)
    pub(crate) before: Option<String>,
    pub(crate) cwd: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub(crate) struct GitChangelogArgs {
    /// Starting tag or commit (defaults to the latest tag)
    pub(crate) from: Option<String>,
    pub(crate) cwd: Option<String>,
}

// ── scaffold ──────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub(crate) struct ScaffoldNewArgs {
    /// Template name (see tooler_scaffold_list)
    pub(crate) template: String,
    /// Project name
    pub(crate) name: String,
    /// Destination directory (defaults to ./<name>)
    pub(crate) dir: Option<String>,
    pub(crate) cwd: Option<String>,
}

// ── report ────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub(crate) struct ReportArgs {
    /// Named JSON inputs, each `NAME=PATH` (e.g. from another tooler command's
    /// `--output json`). Omit to build the report from a single unnamed source.
    #[serde(default)]
    pub(crate) input: Vec<String>,
    /// Output file path to write the generated report to
    pub(crate) out: String,
    /// Report title
    pub(crate) title: Option<String>,
    pub(crate) cwd: Option<String>,
}

// ── db ────────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub(crate) struct DbQueryArgs {
    /// Server profile to run psql/mysql on (see tooler_server_list)
    pub(crate) server: String,
    /// SQL query (read-only: SELECT/SHOW/EXPLAIN/WITH/DESCRIBE only)
    pub(crate) sql: String,
    /// Remote path to a dotenv-style file (e.g. Laravel .env) to read DB_* credentials
    /// from. Preferred over passing credentials explicitly.
    pub(crate) env: Option<String>,
    /// DB engine when not using `env`: mysql or postgres
    pub(crate) engine: Option<String>,
    /// DB host as reachable from the server profile (when not using `env`)
    pub(crate) host: Option<String>,
    /// DB port (when not using `env`; defaults to the engine's standard port)
    pub(crate) port: Option<u16>,
    /// Database name (when not using `env`)
    pub(crate) database: Option<String>,
    /// DB username (when not using `env`)
    pub(crate) user: Option<String>,
    /// Cap the number of rows returned
    pub(crate) max_rows: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
pub(crate) struct DbExecArgs {
    /// Server profile to run psql/mysql on (see tooler_server_list)
    pub(crate) server: String,
    /// SQL statement -- INSERT/UPDATE/DELETE only, no DDL (no DROP/TRUNCATE/ALTER/CREATE)
    pub(crate) sql: String,
    /// Remote path to a dotenv-style file (e.g. Laravel .env) to read DB_* credentials
    /// from. Preferred over passing credentials explicitly.
    pub(crate) env: Option<String>,
    /// DB engine when not using `env`: mysql or postgres
    pub(crate) engine: Option<String>,
    /// DB host as reachable from the server profile (when not using `env`)
    pub(crate) host: Option<String>,
    /// DB port (when not using `env`; defaults to the engine's standard port)
    pub(crate) port: Option<u16>,
    /// Database name (when not using `env`)
    pub(crate) database: Option<String>,
    /// DB username (when not using `env`)
    pub(crate) user: Option<String>,
    /// Actually run the statement. Without this, the call only previews what would run
    /// (the resolved SQL and target database) and makes no change.
    #[serde(default)]
    pub(crate) confirm: bool,
}

#[derive(Deserialize, JsonSchema)]
pub(crate) struct DbBackupArgs {
    /// Server profile to run pg_dump/mysqldump on (see tooler_server_list)
    pub(crate) server: String,
    /// Local file path to write the dump to
    pub(crate) out: String,
    /// Remote path to a dotenv-style file (e.g. Laravel .env) to read DB_* credentials
    /// from. Preferred over passing credentials explicitly.
    pub(crate) env: Option<String>,
    /// DB engine when not using `env`: mysql or postgres
    pub(crate) engine: Option<String>,
    /// DB host as reachable from the server profile (when not using `env`)
    pub(crate) host: Option<String>,
    /// DB port (when not using `env`; defaults to the engine's standard port)
    pub(crate) port: Option<u16>,
    /// Database name (when not using `env`)
    pub(crate) database: Option<String>,
    /// DB username (when not using `env`)
    pub(crate) user: Option<String>,
    /// Skip gzip compression of the dump
    #[serde(default)]
    pub(crate) no_gzip: bool,
}

#[derive(Deserialize, JsonSchema)]
pub(crate) struct DbRestoreArgs {
    /// Server profile to run psql/mysql on (see tooler_server_list)
    pub(crate) server: String,
    /// Local dump file to restore (gzip-compressed input is auto-detected)
    #[serde(rename = "in")]
    pub(crate) input: String,
    pub(crate) env: Option<String>,
    pub(crate) engine: Option<String>,
    pub(crate) host: Option<String>,
    pub(crate) port: Option<u16>,
    pub(crate) database: Option<String>,
    pub(crate) user: Option<String>,
    /// Actually run the restore. Without this, the call only previews what would happen
    /// (bytes to send, target database) and makes no change.
    #[serde(default)]
    pub(crate) confirm: bool,
}

// ── mail ──────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub(crate) struct MailSendArgs {
    /// Recipient address(es), comma-separated
    pub(crate) to: String,
    /// Cc address(es), comma-separated
    pub(crate) cc: Option<String>,
    /// Bcc address(es), comma-separated
    pub(crate) bcc: Option<String>,
    pub(crate) subject: String,
    pub(crate) body: String,
    /// Send the body as text/html instead of text/plain
    #[serde(default)]
    pub(crate) html: bool,
    /// From address. Defaults to the profile's `from` (or its `user`)
    pub(crate) from: Option<String>,
    /// Mail profile to send through (see tooler_config_set mail.<name>.host). Required --
    /// this tool only accepts profile-based, keychain-backed credentials; a mail password
    /// can never be passed as a tool argument.
    pub(crate) server: String,
    /// Local file paths to attach
    #[serde(default)]
    pub(crate) attachments: Vec<String>,
}

#[derive(Deserialize, JsonSchema)]
pub(crate) struct MailCheckArgs {
    /// Mail profile to read from (see tooler_config_set mail.<name>.imap_port). Required --
    /// this tool only accepts profile-based, keychain-backed credentials.
    pub(crate) server: String,
    #[serde(default = "default_mail_folder")]
    pub(crate) folder: String,
    /// Fetch every message in the folder, not just unseen ones (default: unseen only)
    #[serde(default)]
    pub(crate) all: bool,
    /// Fetch each message's plain-text body too, not just headers
    #[serde(default)]
    pub(crate) include_body: bool,
    pub(crate) limit: Option<u32>,
    /// Mark fetched messages \Seen afterward, so a later call with unseen-only (the
    /// default) doesn't see them again. Defaults to false even for an agent -- mutating
    /// mailbox state needs an explicit opt-in.
    #[serde(default)]
    pub(crate) mark_seen: bool,
}

pub(crate) fn default_mail_folder() -> String {
    "INBOX".to_string()
}

// ── vault ─────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub(crate) struct VaultFileArgs {
    /// File to operate on
    pub(crate) file: String,
    /// Env var holding the passphrase (defaults to TOOLER_VAULT_PASSWORD)
    pub(crate) password_env: Option<String>,
    pub(crate) cwd: Option<String>,
}

pub(crate) fn vault_argv(action: &str, args: &VaultFileArgs) -> Vec<String> {
    let mut argv = vec!["vault".to_string(), action.to_string(), args.file.clone()];
    push_opt(&mut argv, "--password-env", &args.password_env);
    argv
}

#[derive(Deserialize, JsonSchema)]
pub(crate) struct VaultRekeyArgs {
    /// File to rekey
    pub(crate) file: String,
    /// Env var holding the current passphrase (defaults to TOOLER_VAULT_PASSWORD)
    pub(crate) old_password_env: Option<String>,
    /// Env var holding the new passphrase
    pub(crate) new_password_env: String,
    pub(crate) cwd: Option<String>,
}

// ── ps ────────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub(crate) struct PsListArgs {
    /// Server profile (see tooler_server_list)
    pub(crate) server: String,
    /// Only show processes whose command line (or PID) matches this substring
    pub(crate) filter: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub(crate) struct PsKillArgs {
    pub(crate) server: String,
    /// Process ID to signal
    pub(crate) pid: u32,
    /// Signal name or number (defaults to TERM)
    pub(crate) signal: Option<String>,
    /// Run via sudo. A sudo password, if needed, must never be passed as a tool
    /// argument -- set TOOLER_SUDO_PASS in the MCP server's own environment instead
    /// (or rely on passwordless/NOPASSWD sudo).
    #[serde(default)]
    pub(crate) sudo: bool,
    /// Actually send the signal. Without this, the call only previews what would happen.
    #[serde(default)]
    pub(crate) confirm: bool,
}

// ── fs ────────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub(crate) struct FsCatArgs {
    /// Server profile (see tooler_server_list)
    pub(crate) server: String,
    /// Remote file path
    pub(crate) path: String,
}

#[derive(Deserialize, JsonSchema)]
pub(crate) struct FsWriteArgs {
    pub(crate) server: String,
    pub(crate) path: String,
    /// Local file to read the new content from (binary-safe). Exactly one of
    /// `from_file`/`content` must be set.
    pub(crate) from_file: Option<String>,
    /// Literal text to write. Exactly one of `from_file`/`content` must be set.
    pub(crate) content: Option<String>,
    /// Actually write the file. Without this, the call only previews what would happen
    /// (byte count) and makes no change.
    #[serde(default)]
    pub(crate) confirm: bool,
}

#[derive(Deserialize, JsonSchema)]
pub(crate) struct FsDiffArgs {
    pub(crate) server: String,
    /// Remote file path
    pub(crate) path: String,
    /// Local file to compare against
    pub(crate) local: String,
}

// ── deploy ───────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub(crate) struct DeployRunArgs {
    /// Server profile (see tooler_server_list)
    pub(crate) server: String,
    /// Remote path (git repo) to deploy
    pub(crate) path: String,
    /// Pull the latest code (git pull) in `path` before restarting
    #[serde(default)]
    pub(crate) pull: bool,
    /// Command to run remotely in `path` after pulling (e.g. a build step)
    pub(crate) build: Option<String>,
    /// Command to restart the service (e.g. "systemctl restart myapp")
    pub(crate) restart: Option<String>,
    /// URL to check after restarting
    pub(crate) health_url: Option<String>,
    /// Timeout in seconds for each health check attempt (default 5)
    pub(crate) health_timeout: Option<u64>,
    /// Number of health check attempts before giving up (default 3)
    pub(crate) health_retries: Option<u32>,
    /// Seconds to wait between health check attempts (default 2)
    pub(crate) health_delay: Option<u64>,
    /// Run the restart command via sudo. A sudo password, if needed, must never be
    /// passed as a tool argument -- set TOOLER_SUDO_PASS in the MCP server's own
    /// environment instead (or rely on passwordless/NOPASSWD sudo).
    #[serde(default)]
    pub(crate) sudo: bool,
    /// Actually run the deploy. Without this, the call only previews the steps that
    /// would run and makes no change.
    #[serde(default)]
    pub(crate) confirm: bool,
}

// ── fleet ─────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub(crate) struct FleetExecArgs {
    /// Comma-separated server profile names (omit if all=true or group is set)
    pub(crate) servers: Option<String>,
    /// Target every configured server profile
    #[serde(default)]
    pub(crate) all: bool,
    /// Target a named server group (see tooler_group_list)
    pub(crate) group: Option<String>,
    /// Command to run
    pub(crate) command: String,
    /// Run command with sudo
    #[serde(default)]
    pub(crate) sudo: bool,
    /// Run on all targeted servers concurrently instead of one at a time
    #[serde(default)]
    pub(crate) parallel: bool,
}

#[derive(Deserialize, JsonSchema)]
pub(crate) struct FleetCheckArgs {
    /// Comma-separated server profile names (omit if all=true or group is set)
    pub(crate) servers: Option<String>,
    /// Target every configured server profile
    #[serde(default)]
    pub(crate) all: bool,
    /// Target a named server group (see tooler_group_list)
    pub(crate) group: Option<String>,
    /// Check all targeted servers concurrently instead of one at a time
    #[serde(default)]
    pub(crate) parallel: bool,
}

// ── group ─────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub(crate) struct GroupNameArgs {
    pub(crate) name: String,
}

#[derive(Deserialize, JsonSchema)]
pub(crate) struct GroupAddArgs {
    pub(crate) name: String,
    /// Server profile names that must already exist (see tooler_server_list)
    pub(crate) members: Vec<String>,
}

// ── stat ──────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub(crate) struct StatArgs {
    /// Server profile (see tooler_server_list)
    pub(crate) server: String,
}

// ── gh ────────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub(crate) struct GhPrsArgs {
    /// Repository as owner/name (defaults to the repo in the current directory)
    pub(crate) repo: Option<String>,
    /// Only PRs created on/after this date (YYYY-MM-DD)
    pub(crate) after: Option<String>,
    /// Only PRs created on/before this date (YYYY-MM-DD)
    pub(crate) before: Option<String>,
    /// PR state to include: open, closed, merged, or all
    pub(crate) state: Option<String>,
    /// Max PRs to fetch from GitHub before date filtering
    pub(crate) limit: Option<u32>,
    pub(crate) cwd: Option<String>,
}

// ── systemd ───────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub(crate) struct SystemdUnitArgs {
    /// Server profile (see tooler_server_list)
    pub(crate) server: String,
    /// Unit name, e.g. nginx or myapp.service
    pub(crate) unit: String,
}

#[derive(Deserialize, JsonSchema)]
pub(crate) struct SystemdRestartArgs {
    pub(crate) server: String,
    pub(crate) unit: String,
    /// Run via sudo. A sudo password, if needed, must never be passed as a tool
    /// argument -- set TOOLER_SUDO_PASS in the MCP server's own environment instead
    /// (or rely on passwordless/NOPASSWD sudo).
    #[serde(default)]
    pub(crate) sudo: bool,
}

#[derive(Deserialize, JsonSchema)]
pub(crate) struct SystemdLogsArgs {
    pub(crate) server: String,
    pub(crate) unit: String,
    /// Number of lines
    pub(crate) lines: Option<u32>,
    /// Run via sudo (some systems restrict journal access to root)
    #[serde(default)]
    pub(crate) sudo: bool,
}

// ── cron ──────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub(crate) struct CronServerArgs {
    pub(crate) server: String,
}

#[derive(Deserialize, JsonSchema)]
pub(crate) struct CronAddArgs {
    pub(crate) server: String,
    /// Full crontab line, e.g. "0 3 * * * /path/to/backup.sh"
    pub(crate) line: String,
}

#[derive(Deserialize, JsonSchema)]
pub(crate) struct CronRemoveArgs {
    pub(crate) server: String,
    /// Fixed substring to match (not a regex) -- matching lines are dropped
    pub(crate) pattern: String,
}

#[derive(Deserialize, JsonSchema)]
pub(crate) struct CronLocalAddArgs {
    /// Full crontab line, e.g. "0 8 * * * /usr/local/bin/tooler play ~/playbooks/x.yml"
    pub(crate) line: String,
}

#[derive(Deserialize, JsonSchema)]
pub(crate) struct CronLocalRemoveArgs {
    /// Fixed substring to match (not a regex) -- matching lines are dropped
    pub(crate) pattern: String,
}

// ── logs ──────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub(crate) struct LogsTailArgs {
    pub(crate) server: String,
    /// Remote file path
    pub(crate) path: String,
    /// Number of lines
    pub(crate) lines: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
pub(crate) struct LogsGrepArgs {
    pub(crate) server: String,
    pub(crate) path: String,
    /// Fixed substring to match (not a regex)
    pub(crate) pattern: String,
    /// Cap the number of matching lines returned
    pub(crate) max_lines: Option<usize>,
}

// ── server profiles ───────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub(crate) struct ServerNameArgs {
    pub(crate) name: String,
}

#[derive(Deserialize, JsonSchema)]
pub(crate) struct ServerAddArgs {
    pub(crate) name: String,
    pub(crate) host: String,
    pub(crate) user: Option<String>,
    pub(crate) port: Option<u16>,
    /// Path to private key, e.g. ~/.ssh/id_rsa
    pub(crate) key: Option<String>,
    /// Remote SSL certificate directory (default /etc/nginx/ssl)
    pub(crate) ssl_dir: Option<String>,
}

// ── ssh ───────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub(crate) struct SshCheckArgs {
    /// Server profile name
    pub(crate) server: String,
}

#[derive(Deserialize, JsonSchema)]
pub(crate) struct SshExecArgs {
    /// Server profile name
    pub(crate) server: String,
    /// Command to run on the remote server
    pub(crate) command: String,
    /// Run the command with sudo
    #[serde(default)]
    pub(crate) sudo: bool,
}

#[derive(Deserialize, JsonSchema)]
pub(crate) struct SshCopyArgs {
    /// Local file path
    pub(crate) local: String,
    /// Destination as server:path (e.g. gdn:/tmp/file)
    pub(crate) remote: String,
}

#[derive(Deserialize, JsonSchema)]
pub(crate) struct SshSslArgs {
    /// Server profile name
    pub(crate) server: String,
    /// Local .pfx certificate file
    pub(crate) pfx: String,
    /// Local private key file
    pub(crate) key: String,
    /// Remote SSL directory (defaults to the server profile's ssl_dir or /etc/nginx/ssl)
    pub(crate) remote_dir: Option<String>,
    /// Certificate filename on the server (defaults to wildcard.crt)
    pub(crate) cert_name: Option<String>,
    /// Key filename on the server (defaults to wildcard.key)
    pub(crate) key_name: Option<String>,
}
