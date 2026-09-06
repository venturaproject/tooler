use crate::{context::Context, output::OutputFormat, project, report};
use anyhow::{Context as _, Result, anyhow, bail};
use clap::Args;
use colored::Colorize;
use rustyline::{Editor, error::ReadlineError, history::DefaultHistory};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Args)]
pub struct PlayArgs {
    /// Playbook YAML file to run (omit with --init to generate a sample)
    pub file: Option<String>,

    /// Preview tasks without executing them
    #[arg(long)]
    pub dry: bool,

    /// Print a colored unified diff of what fs_write:/write_file: are about to change,
    /// right before each one applies its write. Reads the current content first — for
    /// fs_write: this means an SSH read even on a real (non-dry) run; --dry never
    /// connects, so --diff's remote preview only happens on a real run, as part of
    /// applying it. A missing file diffs as all-added ("new file").
    #[arg(long)]
    pub diff: bool,

    /// Override a variable: --var key=value (repeatable)
    #[arg(long = "var", short = 'e')]
    pub vars: Vec<String>,

    /// Load vars from a flat `key: value` YAML/JSON file (repeatable; a later file and
    /// --var both override an earlier one) — for handing a whole computed set of vars to
    /// one invocation without editing the playbook's own vars_files:/vars:. Paths resolve
    /// relative to the current directory, not the playbook's.
    #[arg(long = "vars-file")]
    pub vars_file: Vec<PathBuf>,

    /// Run only tasks matching these tags (comma-separated)
    #[arg(long)]
    pub tags: Option<String>,

    /// Skip tasks matching these tags (comma-separated) — the complement of --tags. A
    /// task must match --tags (if given) and not match any --skip-tags entry to run.
    #[arg(long = "skip-tags")]
    pub skip_tags: Option<String>,

    /// Generate a sample playbook.yml in the current directory
    #[arg(long)]
    pub init: bool,

    /// Print the companion playbooks/<name>.md notes (if any) and exit without running
    #[arg(long)]
    pub notes: bool,

    /// Auto-confirm every `confirm:` task instead of prompting. Required for `confirm:`
    /// tasks when running non-interactively (e.g. `--output json`, or driven by an agent
    /// over MCP) — otherwise those tasks fail fast rather than block on stdin.
    #[arg(long)]
    pub yes: bool,

    /// Skip ahead to the named top-level task, treating every earlier task as already
    /// done (not run, not counted). A practical rerun-after-a-fix tool, not a full
    /// --resume: a later task reading a `{{registered_var}}` from a now-skipped earlier
    /// task sees it unresolved, since no prior state is replayed. Top-level tasks only —
    /// has no effect inside `include:`/`block:`. Mutually exclusive with `--resume`, which
    /// covers this case with real state.
    #[arg(long = "start-at-task")]
    pub start_at_task: Option<String>,

    /// Resume from the checkpoint left by a previous failed run of this same playbook
    /// file (`<file>.state.json`, written after every top-level task and deleted on full
    /// success — see `PlayCheckpoint`). Restores the vars exactly as they were after the
    /// last completed task, then still applies any `--var` overrides on top, and continues
    /// with the task right after it. Errors if no checkpoint exists. Mutually exclusive
    /// with `--start-at-task`.
    #[arg(long)]
    pub resume: bool,

    /// Start an interactive console: type one task action at a time (`run: echo hi`, or
    /// `{http: {url: "..."}, register: x}` for multiple keys on one line) and see it
    /// execute immediately against a `vars` map that persists for the session. `.help`
    /// lists meta-commands (`.vars`, `.save <file>`, `.clear`, `.exit`). FILE, if given,
    /// only seeds initial vars from that playbook's `vars_files:`/`vars:` — its `tasks:`
    /// are never run.
    #[arg(long)]
    pub repl: bool,

    /// Append a JSON line per task attempt (timestamp, task, action, status, duration,
    /// error) to this file — a persistent execution trail, the same mechanism `tooler
    /// mcp --audit-log` uses for MCP tool calls. Covers every concrete attempt (top-level,
    /// inside block:/rescue:/always:/include:, each loop: iteration); a task skipped via
    /// `when:` is not logged, since it never touched anything.
    #[arg(long, env = "TOOLER_PLAY_AUDIT_LOG")]
    pub audit_log: Option<PathBuf>,

    /// List every task (name, action, tags) in the playbook, including tasks nested in
    /// block:/rescue:/always: — an include: task shows its target file but isn't
    /// recursed into. Parses the YAML and exits without resolving vars_files/secrets or
    /// running anything. Respects --tags/--skip-tags (top-level tasks only, same scoping
    /// those flags already have) so the listing matches what a real run would attempt.
    #[arg(long = "list-tasks")]
    pub list_tasks: bool,

    /// List every distinct tag used anywhere in the playbook (sorted, deduplicated).
    /// Same zero-side-effect parsing as --list-tasks.
    #[arg(long = "list-tags")]
    pub list_tags: bool,
}

// ── YAML schema ───────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Playbook {
    name: String,
    #[serde(default)]
    description: Option<String>,
    /// External var files (paths relative to this playbook's own directory), each a flat
    /// `key: value` YAML map — same shape as `vars:`, no new format. Loaded in order
    /// (a later file overrides an earlier one); `vars:` then overrides all of them; a CLI
    /// `--var` overrides everything. See `run()`.
    #[serde(default)]
    vars_files: Vec<String>,
    #[serde(default)]
    vars: HashMap<String, String>,
    tasks: Vec<Task>,
    /// Tasks triggered by `notify:`, run at most once each after all regular tasks
    /// succeed, in first-notified order. Matched by `name` — see `validate_handlers`.
    #[serde(default)]
    handlers: Vec<Task>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Task {
    name: String,
    #[serde(default)]
    ignore_errors: bool,
    #[serde(default)]
    tags: Vec<String>,
    /// Simple condition evaluated once against the playbook's vars, before any `loop:`
    /// expansion (does not see `{{item}}`). Supports "<a> == <b>", "<a> != <b>", or a
    /// bare truthy check after `{{var}}` substitution — not a full expression language.
    #[serde(default)]
    when: Option<String>,
    /// Run this task once per item. A scalar item is available as `{{item}}`; a map
    /// item exposes `{{item.<field>}}` per key (bare `{{item}}` stays literal for a map
    /// item). The first failing iteration fails the task (and, unless `ignore_errors`,
    /// the whole playbook) — remaining items are not attempted. Either a static YAML list
    /// (`loop: [a, b, c]`) or a dynamic source resolved at runtime from a var (typically a
    /// `register:`ed `http:`/`scrape:` result) — see `LoopSpec`.
    #[serde(default, rename = "loop")]
    loop_spec: Option<LoopSpec>,
    /// Capture this task's output into a variable, usable by later tasks via
    /// `{{name}}`. Supported on run/ssh/fleet only (see `run_task_once`). Inside a
    /// `loop:`, `<name>` still holds only the last iteration's value, but `<name>.results`
    /// is also set to a JSON array of every iteration's value in order — see
    /// `run_task`/`run_loop_parallel`.
    #[serde(default)]
    register: Option<String>,
    /// Retry this task up to N times (total attempts = retries + 1) before giving up.
    /// Applies per `loop:` iteration if combined with `loop:`. Ignored in `--dry`.
    #[serde(default)]
    retries: Option<u32>,
    /// Seconds to wait between retry attempts (default 1 if `retries:` is set).
    #[serde(default)]
    delay: Option<u64>,
    /// Retry this task (same syntax as `when:`) until this condition on its current vars
    /// (typically its own `register:`ed value) is true, or `retries:` attempts are
    /// exhausted — unlike plain `retries:`, which only retries on *failure*, `until:`
    /// also retries a *successful* task whose result doesn't satisfy the condition yet
    /// (e.g. polling a `run:`/`http:` result for "ready"). Only meaningful combined with
    /// `retries:` — without it, it's checked once, same as an `assert:` right after the
    /// task. Ignored in `--dry`, same as `retries:` (a dry run never really registers a
    /// value to check).
    #[serde(default)]
    until: Option<String>,
    /// Handler names (matching an entry in the playbook's `handlers:`) to trigger when
    /// this task succeeds and is considered "changed" (see `changed_when`). Deduplicated
    /// and run at most once each, after all regular tasks succeed.
    #[serde(default)]
    notify: Vec<String>,
    /// Condition (same syntax as `when:`) deciding whether this task's success counts as
    /// "changed" for `notify:` purposes. Absent means always changed on success —
    /// matches how a plain shell command has no built-in idempotency signal.
    #[serde(default)]
    changed_when: Option<String>,
    /// Condition (same syntax as `when:`) that overrides a task's outcome to failed even
    /// though its exit code says otherwise — e.g. a `run:` that always exits 0 but whose
    /// `register:`ed output contains an error marker. Evaluated independently of
    /// `changed_when:` (a task can be both "changed" and "failed"); checked before
    /// `until:`, so a `failed_when:`-triggered failure is retried by `retries:`/`delay:`
    /// like any other failure, not treated as "succeeded but not yet satisfied." Ignored
    /// in `--dry`, same as `until:` — a dry run never really registers a value to check.
    #[serde(default)]
    failed_when: Option<String>,

    // Actions — only one should be set per task
    run: Option<String>,
    check_url: Option<String>,
    check_port: Option<CheckPortSpec>,
    /// Make an HTTP request. `register:` (if set) captures two vars: `<reg>` = the
    /// response body text, `<reg>.status` = the status code as a string — the same
    /// dotted-key convention `loop:`'s map items already use for `item.<field>`. See
    /// `HttpSpec`; combine with the `| json:<path>` render filter to pull a field out of
    /// a JSON response, e.g. `{{resp | json:data.id}}`.
    http: Option<HttpSpec>,
    /// Scrape a page with CSS selectors. `register:` (if set) captures a JSON array of
    /// `fields` objects, one per `each:` match — directly loopable via a dynamic
    /// `loop: {from: "{{reg}}"}`. See `ScrapeSpec`.
    scrape: Option<ScrapeSpec>,
    /// Poll a check until it succeeds or times out — see `WaitForSpec`. Exactly one of
    /// `check_url`/`check_port`/`ssh` must be set within it (validated upfront).
    wait_for: Option<WaitForSpec>,
    /// Generate a PDF/Excel/HTML report from inline data — see `ReportSpec`. `register:`
    /// (if set) captures the output file's byte size, same convention as `sync_db:`.
    report: Option<ReportSpec>,
    env_check: Option<EnvCheckSpec>,
    ssh: Option<SshSpec>,
    fleet: Option<FleetSpec>,
    /// Read a remote file over SSH — the same `commands::fs::cat_cmd` `tooler fs cat`
    /// uses. See `FsCatSpec`.
    fs_cat: Option<FsCatSpec>,
    /// Overwrite a remote file over SSH — the same `commands::fs::write_cmd` `tooler fs
    /// write` uses. See `FsWriteSpec`.
    fs_write: Option<FsWriteSpec>,
    /// Restart a remote systemd unit — the same `commands::systemd::restart_cmd` `tooler
    /// systemd restart` uses. See `SystemdRestartSpec`.
    systemd_restart: Option<SystemdRestartSpec>,
    /// Check a remote systemd unit's status — the same `commands::systemd::status_cmd`
    /// `tooler systemd status` uses. See `SystemdStatusSpec`.
    systemd_status: Option<SystemdStatusSpec>,
    /// Tail a remote file over SSH — the same `commands::logs::tail_cmd` `tooler logs
    /// tail` uses. See `LogsTailSpec`.
    logs_tail: Option<LogsTailSpec>,
    /// Search a remote file over SSH — the same `commands::logs::grep_cmd` `tooler logs
    /// grep` uses. See `LogsGrepSpec`.
    logs_grep: Option<LogsGrepSpec>,
    /// List remote processes over SSH — the same `commands::ps::parse_ps_aux`/
    /// `apply_filter` `tooler ps list` uses. See `PsListSpec`.
    ps_list: Option<PsListSpec>,
    /// Send a signal to a remote process over SSH — the same `commands::ps::kill_cmd`
    /// `tooler ps kill` uses. See `PsKillSpec`.
    ps_kill: Option<PsKillSpec>,
    /// A remote server's uptime/memory/disk snapshot — the same `commands::stat`
    /// engine `tooler stat` uses. See `StatSpec`.
    stat: Option<StatSpec>,
    /// Run another whole playbook (by bare playbooks/ name, or a path relative to this
    /// playbook's own directory) as a single task — either bare (`include: sub.yml`) or
    /// with per-call var overrides (`include: {file: sub.yml, vars: {...}}`). See
    /// `IncludeSpec`, `resolve_include_path`.
    include: Option<IncludeSpec>,
    /// Fails the task immediately (not skips) unless the condition (same syntax as
    /// `when:`) holds.
    assert: Option<String>,
    /// Run these tasks in order as a single unit; see `rescue`/`always`. Counts as one
    /// outcome in the parent's recap — its own tasks aren't flattened into the parent's
    /// totals (same scope line as `include:`).
    block: Option<Vec<Task>>,
    /// Run only if `block:` failed; if these succeed, the block is considered recovered.
    rescue: Option<Vec<Task>>,
    /// Always run after `block:`/`rescue:`, regardless of outcome; a failure here fails
    /// the block even after a successful rescue.
    always: Option<Vec<Task>>,
    /// Print a rendered message; no side effects.
    debug: Option<String>,
    /// Compute/override vars from rendered expressions (supports the `| json:<path>`
    /// filter — see `render()`). Side-effect-only, like `debug:` — runs even in `--dry`,
    /// since setting a var has no external effect. Keys within one `set_fact:` block
    /// don't see each other (`HashMap` iteration order isn't defined) — split into
    /// separate tasks if one fact needs to build on another.
    set_fact: Option<HashMap<String, String>>,
    /// Like `set_fact:`, but persisted to `<file>.data.json` (a sibling of the playbook,
    /// never auto-deleted) so `{{state.<key>}}` is readable in *later, separate*
    /// `tooler play` invocations too, not just later tasks in this same run -- the
    /// memory a `tooler cron local`-scheduled playbook needs across runs (e.g. "last
    /// processed row ID"). No symmetric `state_get:`: reading is just `{{state.<key>}}`
    /// in any field, the same way there's no `get_fact:` for `set_fact:`.
    state_set: Option<HashMap<String, String>>,
    /// Pause for a human `y`/`N` confirmation before continuing; the rendered message is
    /// the prompt. Never blocks when driven non-interactively (`--output json`, which is
    /// also the MCP/agent path) unless `--yes` was passed — it fails fast instead, so an
    /// agent-driven `tooler play` can't hang forever on stdin. See `RunEnv.auto_yes`.
    confirm: Option<String>,
    /// Kill the task if it runs longer than this many seconds. Only supported on
    /// `run:` — there's no process handle to kill for `ssh:`/`fleet:` without changing
    /// the shared SSH helper they route through, so those reject `timeout:` upfront
    /// rather than silently not honoring it.
    #[serde(default)]
    timeout: Option<u64>,
    /// Dump `from`'s database and restore it into `to`'s, both reached through the same
    /// `server:` SSH profile. Dump bytes stay in memory the whole way — never written to
    /// local disk.
    sync_db: Option<DbSyncSpec>,
    /// Rsync a directory from one path to another on the same `server:`. `from` gets a
    /// trailing slash appended if missing, so it always copies contents, not the
    /// directory itself (see `ensure_trailing_slash`).
    sync_files: Option<SyncFilesSpec>,
    /// Write rendered `content` to a local file at `path` (relative to this playbook's
    /// own directory). Only the destination path and byte count are ever printed — never
    /// the content — since `content` may itself resolve `{{secret.*}}` tokens (e.g.
    /// writing a `.env` file). `register:` (if set) captures the byte count written. See
    /// `WriteFileSpec`.
    write_file: Option<WriteFileSpec>,
    /// Parse a local CSV file at `path` (relative to this playbook's own directory).
    /// `register:` (if set) captures a JSON array of rows — same directly-`loop:
    /// {from: "{{reg}}"}`-chainable convention `db_query:`/`scrape:`/`mail_check:` all
    /// use. See `ReadCsvSpec`.
    read_csv: Option<ReadCsvSpec>,
    /// Write a registered JSON array (from db_query:/read_csv:/http:+`| json:` filter) to
    /// a local CSV file at `path` (relative to this playbook's own directory) — the
    /// inverse of `read_csv:`. See `WriteCsvSpec`.
    write_csv: Option<WriteCsvSpec>,
    /// The playbook's own repo's branch/tag/status/recent-commits summary — the same
    /// `commands::git::compute_summary` `tooler git summary` uses. No fields; invoke as
    /// `git_summary: {}`. See `GitSummarySpec`.
    git_summary: Option<GitSummarySpec>,
    /// Commits since the last tag (or `from:`), categorized into features/fixes/other —
    /// the same `commands::git::compute_changelog` `tooler git changelog` uses. See
    /// `GitChangelogSpec`.
    git_changelog: Option<GitChangelogSpec>,
    /// List GitHub pull requests via the `gh` CLI — the same `commands::gh::fetch_prs`
    /// `tooler gh prs` uses. See `GhPrsSpec`.
    gh_prs: Option<GhPrsSpec>,
    /// Run a read-only SQL query against a database over SSH and capture the rows.
    /// `register:` (if set) captures a JSON array of row objects, same convention as
    /// `scrape:` — directly chainable into `loop: {from: "{{reg}}"}` or `report:`. See
    /// `DbQuerySpec`.
    db_query: Option<DbQuerySpec>,
    /// Send an email over SMTP, either through a configured `server:` profile
    /// (`tooler config set mail.<name>.host ...` + `mail.<name>.password`, the latter in
    /// the OS keychain) or fully inline `host`/`user`/`password` fields. See `MailSpec`.
    mail: Option<MailSpec>,
    /// Read a mail profile's inbox over IMAP (defaults to unseen messages only).
    /// `register:` (if set) captures a JSON array of messages — same
    /// `loop: {from: "{{reg}}"}`-chainable convention as `db_query:`/`scrape:`. See
    /// `MailCheckSpec`.
    mail_check: Option<MailCheckSpec>,
    /// Run a single INSERT/UPDATE/DELETE statement against a database over SSH.
    /// Deliberately requires `confirm: true` in the YAML itself — never runs silently.
    /// See `DbExecSpec`.
    db_exec: Option<DbExecSpec>,
    /// Cap concurrent `loop:` iterations to N at a time (processed in chunks of N) instead
    /// of the default strictly-sequential execution. Only valid combined with `loop:`. See
    /// `run_loop_parallel`.
    #[serde(default)]
    max_parallel: Option<usize>,
    /// Only valid combined with `loop:`: attempt every item regardless of an earlier one
    /// failing (each item still respects its own `retries:`/`until:`/`failed_when:`),
    /// instead of aborting on the first failure and leaving the rest untried. The task
    /// itself still fails at the end (respecting `ignore_errors:`, same as any other
    /// failure) if any item failed, with a summary naming which ones. `register:`'s
    /// `.results` keeps one entry per item either way — a failed item's slot is an empty
    /// string, so positions still line up with the original item order. See `run_task`,
    /// `run_loop_parallel`.
    #[serde(default)]
    continue_on_error: bool,
    /// Loads a flat `key: value` vars file mid-playbook (same file shape and loader as
    /// `vars_files:`/`--vars-file`, including transparent decryption of a `tooler
    /// vault`-encrypted file) — for loading vars based on something computed during this
    /// run, rather than only ever upfront via `vars_files:`. Path resolves relative to
    /// this playbook's own directory, same as `vars_files:` (also unconfined, same as
    /// `vars_files:` — this is an author-time path, not untrusted input). No `register:`
    /// support — like `set_fact:`, its job is setting vars directly.
    #[serde(default)]
    include_vars: Option<String>,
    /// Run any handlers `notify:`ed so far, right now, instead of waiting for them to run
    /// once at the very end of the playbook — Ansible's `meta: flush_handlers`. Only
    /// meaningful as a direct task in the top-level playbook's own `tasks:` (or an
    /// `include:`d sub-playbook's own `tasks:`, which has its own handler state); used
    /// inside `block:`/`rescue:`/`always:` it fails clearly instead of silently doing
    /// nothing, since those run outside `execute_playbook`'s handler bookkeeping. A no-op
    /// (still counts as `ok`) when nothing is pending. See `execute_playbook`,
    /// `run_notified_handlers`.
    #[serde(default)]
    flush_handlers: bool,
}

/// One `loop:` item — a plain scalar (`{{item}}`) or a map (`{{item.<field>}}` per key).
#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
enum LoopItem {
    Scalar(String),
    Map(HashMap<String, String>),
}

/// `loop:`'s two shapes — a static YAML list (unchanged, existing behavior) or a dynamic
/// source resolved at task-run time from a rendered var. `serde`'s untagged matching tries
/// `Static` first; a YAML sequence (`loop: [a, b, c]`) parses as `Static`, and a mapping
/// with a `from:` key (`loop: {from: "{{items}}"}`) parses as `Dynamic`.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum LoopSpec {
    Static(Vec<LoopItem>),
    Dynamic {
        /// Rendered once per task run. If the result parses as a JSON array, each element
        /// becomes a loop item (an object -> `LoopItem::Map` with stringified fields, any
        /// other JSON value -> `LoopItem::Scalar`); otherwise the rendered string is split
        /// on `split` (default `"\n"`) into scalar items, trimming empty lines. This makes
        /// a `register:`ed `scrape:`/`http:` result directly loopable with no new syntax.
        from: String,
        #[serde(default)]
        split: Option<String>,
    },
}

/// Resolves a `LoopSpec` into the `Vec<LoopItem>` `run_task` actually iterates —
/// `Static` is used as-is; `Dynamic` renders `from` against `vars` and either parses it as
/// a JSON array or falls back to a plain-text split. See `LoopSpec::Dynamic`'s doc comment
/// for the exact rules.
fn resolve_loop_items(spec: &LoopSpec, vars: &HashMap<String, String>) -> Vec<LoopItem> {
    match spec {
        LoopSpec::Static(items) => items.clone(),
        LoopSpec::Dynamic { from, split } => {
            let rendered = render(from, vars);
            if let Ok(serde_json::Value::Array(elements)) =
                serde_json::from_str::<serde_json::Value>(&rendered)
            {
                return elements
                    .into_iter()
                    .map(|el| match el {
                        serde_json::Value::Object(map) => LoopItem::Map(
                            map.into_iter()
                                .map(|(k, v)| {
                                    let s = match v {
                                        serde_json::Value::String(s) => s,
                                        other => other.to_string(),
                                    };
                                    (k, s)
                                })
                                .collect(),
                        ),
                        serde_json::Value::String(s) => LoopItem::Scalar(s),
                        other => LoopItem::Scalar(other.to_string()),
                    })
                    .collect();
            }
            let sep = split.as_deref().unwrap_or("\n");
            rendered
                .split(sep)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| LoopItem::Scalar(s.to_string()))
                .collect()
        }
    }
}

/// `include:`'s two shapes — a bare playbook reference (existing, unchanged behavior) or
/// a mapping with per-call `vars:` overrides. `serde` tries `Simple` first: a bare scalar
/// (`include: sub.yml`) parses as `Simple`; a mapping (`include: {file: sub.yml, vars:
/// {...}}`) parses as `WithVars`.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum IncludeSpec {
    Simple(String),
    WithVars {
        file: String,
        #[serde(default)]
        vars: HashMap<String, String>,
    },
}

impl IncludeSpec {
    fn file(&self) -> &str {
        match self {
            IncludeSpec::Simple(f) => f,
            IncludeSpec::WithVars { file, .. } => file,
        }
    }

    fn vars(&self) -> &HashMap<String, String> {
        static EMPTY: std::sync::OnceLock<HashMap<String, String>> = std::sync::OnceLock::new();
        match self {
            IncludeSpec::Simple(_) => EMPTY.get_or_init(HashMap::new),
            IncludeSpec::WithVars { vars, .. } => vars,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SshSpec {
    /// Server profile name (see: tooler server list)
    server: String,
    command: String,
    #[serde(default)]
    sudo: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FleetSpec {
    /// Comma-separated server profile names (mutually exclusive with all/group)
    #[serde(default)]
    servers: Option<String>,
    /// Named server group (see: tooler group list)
    #[serde(default)]
    group: Option<String>,
    #[serde(default)]
    all: bool,
    command: String,
    #[serde(default)]
    sudo: bool,
    /// Run on all targeted servers concurrently instead of one at a time
    #[serde(default)]
    parallel: bool,
    /// Only meaningful combined with `parallel: true` — runs targets in chunks of this
    /// size (one chunk fully finishes before the next starts) instead of all-at-once, a
    /// canary/rolling pattern (e.g. restart nginx 3 servers at a time across a
    /// 20-server fleet) rather than either strictly one-at-a-time or all-at-once.
    /// Ignored when `parallel` isn't set.
    #[serde(default)]
    batch_size: Option<usize>,
}

/// `fs_cat:` — reads a remote file over SSH via `commands::fs::cat_cmd`, the same
/// command builder `tooler fs cat` uses.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FsCatSpec {
    /// Server profile name (see: tooler server list)
    server: String,
    /// Remote file path
    path: String,
}

/// A destructive task spec gated behind `confirm: true` in the YAML — implemented by
/// `FsWriteSpec`/`SystemdRestartSpec`/`PsKillSpec`/`DbExecSpec`. See `Confirmed`.
trait RequiresConfirm {
    fn is_confirmed(&self) -> bool;
}

/// Proof that a destructive spec's `confirm: true` gate has already been checked. The
/// only way to obtain one is `Confirmed::require`, which bails if `confirm` isn't set —
/// so a function performing the actual side effect (`exec_fs_write`, etc.) can require
/// `&Confirmed<T>` instead of `&T` in its signature, making it impossible to call from
/// anywhere in the crate without going through the check first, at compile time rather
/// than by convention. This exists specifically because the "just remember to check
/// confirm" convention already failed once — `systemd_restart:` briefly shipped without
/// its gate and needed a follow-up fix — so this makes that class of bug a compile error
/// for any destructive action added from here on, not something a review has to catch.
#[derive(Debug)]
struct Confirmed<'a, T>(&'a T);

impl<'a, T: RequiresConfirm> Confirmed<'a, T> {
    fn require(spec: &'a T, action: &str, task_name: &str) -> Result<Self> {
        if !spec.is_confirmed() {
            bail!(
                "{action}: refused to run without confirm: true (task '{task_name}') — this \
                 is a deliberate action, add confirm: true to the task once you've reviewed it"
            );
        }
        Ok(Self(spec))
    }
}

impl<'a, T> std::ops::Deref for Confirmed<'a, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.0
    }
}

/// `fs_write:` — overwrites a remote file over SSH via `commands::fs::write_cmd`.
/// Deliberately requires `confirm: true` in the YAML itself, same non-negotiable gate
/// `db_exec:` uses — overwriting a remote file is just as destructive/hard-to-reverse as
/// a DML write, and a playbook has no interactive `--confirm` re-run step the way the
/// standalone `tooler fs write` command does.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FsWriteSpec {
    server: String,
    /// Remote file path
    path: String,
    content: String,
    #[serde(default)]
    confirm: bool,
}

impl RequiresConfirm for FsWriteSpec {
    fn is_confirmed(&self) -> bool {
        self.confirm
    }
}

/// `systemd_restart:` — restarts a remote systemd unit via `commands::systemd::restart_cmd`.
/// Deliberately requires `confirm: true` in the YAML itself, same non-negotiable gate
/// `fs_write:`/`ps_kill:`/`db_exec:` use — restarting a live service is just as
/// disruptive as a write or a kill.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SystemdRestartSpec {
    server: String,
    /// Unit name, e.g. nginx or myapp.service
    unit: String,
    #[serde(default)]
    sudo: bool,
    /// Sudo password (only used with sudo: true; omit to rely on NOPASSWD)
    #[serde(default)]
    sudo_pass: Option<String>,
    #[serde(default)]
    confirm: bool,
}

impl RequiresConfirm for SystemdRestartSpec {
    fn is_confirmed(&self) -> bool {
        self.confirm
    }
}

/// `systemd_status:` — checks a remote systemd unit via `commands::systemd::status_cmd`.
/// Never fails the task on an inactive unit — same query-not-control behavior
/// `tooler systemd status` already has; use `assert:`/`when:` on the registered
/// `<reg>.active` to decide what an inactive unit means for the playbook.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SystemdStatusSpec {
    server: String,
    unit: String,
}

/// `logs_tail:` — tails a remote file over SSH via `commands::logs::tail_cmd`. Only the
/// line count is ever printed (never the content, which could contain sensitive data) —
/// `register:` (if set) captures a JSON array of lines, same `loop: {from: "{{reg}}"}`
/// -chainable convention as `db_query:`/`scrape:`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LogsTailSpec {
    server: String,
    /// Remote file path
    path: String,
    #[serde(default = "default_tail_lines")]
    lines: u32,
}

fn default_tail_lines() -> u32 {
    100
}

/// `logs_grep:` — searches a remote file over SSH via `commands::logs::grep_cmd` (a
/// fixed-substring match, not a regex). Same content-hiding convention as `logs_tail:`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LogsGrepSpec {
    server: String,
    /// Remote file path
    path: String,
    /// Fixed substring to match (not a regex)
    pattern: String,
    #[serde(default = "default_grep_max_lines")]
    max_lines: usize,
}

fn default_grep_max_lines() -> usize {
    200
}

/// `ps_list:` — lists remote processes over SSH via `commands::ps::parse_ps_aux`/
/// `apply_filter`. Same content-hiding convention as `fs_cat:`/`logs_tail:`: row-shaped
/// data, so only the count prints; `register:` captures the JSON array.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PsListSpec {
    server: String,
    /// Only include processes whose command line (or PID) matches this substring
    #[serde(default)]
    filter: Option<String>,
}

/// `ps_kill:` — sends a signal to a remote process via `commands::ps::kill_cmd`.
/// Deliberately requires `confirm: true` in the YAML itself, same non-negotiable gate
/// `db_exec:`/`fs_write:` use.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PsKillSpec {
    server: String,
    pid: u32,
    #[serde(default = "default_kill_signal")]
    signal: String,
    #[serde(default)]
    sudo: bool,
    #[serde(default)]
    sudo_pass: Option<String>,
    #[serde(default)]
    confirm: bool,
}

impl RequiresConfirm for PsKillSpec {
    fn is_confirmed(&self) -> bool {
        self.confirm
    }
}

fn default_kill_signal() -> String {
    "TERM".to_string()
}

/// `stat:` — a remote server's uptime/memory/disk snapshot via `commands::stat::stat_cmd`/
/// `parse_sections`. A single small operational status blob, not row-shaped bulk data,
/// so it prints directly (same as `systemd_status:`); `register:` captures
/// `{uptime, memory, disk}` as JSON for `report:`/`mail:` chaining.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StatSpec {
    server: String,
}

/// `git_summary:` — the local repo's branch/tag/status/recent-commits summary via
/// `commands::git::compute_summary`, run against the playbook's own directory (same cwd
/// convention `run:` already has). No fields; invoked as `git_summary: {}`.
#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct GitSummarySpec {}

/// `git_changelog:` — commits since the last tag (or `from:`), categorized into
/// features/fixes/other, via `commands::git::compute_changelog`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GitChangelogSpec {
    /// Starting tag or commit (defaults to the latest tag)
    #[serde(default)]
    from: Option<String>,
}

/// `gh_prs:` — lists GitHub pull requests via `commands::gh::fetch_prs` (shells out to
/// the `gh` CLI). Row-shaped external data like `db_query:`, so only the count prints;
/// `register:` captures the JSON array, directly chainable into `report:`/`loop:`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GhPrsSpec {
    /// Repository as owner/name (defaults to the repo in the playbook's own directory)
    #[serde(default)]
    repo: Option<String>,
    /// Only PRs created on/after this date (YYYY-MM-DD)
    #[serde(default)]
    after: Option<String>,
    /// Only PRs created on/before this date (YYYY-MM-DD)
    #[serde(default)]
    before: Option<String>,
    #[serde(default = "default_pr_state")]
    state: String,
    #[serde(default = "default_pr_limit")]
    limit: u32,
}

fn default_pr_state() -> String {
    "all".to_string()
}

fn default_pr_limit() -> u32 {
    500
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DbSyncSpec {
    /// Server profile to run mysqldump/pg_dump + mysql/psql through (both sides)
    server: String,
    from: DbSyncSide,
    to: DbSyncSide,
}

/// One side of a `sync_db:` task — either `env:` (a remote dotenv-style file, e.g. a
/// Laravel `.env`, to read DB_* credentials from) or the explicit fields. Mirrors
/// `commands::db::ConnOpts`, which `resolve_db_sync_creds` delegates to.
#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct DbSyncSide {
    #[serde(default)]
    env: Option<String>,
    #[serde(default)]
    engine: Option<String>,
    #[serde(default)]
    host: Option<String>,
    #[serde(default)]
    port: Option<u16>,
    #[serde(default)]
    database: Option<String>,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    password: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SyncFilesSpec {
    /// Server profile (see: tooler server list)
    server: String,
    from: String,
    to: String,
    /// Pass `--delete` to rsync, removing destination files no longer present in `from`
    #[serde(default)]
    delete: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckPortSpec {
    host: String,
    port: u16,
    #[serde(default = "default_timeout")]
    timeout: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EnvCheckSpec {
    reference: String,
    #[serde(default = "default_env_target")]
    target: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HttpSpec {
    #[serde(default = "default_http_method")]
    method: String,
    url: String,
    #[serde(default)]
    headers: HashMap<String, String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default = "default_timeout")]
    timeout: u64,
    /// Don't fail the task on a non-2xx status — let when:/assert: on the registered
    /// `<reg>.status` decide instead. Default false, matching check_url:'s fail-fast.
    #[serde(default)]
    ignore_status: bool,
    /// Save the response body to this local file (relative to the playbook's own
    /// directory, confined via `join_confined`) instead of capturing it as a string —
    /// binary-safe, unlike the default `resp.text()` path. Combine with `register:` to
    /// capture the (still-relative) rendered `download:` path — not its bytes — for
    /// chaining straight into a later path-taking task, e.g.
    /// `mail: {attachments: ["{{reg}}"]}`, since every such task resolves its path the
    /// same way, relative to this same playbook directory.
    #[serde(default)]
    download: Option<String>,
}

/// Poll one of `check_url`/`check_port`/`ssh` (exactly one — validated upfront in
/// `run_task_once`) every `interval` seconds until it succeeds or `timeout` elapses.
/// Distinct from `retries:`, which retries a whole task on *failure*; `wait_for:` is for
/// "keep checking until this becomes true" (e.g. wait for a service to come back up after
/// a restart), so it doesn't log every attempt the way `retries:` does.
#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct WaitForSpec {
    #[serde(default)]
    check_url: Option<String>,
    #[serde(default)]
    check_port: Option<CheckPortSpec>,
    #[serde(default)]
    ssh: Option<SshSpec>,
    /// Poll until a local file (relative to the playbook directory, confined via
    /// `join_confined`) exists -- e.g. waiting for an upload to land.
    #[serde(default)]
    file_exists: Option<String>,
    /// Poll until a local file no longer exists -- e.g. waiting for a lock to clear.
    #[serde(default)]
    file_absent: Option<String>,
    #[serde(default = "default_wait_interval")]
    interval: u64,
    #[serde(default = "default_wait_timeout")]
    timeout: u64,
}

fn default_wait_interval() -> u64 {
    2
}
fn default_wait_timeout() -> u64 {
    60
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScrapeSpec {
    url: String,
    #[serde(default)]
    headers: HashMap<String, String>,
    #[serde(default = "default_timeout")]
    timeout: u64,
    /// CSS selector for each "row"; omit to scrape the whole page as a single item.
    #[serde(default)]
    each: Option<String>,
    /// field name -> CSS selector, optionally `"<selector>@<attr>"` to grab an attribute
    /// (e.g. `href`, `src`) instead of trimmed text content.
    fields: HashMap<String, String>,
}

/// Generate a PDF/Excel/HTML report — the same engine `tooler report pdf/excel/html`
/// uses (`report::{pdf,excel,html}::build`), but fed inline data instead of file paths,
/// so a `register:`ed `http:`/`scrape:` result can go straight into a report with no
/// temp-file round-trip.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReportSpec {
    /// "html", "pdf", or "excel"
    format: String,
    #[serde(default = "default_report_title")]
    title: String,
    /// name -> a rendered value (typically `"{{a_registered_var}}"`). Parsed as JSON if
    /// possible; a value that isn't valid JSON is wrapped as a plain JSON string instead
    /// of failing the task, matching the DSL's general tolerance for opaque var content
    /// elsewhere (e.g. a missing `scrape:` field becomes `""`, not an error).
    sources: HashMap<String, String>,
    /// Output path, relative to the playbook's own directory (same rule as `run:`'s
    /// working directory / `env_check:`'s paths).
    out: String,
}

fn default_report_title() -> String {
    "Tooler Report".to_string()
}

/// `write_file:` — writes rendered `content` to `path` (relative to the playbook's own
/// directory), creating parent directories as needed. `path` is confined to that
/// directory by `join_confined` — an absolute path or a `..` that nets outside it is
/// rejected, rather than silently writing wherever a rendered `{{var}}` happened to point.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteFileSpec {
    path: String,
    content: String,
    /// Append instead of overwrite.
    #[serde(default)]
    append: bool,
}

/// `read_csv:` — the read-side counterpart to `write_file:`. `path` is confined to the
/// playbook's own directory the same way (`join_confined`). `headers: true` (default)
/// uses the first row as field names, producing one JSON object per row; `headers: false`
/// produces plain arrays instead. Every cell comes back as a JSON string -- no type
/// guessing, same "let the consumer decide" philosophy `parse_mysql_tsv` already uses.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadCsvSpec {
    path: String,
    #[serde(default = "default_true")]
    headers: bool,
    /// Single character. Defaults to ','.
    #[serde(default)]
    delimiter: Option<String>,
}

/// `write_csv:` — the write-side counterpart to `read_csv:`. `path` is confined to the
/// playbook's own directory the same way. `data` is rendered and must parse as a JSON
/// array: an array of objects writes a header row from the *first* object's keys (unless
/// `headers: false`) followed by one row per object in that key order — since this crate
/// builds `serde_json::Value::Object` without the `preserve_order` feature, that key
/// order is alphabetical, not YAML/JSON source order; an array of plain values/arrays is
/// written as raw rows (`headers:` has no effect — there are no field names to derive a
/// header from).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteCsvSpec {
    path: String,
    /// Rendered, then parsed as a JSON array — typically `"{{a_registered_var}}"`.
    data: String,
    #[serde(default = "default_true")]
    headers: bool,
    /// Single character. Defaults to ','.
    #[serde(default)]
    delimiter: Option<String>,
}

/// Renders one JSON value as a CSV cell for `write_csv:`: a string is used as-is (not
/// re-quoted with JSON escaping), a number/bool uses its plain display form, and
/// null/missing becomes an empty cell — matching how `render()` already stringifies
/// values elsewhere in this DSL.
fn json_cell_to_string(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// `db_query:` — mirrors `commands::db::DbSubcommand::Query`'s fields exactly, so the
/// mental model transfers 1:1 from the standalone `tooler db query` command.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DbQuerySpec {
    /// Server profile to run the query through (see: tooler server list)
    server: String,
    /// SQL query (SELECT/SHOW/EXPLAIN/WITH/DESCRIBE only — enforced by `db::run_query`)
    sql: String,
    /// Remote path to a dotenv-style file (e.g. Laravel .env) to read DB_* credentials
    /// from, instead of the explicit fields below.
    #[serde(default)]
    env: Option<String>,
    #[serde(default)]
    engine: Option<String>,
    #[serde(default)]
    host: Option<String>,
    #[serde(default)]
    port: Option<u16>,
    #[serde(default)]
    database: Option<String>,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    password: Option<String>,
    #[serde(default = "default_db_max_rows")]
    max_rows: usize,
}

fn default_db_max_rows() -> usize {
    1000
}

/// `db_exec:` — same connection fields as `DbQuerySpec` minus `max_rows` (a single
/// statement has no rows to cap), plus `confirm`. Mirrors `commands::db::DbSubcommand::
/// Exec`'s fields, enforced read-side by `db::ensure_write_only` (INSERT/UPDATE/DELETE
/// only, no DDL). `confirm` must be `true` in the YAML itself -- the same "never runs
/// silently" posture `tooler db restore --confirm` uses on the CLI, just expressed as a
/// visible task field instead of a flag, so it shows up in a code review/diff.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DbExecSpec {
    server: String,
    /// SQL statement (INSERT/UPDATE/DELETE only — enforced by `db::run_exec`)
    sql: String,
    #[serde(default)]
    env: Option<String>,
    #[serde(default)]
    engine: Option<String>,
    #[serde(default)]
    host: Option<String>,
    #[serde(default)]
    port: Option<u16>,
    #[serde(default)]
    database: Option<String>,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    confirm: bool,
}

impl RequiresConfirm for DbExecSpec {
    fn is_confirmed(&self) -> bool {
        self.confirm
    }
}

/// `mail:` — every field is renderable via `render()` (so `{{secret.<profile>.password}}`
/// or any `{{var}}` works anywhere here, same as `db_query:`). `to`/`cc`/`bcc` accept a
/// comma-separated list of addresses. Credentials resolve through `resolve_mail_creds`:
/// explicit `host`/`port`/`user`/`password`/`tls` fields win over the named `server:`
/// profile (`config.mail.<name>` + the OS keychain), which wins over `TOOLER_MAIL_PASSWORD`
/// for the password specifically.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MailSpec {
    /// Mail profile to send through (see: tooler config set mail.<name>.host, and
    /// following fields).
    #[serde(default)]
    server: Option<String>,
    to: String,
    #[serde(default)]
    cc: Option<String>,
    #[serde(default)]
    bcc: Option<String>,
    subject: String,
    body: String,
    /// Send the body as `text/html` instead of `text/plain`.
    #[serde(default)]
    html: bool,
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    host: Option<String>,
    #[serde(default)]
    port: Option<u16>,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    password: Option<String>,
    /// "starttls" | "tls" | "none" — overrides both the profile's `tls` and the
    /// port-based inference in `resolve_mail_creds`.
    #[serde(default)]
    tls: Option<String>,
    /// Local file paths to attach, relative to the playbook's own directory (confined
    /// via `join_confined`) — typically a `report:` output or an `http: {download:
    /// ...}` result.
    #[serde(default)]
    attachments: Vec<String>,
}

/// `mail_check:` — reads a `server:` mail profile's inbox over IMAP. Profile-only (no
/// inline host/user/password the way `mail:`/`db_query:` allow): narrower, newer, and a
/// profile is the common case since IMAP shares the same mailbox login `mail:` already
/// uses. See `fetch_mail`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MailCheckSpec {
    /// Mail profile to read from (see: tooler config set mail.<name>.imap_port, etc).
    server: String,
    #[serde(default = "default_mail_folder")]
    folder: String,
    /// Only fetch messages without the \Seen flag. Default true — the common "what's new"
    /// case.
    #[serde(default = "default_true")]
    unseen_only: bool,
    #[serde(default = "default_mail_check_limit")]
    limit: u32,
    /// Fetch each message's plain-text body too, not just headers. Off by default to keep
    /// `register:`'s captured JSON small.
    #[serde(default)]
    include_body: bool,
    /// Mark fetched messages \Seen afterward, so a later run's `unseen_only` doesn't
    /// reprocess them -- the idempotency primitive for "check inbox -> act -> don't act
    /// twice". Off by default: mutating mailbox state is opt-in, same posture `db_query:`'s
    /// read-only default and `db exec`'s `confirm:` gate already establish.
    #[serde(default)]
    mark_seen: bool,
}

fn default_mail_folder() -> String {
    "INBOX".to_string()
}
fn default_mail_check_limit() -> u32 {
    10
}
fn default_true() -> bool {
    true
}

fn default_timeout() -> u64 {
    5
}
fn default_env_target() -> String {
    ".env".to_string()
}
fn default_http_method() -> String {
    "GET".to_string()
}

// ── Entrypoint ────────────────────────────────────────────────────────────────

/// A playbook argument is a literal path (existing, unchanged behavior) if it contains a
/// `/` or already ends in `.yml`/`.yaml`; otherwise it's a bare name, resolved against
/// `<project_root>/playbooks/<name>.yml` (then `.yaml`).
fn is_literal_path(s: &str) -> bool {
    s.contains('/') || s.ends_with(".yml") || s.ends_with(".yaml")
}

/// Joins `rel` onto `base`, rejecting anything that would land outside `base`: an
/// absolute `rel` (which `Path::join` would otherwise honor verbatim, discarding `base`
/// entirely), or a `..` that nets below `base` once walked lexically. Doesn't touch the
/// filesystem (no `canonicalize`) since the caller — `write_file:` — may be about to
/// create the file, so it need not exist yet. `a/../b` is allowed (it never actually
/// leaves `base`, just references it awkwardly); `../b` or `a/../../b` are not.
fn join_confined(base: &Path, rel: &str) -> Result<PathBuf> {
    if Path::new(rel).is_absolute() {
        bail!(
            "path '{rel}' must be relative to the playbook directory (absolute paths are rejected)"
        );
    }
    let mut resolved = base.to_path_buf();
    let mut depth: i32 = 0;
    for comp in Path::new(rel).components() {
        match comp {
            std::path::Component::Normal(part) => {
                depth += 1;
                resolved.push(part);
            }
            std::path::Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    bail!("path '{rel}' escapes the playbook directory");
                }
                resolved.pop();
            }
            std::path::Component::CurDir => {}
            _ => bail!("path '{rel}' is not a valid relative path"),
        }
    }
    Ok(resolved)
}

/// Appends a trailing `/` to `path` if missing — rsync only copies a source directory's
/// *contents* when the source path ends in `/`; without it, the directory itself gets
/// nested one level deeper inside the destination. A well-known footgun `sync_files:`
/// guards against automatically.
fn ensure_trailing_slash(path: &str) -> String {
    if path.ends_with('/') {
        path.to_string()
    } else {
        format!("{path}/")
    }
}

fn resolve_playbook_file(file: &str, project_root: &Path) -> Result<PathBuf> {
    if is_literal_path(file) {
        return PathBuf::from(file)
            .canonicalize()
            .with_context(|| format!("Cannot resolve path: {file}"));
    }
    let dir = project_root.join("playbooks");
    for ext in ["yml", "yaml"] {
        let candidate = dir.join(format!("{file}.{ext}"));
        if candidate.exists() {
            return candidate
                .canonicalize()
                .with_context(|| format!("Cannot resolve path: {}", candidate.display()));
        }
    }
    bail!(
        "No playbook named '{file}' in {} (looked for {file}.yml, {file}.yaml).\n  \
         Create one with: tooler play --init {file}",
        dir.display()
    );
}

/// Companion runbook path for a resolved playbook file — same directory/stem, `.md`
/// extension. Works uniformly whether `file_path` came from `playbooks/<name>.yml` or a
/// literal path, since it's derived from the already-resolved path.
fn notes_path(file_path: &Path) -> PathBuf {
    file_path.with_extension("md")
}

fn read_notes(file_path: &Path) -> Option<String> {
    std::fs::read_to_string(notes_path(file_path)).ok()
}

fn print_notes_only(file_path: &Path, ctx: &Context) -> Result<()> {
    let notes = read_notes(file_path);
    if ctx.output == OutputFormat::Json {
        println!(
            "{}",
            serde_json::json!({"file": file_path.display().to_string(), "notes": notes})
        );
        return Ok(());
    }
    match notes {
        Some(n) => println!("{}", n.trim_end()),
        None => println!("{}", "No notes for this playbook.".dimmed()),
    }
    Ok(())
}

/// One node of the `--list-tasks` tree — see `build_task_list`/`print_task_list`. Only
/// ever built from a parsed `Playbook`, never executed against; the point is to describe
/// a playbook's shape with zero side effects.
#[derive(Serialize)]
struct TaskListEntry {
    name: String,
    action: &'static str,
    tags: Vec<String>,
    /// An `include:` task's target file — shown, but not recursed into (a separate file,
    /// possibly not resolvable without the full project context `--list-tasks` deliberately
    /// skips).
    #[serde(skip_serializing_if = "Option::is_none")]
    include: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    block: Vec<TaskListEntry>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    rescue: Vec<TaskListEntry>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    always: Vec<TaskListEntry>,
}

/// Builds the `--list-tasks` tree from a task list, recursing into `block:`/`rescue:`/
/// `always:` (which are real tasks of this same playbook) but not `include:` (a separate
/// file — see `TaskListEntry::include`). Generic over the iterator so both a filtered
/// `&[&Task]` (the top-level call) and an owned `&[Task]` (`block:`'s own `Vec<Task>`)
/// work without cloning a `Task`.
fn build_task_list<'a>(tasks: impl IntoIterator<Item = &'a Task>) -> Vec<TaskListEntry> {
    tasks
        .into_iter()
        .map(|t| TaskListEntry {
            name: t.name.clone(),
            action: task_action_label(t),
            tags: t.tags.clone(),
            include: t.include.as_ref().map(|s| s.file().to_string()),
            block: t.block.as_deref().map(build_task_list).unwrap_or_default(),
            rescue: t.rescue.as_deref().map(build_task_list).unwrap_or_default(),
            always: t.always.as_deref().map(build_task_list).unwrap_or_default(),
        })
        .collect()
}

/// `--list-tasks`: prints the playbook's task tree (name, action, tags, and nested
/// `block:`/`rescue:`/`always:`) with zero side effects — no `vars_files:`/secrets
/// resolution, no connections, no execution. `tasks` is already `--tags`/`--skip-tags`
/// filtered (see `task_matches_tags`) so the listing matches what a real run would
/// attempt.
fn print_task_list(playbook_name: &str, tasks: &[&Task], ctx: &Context) -> Result<()> {
    let entries = build_task_list(tasks.iter().copied());
    if ctx.output == OutputFormat::Json {
        println!(
            "{}",
            serde_json::json!({"playbook": playbook_name, "tasks": entries})
        );
        return Ok(());
    }
    println!(
        "{} {}",
        "PLAY".bold().cyan(),
        format!("[{playbook_name}]").bold()
    );
    fn print_entries(entries: &[TaskListEntry], depth: usize) {
        let indent = "  ".repeat(depth + 1);
        for e in entries {
            let mut line = format!("{indent}{} {}", "-".dimmed(), e.name);
            if let Some(file) = &e.include {
                line.push_str(&format!(" {}", format!("(include: {file})").dimmed()));
            } else {
                line.push_str(&format!(" {}", format!("({})", e.action).dimmed()));
            }
            if !e.tags.is_empty() {
                line.push_str(&format!(
                    " {}",
                    format!("tags: [{}]", e.tags.join(", ")).dimmed()
                ));
            }
            println!("{line}");
            for (label, sub) in [
                ("block:", &e.block),
                ("rescue:", &e.rescue),
                ("always:", &e.always),
            ] {
                if !sub.is_empty() {
                    println!("{}{}", "  ".repeat(depth + 2), label.dimmed());
                    print_entries(sub, depth + 2);
                }
            }
        }
    }
    print_entries(&entries, 0);
    Ok(())
}

/// `--list-tags`: every distinct tag used anywhere in `tasks`, including nested
/// `block:`/`rescue:`/`always:` (but not a separate `include:`d file), sorted and
/// deduplicated. Same zero-side-effect parsing as `--list-tasks`.
fn collect_tags<'a>(
    tasks: impl IntoIterator<Item = &'a Task>,
    out: &mut std::collections::BTreeSet<String>,
) {
    for t in tasks {
        out.extend(t.tags.iter().cloned());
        if let Some(b) = &t.block {
            collect_tags(b, out);
        }
        if let Some(r) = &t.rescue {
            collect_tags(r, out);
        }
        if let Some(a) = &t.always {
            collect_tags(a, out);
        }
    }
}

fn print_tag_list(tasks: &[&Task], ctx: &Context) -> Result<()> {
    let mut tags = std::collections::BTreeSet::new();
    collect_tags(tasks.iter().copied(), &mut tags);
    if ctx.output == OutputFormat::Json {
        println!("{}", serde_json::json!({"tags": tags}));
        return Ok(());
    }
    if tags.is_empty() {
        println!("{}", "No tags used in this playbook.".dimmed());
    } else {
        for tag in tags {
            println!("{tag}");
        }
    }
    Ok(())
}

/// Mostly-static, per-run execution context threaded through the dispatch chain —
/// bundled into one struct because the parameter list (playbook_dir, project_root, dry,
/// quiet, ctx, plus mutable vars/include_stack passed alongside) got too long to stay
/// readable as positional args once `register:`/`include:` needed threading through too.
struct RunEnv<'a> {
    playbook_dir: PathBuf,
    /// This playbook's `name:` (the outer one's, for `--repl` without a file: `"repl"`).
    /// Copied into an `include:`'s `sub_env` as *that* sub-playbook's own name — unlike
    /// `start_at`/`state_path`/`data_path`, an `--audit-log` entry for a task inside an
    /// `include:` should say which playbook it actually belongs to. See
    /// `write_audit_entry`.
    playbook_name: String,
    /// From `--audit-log`/`TOOLER_PLAY_AUDIT_LOG` — appends one JSON line per task
    /// attempt to this file if set (see `write_audit_entry`). Propagated into `include:`'s
    /// `sub_env` (like `dry`/`quiet`/`auto_yes`), so nested tasks are captured in the same
    /// trail.
    audit_log: Option<PathBuf>,
    project_root: PathBuf,
    dry: bool,
    /// From `--diff` — print a unified diff of what fs_write:/write_file: are about to
    /// change, right before each one applies its write. Propagated into `include:`'s
    /// `sub_env` (like `dry`/`quiet`/`auto_yes`).
    diff: bool,
    quiet: bool,
    /// From `--yes` — auto-confirms every `confirm:` task instead of prompting or (when
    /// `quiet`) failing fast.
    auto_yes: bool,
    /// From `--start-at-task` — set only on the top-level run's own `RunEnv`, never
    /// copied into an `include:`'s `sub_env`, so the skip only ever applies to the
    /// outermost playbook's own task list (see `execute_playbook`). `--resume` also goes
    /// through this same field — `run()` resolves it to a concrete task name upfront.
    start_at: Option<String>,
    /// Where to write/read this playbook's `--resume` checkpoint (`<file>.state.json`).
    /// `Some(...)` only on the top-level run's own `RunEnv`, `None` for `include:`'s
    /// `sub_env` — checkpointing, like `start_at`, is a top-level-only concept.
    state_path: Option<PathBuf>,
    /// Where `state_set:` persists `{{state.*}}` values (`<file>.data.json`). `Some(...)`
    /// only on the top-level run's own `RunEnv`, same top-level-only scoping `state_path`
    /// has and for the same reason — an `include:`'s `sub_env` shares the outer
    /// playbook's `vars` map already, so its `state.*` vars flow through for free with
    /// no extra plumbing; only the on-disk *persistence* is a top-level concept.
    data_path: Option<PathBuf>,
    ctx: &'a Context,
}

/// Merges a playbook's own `vars_files:` (in order — a later file overrides an earlier
/// one) with its inline `vars:` (which wins over all of them) into one map — the
/// playbook's own baseline vars, before any `--var`/include-time override is layered on
/// top. `dir` is the directory `vars_files:` paths resolve relative to (the playbook's
/// own directory, same as `run:`/`env_check:` paths). Used both for the top-level
/// playbook in `run()` and for an `include:`d sub-playbook.
fn load_playbook_vars(playbook: &Playbook, dir: &Path) -> Result<HashMap<String, String>> {
    let mut merged: HashMap<String, String> = HashMap::new();
    for vf in &playbook.vars_files {
        merged.extend(load_vars_file(&dir.join(vf))?);
    }
    merged.extend(playbook.vars.clone());
    Ok(merged)
}

/// Reads one flat `key: value` vars file (YAML, or JSON since it's valid YAML) — shared
/// by a playbook's own `vars_files:` entries (`load_playbook_vars`) and the CLI's
/// `--vars-file` (`apply_vars_file_overrides`). Transparently decrypts a file encrypted
/// via `tooler vault encrypt` first (no new syntax — detected by its magic header, same
/// as `tooler vault` itself), using the fixed `TOOLER_VAULT_PASSWORD` env var — always
/// that one name at playbook-run time, unlike `tooler vault`'s own `--password-env`,
/// which is only a convenience for encrypting/decrypting outside a playbook run.
fn load_vars_file(path: &Path) -> Result<HashMap<String, String>> {
    let raw = std::fs::read(path)
        .with_context(|| format!("Cannot read vars file: {}", path.display()))?;
    let content = if crate::commands::vault::is_vault_encrypted(&raw) {
        let password = std::env::var("TOOLER_VAULT_PASSWORD").with_context(|| {
            format!(
                "vars file '{}' is vault-encrypted; set TOOLER_VAULT_PASSWORD to decrypt it",
                path.display()
            )
        })?;
        let plaintext = crate::commands::vault::decrypt(&raw, &password)
            .with_context(|| format!("decrypting vars file: {}", path.display()))?;
        String::from_utf8(plaintext).with_context(|| {
            format!(
                "vars file '{}' is not valid UTF-8 after decrypting",
                path.display()
            )
        })?
    } else {
        String::from_utf8(raw)
            .with_context(|| format!("vars file '{}' is not valid UTF-8", path.display()))?
    };
    serde_yaml::from_str(&content)
        .with_context(|| format!("Invalid YAML in vars file: {}", path.display()))
}

/// Applies `--vars-file <path>` CLI overrides (repeatable, in order — a later file wins
/// on an overlapping key) on top of `vars`. Resolves paths relative to the current
/// directory (not the playbook's), same as any other CLI-supplied path. Applied *before*
/// `apply_var_overrides` at every call site, so a single `--var key=value` still wins
/// over anything a `--vars-file` set — the most specific override stays the strongest,
/// same precedence the playbook's own `vars_files:`/`vars:` already establish relative to
/// each other.
fn apply_vars_file_overrides(vars: &mut HashMap<String, String>, paths: &[PathBuf]) -> Result<()> {
    for path in paths {
        vars.extend(load_vars_file(path)?);
    }
    Ok(())
}

/// Applies `--var key=value` CLI overrides (repeatable) on top of `vars`, in order —
/// shared by a normal run, a `--resume`d run, and `--repl`.
fn apply_var_overrides(vars: &mut HashMap<String, String>, raw: &[String]) -> Result<()> {
    for var in raw {
        if let Some((k, v)) = var.split_once('=') {
            vars.insert(k.trim().to_string(), v.trim().to_string());
        } else {
            bail!("--var must be in key=value format, got: '{var}'");
        }
    }
    Ok(())
}

pub fn run(args: PlayArgs, ctx: &Context) -> Result<()> {
    let (_, project_root) = project::load()?;
    let playbooks_dir = project_root.join("playbooks");

    if args.init {
        let name = args.file.as_deref().unwrap_or("playbook");
        let path = if is_literal_path(name) {
            PathBuf::from(name)
        } else {
            std::fs::create_dir_all(&playbooks_dir)?;
            playbooks_dir.join(format!("{name}.yml"))
        };
        return write_sample(&path, ctx);
    }

    if args.repl {
        let (mut vars, playbook_dir, playbook_name) = match args.file.as_deref() {
            Some(file) => {
                let file_path = resolve_playbook_file(file, &project_root)?;
                let dir = file_path.parent().unwrap_or(Path::new(".")).to_path_buf();
                let content = std::fs::read_to_string(&file_path)
                    .with_context(|| format!("Cannot read playbook: {file}"))?;
                let playbook: Playbook = serde_yaml::from_str(&content)
                    .with_context(|| format!("Invalid YAML in {file}"))?;
                let vars = load_playbook_vars(&playbook, &dir)?;
                (vars, dir, playbook.name)
            }
            None => (HashMap::new(), PathBuf::from("."), "repl".to_string()),
        };
        apply_vars_file_overrides(&mut vars, &args.vars_file)?;
        apply_var_overrides(&mut vars, &args.vars)?;
        let env = RunEnv {
            playbook_dir,
            playbook_name,
            audit_log: args.audit_log.clone(),
            project_root,
            dry: args.dry,
            diff: args.diff,
            quiet: false,
            auto_yes: false,
            start_at: None,
            state_path: None,
            data_path: None,
            ctx,
        };
        return run_repl(vars, env);
    }

    let Some(file) = args.file.as_deref() else {
        return list_playbooks(&playbooks_dir, ctx);
    };

    if args.resume && args.start_at_task.is_some() {
        bail!("--resume and --start-at-task are mutually exclusive");
    }

    let file_path = resolve_playbook_file(file, &project_root)?;

    if args.notes {
        return print_notes_only(&file_path, ctx);
    }

    if args.list_tasks || args.list_tags {
        let content = std::fs::read_to_string(&file_path)
            .with_context(|| format!("Cannot read playbook: {file}"))?;
        let playbook: Playbook =
            serde_yaml::from_str(&content).with_context(|| format!("Invalid YAML in {file}"))?;
        let tag_filter: Option<Vec<&str>> = args
            .tags
            .as_deref()
            .map(|t| t.split(',').map(str::trim).collect());
        let skip_tag_filter: Option<Vec<&str>> = args
            .skip_tags
            .as_deref()
            .map(|t| t.split(',').map(str::trim).collect());
        let tasks: Vec<&Task> = playbook
            .tasks
            .iter()
            .filter(|t| task_matches_tags(t, &tag_filter, &skip_tag_filter))
            .collect();
        return if args.list_tasks {
            print_task_list(&playbook.name, &tasks, ctx)
        } else {
            print_tag_list(&tasks, ctx)
        };
    }

    let notes = read_notes(&file_path);

    let playbook_dir = file_path.parent().unwrap_or(Path::new(".")).to_path_buf();
    let state_path = state_path_for(&file_path);

    let content = std::fs::read_to_string(&file_path)
        .with_context(|| format!("Cannot read playbook: {file}"))?;

    let playbook: Playbook =
        serde_yaml::from_str(&content).with_context(|| format!("Invalid YAML in {file}"))?;

    // Not resuming: the usual fresh baseline — vars_files: (in order) merged under inline
    // vars:. Resuming: the checkpoint's vars *entirely* replace this baseline (it already
    // reflects vars_files:/vars: from the original run) — see `PlayCheckpoint`. Also
    // resolves the effective --start-at-task: the task right after the checkpoint's last
    // completed one, fed into the existing skip-ahead mechanism unchanged.
    let (mut vars, start_at_task) = if args.resume {
        if !state_path.exists() {
            bail!(
                "no checkpoint found at {} — nothing to resume; run without --resume",
                state_path.display()
            );
        }
        let checkpoint = load_checkpoint(&state_path)?;
        let idx = playbook
            .tasks
            .iter()
            .position(|t| t.name == checkpoint.last_completed_task)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "checkpoint's last completed task '{}' no longer exists in this playbook",
                    checkpoint.last_completed_task
                )
            })?;
        let next = playbook.tasks.get(idx + 1).ok_or_else(|| {
            anyhow::anyhow!(
                "nothing left to resume — '{file}' already completed all tasks per the \
                 checkpoint; delete {} to start over",
                state_path.display()
            )
        })?;
        (checkpoint.vars, Some(next.name.clone()))
    } else {
        (
            load_playbook_vars(&playbook, &playbook_dir)?,
            args.start_at_task.clone(),
        )
    };

    // --vars-file, then --var, apply on top either way — on a fresh run as always, and on
    // a resumed run so a bad value can be fixed before retrying (the whole point of
    // resuming rather than restarting from scratch).
    apply_vars_file_overrides(&mut vars, &args.vars_file)?;
    apply_var_overrides(&mut vars, &args.vars)?;

    // Independent of --resume/--start-at-task's checkpoint mechanism -- durable memory
    // from previous *successful* runs (see `data_path_for`), not a resume snapshot.
    // Loaded after --var overrides so a persisted value is what a later task's
    // `{{state.*}}` sees by default, same as any other var seeded before the playbook
    // starts.
    let data_path = data_path_for(&file_path);
    for (k, v) in load_persisted_state(&data_path)? {
        vars.insert(format!("state.{k}"), v);
    }

    let tag_filter: Option<Vec<&str>> = args
        .tags
        .as_deref()
        .map(|t| t.split(',').map(str::trim).collect());
    let skip_tag_filter: Option<Vec<&str>> = args
        .skip_tags
        .as_deref()
        .map(|t| t.split(',').map(str::trim).collect());

    let mut include_stack: Vec<PathBuf> = vec![file_path];
    let env = RunEnv {
        playbook_dir,
        playbook_name: playbook.name.clone(),
        audit_log: args.audit_log.clone(),
        project_root,
        dry: args.dry,
        diff: args.diff,
        quiet: ctx.output == OutputFormat::Json,
        auto_yes: args.yes,
        start_at: start_at_task,
        state_path: Some(state_path),
        data_path: Some(data_path),
        ctx,
    };

    execute_playbook(
        &playbook,
        &tag_filter,
        &skip_tag_filter,
        &notes,
        &mut vars,
        &mut include_stack,
        true,
        &env,
    )
}

// ── REPL ──────────────────────────────────────────────────────────────────────

/// Inserts `name: "<name>"` into `value` (must already be a `Mapping`) if it doesn't
/// already have a `name` key — the synthetic name every `--repl` line gets so it can
/// deserialize into `Task` (whose `name` field is required) without the user typing one.
fn merge_repl_name(value: &mut serde_yaml::Value, name: &str) {
    if let serde_yaml::Value::Mapping(map) = value {
        let key = serde_yaml::Value::String("name".to_string());
        if !map.contains_key(&key) {
            map.insert(key, serde_yaml::Value::String(name.to_string()));
        }
    }
}

fn print_repl_help() {
    println!("  Type one task action per line, e.g.:");
    println!("    run: echo hi");
    println!("    {{http: {{url: \"https://example.com\"}}, register: resp}}");
    println!("    {{set_fact: {{x: \"1\"}}}}");
    println!("  Any field a real playbook task supports works here too (when:, loop:,");
    println!("  register:, retries:, ignore_errors:, ...).");
    println!("  Commands:");
    println!("    .vars          show every current var");
    println!("    .clear         empty all vars");
    println!("    .save <path>   write this session as a playbook (relative to playbook dir)");
    println!("    .exit / .quit  end the session (Ctrl+D also works)");
}

fn print_repl_vars(vars: &HashMap<String, String>) {
    if vars.is_empty() {
        println!("  (no vars yet)");
        return;
    }
    let mut keys: Vec<&String> = vars.keys().collect();
    keys.sort();
    for k in keys {
        println!("  {} = {}", k.cyan(), vars[k].dimmed());
    }
}

/// Writes the accumulated session (the original parsed `Value`s, never round-tripped
/// through `Task` — so this needs no `Serialize` impl anywhere in this file) as a real
/// playbook file, resolved relative to `playbook_dir` like every other path in this file.
fn save_repl_session(session: &[serde_yaml::Value], playbook_dir: &Path, arg: &str) -> Result<()> {
    let out_path = playbook_dir.join(arg);
    let mut mapping = serde_yaml::Mapping::new();
    mapping.insert(
        serde_yaml::Value::String("name".to_string()),
        serde_yaml::Value::String("REPL session".to_string()),
    );
    mapping.insert(
        serde_yaml::Value::String("tasks".to_string()),
        serde_yaml::Value::Sequence(session.to_vec()),
    );
    let yaml = serde_yaml::to_string(&serde_yaml::Value::Mapping(mapping))
        .context("serializing REPL session")?;
    std::fs::write(&out_path, yaml).with_context(|| format!("writing {}", out_path.display()))?;
    println!(
        "  saved {} task(s) to {}",
        session.len(),
        out_path.display()
    );
    Ok(())
}

/// One meta-command or task-action prefix `--repl`'s tab-completion offers, matched
/// against the start of the current line — see `ReplHelper`.
const REPL_COMPLETIONS: &[&str] = &[
    ".help",
    ".vars",
    ".clear",
    ".save ",
    ".exit",
    ".quit",
    "run: ",
    "check_url: ",
    "check_port: ",
    "http: ",
    "scrape: ",
    "wait_for: ",
    "report: ",
    "env_check: ",
    "ssh: ",
    "fleet: ",
    "fs_cat: ",
    "fs_write: ",
    "systemd_restart: ",
    "systemd_status: ",
    "logs_tail: ",
    "logs_grep: ",
    "ps_list: ",
    "ps_kill: ",
    "stat: ",
    "git_summary: ",
    "git_changelog: ",
    "gh_prs: ",
    "include: ",
    "assert: ",
    "block:",
    "debug: ",
    "confirm: ",
    "set_fact: ",
    "state_set: ",
    "sync_db: ",
    "sync_files: ",
    "write_file: ",
    "read_csv: ",
    "write_csv: ",
    "db_query: ",
    "db_exec: ",
    "mail: ",
    "mail_check: ",
];

/// `--repl`'s `rustyline` helper: tab-completion only (`REPL_COMPLETIONS`, prefix-matched
/// against the whole line so far — covers the bare `run: ...`/`.command` line shapes, not
/// the flow-style `{action: ...}` one). Hinting/highlighting/validation are all left at
/// their default no-ops.
struct ReplHelper;

impl rustyline::completion::Completer for ReplHelper {
    type Candidate = String;
    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &rustyline::Context<'_>,
    ) -> rustyline::Result<(usize, Vec<String>)> {
        let prefix = &line[..pos];
        let matches = REPL_COMPLETIONS
            .iter()
            .filter(|c| c.starts_with(prefix))
            .map(|c| c.to_string())
            .collect();
        Ok((0, matches))
    }
}
impl rustyline::hint::Hinter for ReplHelper {
    type Hint = String;
}
impl rustyline::highlight::Highlighter for ReplHelper {}
impl rustyline::validate::Validator for ReplHelper {}
impl rustyline::Helper for ReplHelper {}

/// `--repl`'s history file (arrow-key recall within a session, persisted across them) —
/// `~/.tooler/repl_history`, the same `~/.tooler/` directory `config::config_path()`
/// already uses. `None` if the home directory can't be resolved; history then still works
/// for the current session, it just isn't persisted.
fn repl_history_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".tooler").join("repl_history"))
}

/// The `--repl` loop: reads one line at a time (a task action, minus `name:` — see
/// `PlayArgs.repl`'s doc comment for the two accepted shapes), executes it immediately
/// against `vars`/`env` via the exact same `run_task` a real playbook run uses, and keeps
/// going even after a failing line — unlike a batch `tooler play` run, one bad REPL line
/// must not end the session. `.help` lists the meta-commands. Arrow-key history (in-session
/// and persisted across sessions) and Tab-completion of action/meta-command prefixes come
/// from `rustyline`; it degrades to plain line reads when stdin isn't a real terminal (a
/// piped/scripted session), so a non-interactive `--repl` invocation keeps working exactly
/// as before.
fn run_repl(mut vars: HashMap<String, String>, env: RunEnv) -> Result<()> {
    println!(
        "{}",
        "tooler play --repl — type a task action, or .help for commands. Ctrl+D / .exit to quit."
            .dimmed()
    );

    let mut include_stack: Vec<PathBuf> = Vec::new();
    let mut session: Vec<serde_yaml::Value> = Vec::new();
    let mut counter = 0usize;

    let history_path = repl_history_path();
    let mut editor: Editor<ReplHelper, DefaultHistory> = Editor::new()?;
    editor.set_helper(Some(ReplHelper));
    if let Some(path) = &history_path {
        let _ = editor.load_history(path);
    }

    loop {
        let line = match editor.readline("tooler-repl> ") {
            Ok(l) => l,
            Err(ReadlineError::Interrupted) => continue, // Ctrl+C: cancel this line, stay in the REPL
            Err(ReadlineError::Eof) => break,            // Ctrl+D
            Err(e) => {
                println!("  readline error: {e}");
                break;
            }
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let _ = editor.add_history_entry(line);

        if let Some(rest) = line.strip_prefix('.') {
            let mut parts = rest.splitn(2, char::is_whitespace);
            let cmd = parts.next().unwrap_or("");
            let arg = parts.next().map(str::trim).unwrap_or("");
            match cmd {
                "exit" | "quit" => break,
                "help" => print_repl_help(),
                "vars" => print_repl_vars(&vars),
                "clear" => {
                    vars.clear();
                    println!("  vars cleared");
                }
                "save" if arg.is_empty() => println!("  usage: .save <path>"),
                "save" => save_repl_session(&session, &env.playbook_dir, arg)?,
                other => println!("  unknown command: .{other} (try .help)"),
            }
            continue;
        }

        let mut value: serde_yaml::Value = match serde_yaml::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                println!("  invalid YAML: {e}");
                continue;
            }
        };
        if !value.is_mapping() {
            println!(
                "  expected a task action, e.g. `run: echo hi` or \
                 `{{http: {{url: \"...\"}}, register: x}}` — .help for more"
            );
            continue;
        }
        counter += 1;
        merge_repl_name(&mut value, &format!("repl-{counter}"));

        let task: Task = match serde_yaml::from_value(value.clone()) {
            Ok(t) => t,
            Err(e) => {
                println!("  invalid task: {e}");
                continue;
            }
        };

        // execute_playbook normally evaluates when: one level up, before calling
        // run_task — replicated here since the REPL calls run_task directly.
        if let Some(w) = &task.when
            && !eval_when(w, &vars)
        {
            println!("  (skipped — when: {w} was false)");
            session.push(value);
            continue;
        }

        // Only recorded into the .save-able session on success (or an explicitly
        // ignore_errors:'d failure) — a hard failure is very often a typo/mistake being
        // actively debugged, and .save shouldn't bake a task that's known to fail straight
        // back into a "clean" playbook file.
        match run_task(&task, &mut vars, &mut include_stack, &env) {
            Ok(()) => {
                session.push(value);
                if let Some(reg) = &task.register {
                    let val = vars.get(reg).map(String::as_str).unwrap_or("");
                    println!("  {} = {}", reg.cyan(), val.dimmed());
                }
            }
            Err(e) => {
                if task.ignore_errors {
                    session.push(value);
                    println!("  {} failed (ignored): {e}", "!".yellow().bold());
                } else {
                    println!("  {} {e}", "✗".red().bold());
                }
            }
        }
    }

    if let Some(path) = &history_path {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = editor.save_history(path);
    }

    Ok(())
}

// ── Runner ────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct TaskOutcome {
    name: String,
    status: &'static str,
    error: Option<String>,
}

/// `--resume`'s on-disk checkpoint — a sibling of the playbook file (`<file>.state.json`,
/// see `state_path_for`), written after every top-level task's non-fatal outcome and
/// deleted on full success. `vars` is the *entire* vars map at that point, which can
/// include values resolved from `{{secret.*}}` (e.g. via `set_fact:`) — see
/// `write_checkpoint`'s 0600-permission handling.
#[derive(Debug, Serialize, Deserialize)]
struct PlayCheckpoint {
    playbook: String,
    last_completed_task: String,
    vars: HashMap<String, String>,
    updated_at: String,
}

/// The `--resume` checkpoint path for a given playbook file: the file's own path with
/// `.state.json` appended (e.g. `playbooks/deploy.yml` -> `playbooks/deploy.yml.state.json`).
fn state_path_for(file_path: &Path) -> PathBuf {
    let mut s = file_path.as_os_str().to_os_string();
    s.push(".state.json");
    PathBuf::from(s)
}

/// Where `state_set:`'s persisted values live -- `<file>.data.json`, a sibling of the
/// playbook, deliberately a *different* suffix from `state_path_for`'s `.state.json`
/// (the `--resume` checkpoint): that file is ephemeral (deleted on full success, exists
/// to resume one specific failed run); this one is durable and never auto-deleted,
/// meant to carry memory forward across many separate *successful* runs (e.g. once a day
/// via `tooler cron local`).
fn data_path_for(file_path: &Path) -> PathBuf {
    let mut s = file_path.as_os_str().to_os_string();
    s.push(".data.json");
    PathBuf::from(s)
}

/// Loads `<file>.data.json` if it exists (a missing file is an empty map, not an error --
/// the common case on a playbook's first run) into a flat, unprefixed `HashMap`. Callers
/// insert each entry into `vars` under a `state.<key>` prefix so `{{state.<key>}}`
/// resolves through the ordinary `vars.get(token)` branch of `resolve_token` -- no
/// changes needed to `render`/`resolve_token` themselves.
fn load_persisted_state(path: &Path) -> Result<HashMap<String, String>> {
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("reading persisted state {}", path.display()))?;
    serde_json::from_str(&content)
        .with_context(|| format!("parsing persisted state {}", path.display()))
}

/// Writes every `state.`-prefixed `vars` entry (stripped of that prefix) back to
/// `env.data_path` as a flat JSON map, `0600` on Unix (same posture `write_checkpoint`
/// already has for `--resume`'s checkpoint -- a `state_set:` value resolved from
/// `{{secret.*}}` ends up here too). A no-op when `env.data_path` is `None` (an
/// `include:`'s `sub_env`, or `--repl` -- state persistence, like `--resume`
/// checkpointing, is a top-level-playbook-file concept). Called immediately after every
/// `state_set:`, not batched, so state already set survives even a later task's crash --
/// same "durable as you go" philosophy `write_checkpoint` already has.
fn write_persisted_state(env: &RunEnv, vars: &HashMap<String, String>) {
    let Some(path) = &env.data_path else {
        return;
    };
    let state: HashMap<&str, &str> = vars
        .iter()
        .filter_map(|(k, v)| k.strip_prefix("state.").map(|key| (key, v.as_str())))
        .collect();
    let result = serde_json::to_string_pretty(&state)
        .map_err(anyhow::Error::from)
        .and_then(|json| {
            std::fs::write(path, json)
                .with_context(|| format!("writing persisted state {}", path.display()))
        });
    if let Err(e) = result {
        if !env.quiet {
            println!("  {}", format!("(state not saved: {e})").dimmed());
        }
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
}

fn load_checkpoint(path: &Path) -> Result<PlayCheckpoint> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("reading checkpoint {}", path.display()))?;
    serde_json::from_str(&content)
        .with_context(|| format!("invalid checkpoint at {}", path.display()))
}

/// Best-effort: snapshots `vars` to `env.state_path` (a no-op if unset, i.e. not the
/// top-level run) so a later `--resume` can pick up right after `last_completed_task`. A
/// write failure prints a dimmed warning (if not quiet) but never fails the task — a
/// checkpoint hiccup must never sink an otherwise-successful run. On Unix the file is
/// chmod'd 0600 right after writing: `vars` can hold values resolved from `{{secret.*}}`,
/// making this a file worth protecting the same way any other local credential material
/// is (see the `--resume` README section for the full caveat).
fn write_checkpoint(
    env: &RunEnv,
    playbook_name: &str,
    last_completed_task: &str,
    vars: &HashMap<String, String>,
) {
    let Some(path) = &env.state_path else {
        return;
    };
    let checkpoint = PlayCheckpoint {
        playbook: playbook_name.to_string(),
        last_completed_task: last_completed_task.to_string(),
        vars: vars.clone(),
        updated_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
    };
    let result = serde_json::to_string_pretty(&checkpoint)
        .map_err(anyhow::Error::from)
        .and_then(|json| {
            std::fs::write(path, json)
                .with_context(|| format!("writing checkpoint {}", path.display()))
        });
    if let Err(e) = result {
        if !env.quiet {
            println!("  {}", format!("(checkpoint not saved: {e})").dimmed());
        }
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
}

/// Rejects a `notify:` name with no matching `playbook.handlers` entry upfront, rather
/// than silently never running it — walks into `block:`/`rescue:`/`always:` too, since
/// those tasks can `notify:` just like any other.
fn validate_handlers(playbook: &Playbook) -> Result<()> {
    let handler_names: std::collections::HashSet<&str> =
        playbook.handlers.iter().map(|h| h.name.as_str()).collect();

    fn walk(tasks: &[Task], handler_names: &std::collections::HashSet<&str>) -> Result<()> {
        for t in tasks {
            for n in &t.notify {
                if !handler_names.contains(n.as_str()) {
                    bail!("task '{}' notifies unknown handler '{}'", t.name, n);
                }
            }
            if let Some(b) = &t.block {
                walk(b, handler_names)?;
            }
            if let Some(r) = &t.rescue {
                walk(r, handler_names)?;
            }
            if let Some(a) = &t.always {
                walk(a, handler_names)?;
            }
        }
        Ok(())
    }
    walk(&playbook.tasks, &handler_names)
}

/// Whether a task passes `--tags`/`--skip-tags` filtering: matches `--tags` (if set,
/// needs at least one overlapping tag) and matches none of `--skip-tags`. Only ever
/// applied to a playbook's own top-level tasks — same scoping `tag_filter` already has
/// (see `execute_playbook`), not nested `block:`/`rescue:`/`always:`/`include:` tasks.
/// Shared by `execute_playbook`'s task selection and `--list-tasks`/`--list-tags` so the
/// two stay consistent.
/// A task tagged `always` is included even when `--tags` wouldn't otherwise select it —
/// an escape hatch for a cleanup/logging task that should never be skipped by tag
/// filtering, the same special tag real Ansible has. It's still excluded by an explicit
/// `--skip-tags always` (or any other tag it also carries that's in `--skip-tags`) —
/// `always` only ever widens what `--tags` selects, it never overrides `--skip-tags`.
fn task_matches_tags(
    task: &Task,
    tag_filter: &Option<Vec<&str>>,
    skip_tag_filter: &Option<Vec<&str>>,
) -> bool {
    let always = task.tags.iter().any(|t| t == "always");
    let included = match tag_filter {
        None => true,
        Some(tags) => always || task.tags.iter().any(|t| tags.contains(&t.as_str())),
    };
    let excluded = match skip_tag_filter {
        None => false,
        Some(skip) => task.tags.iter().any(|t| skip.contains(&t.as_str())),
    };
    included && !excluded
}

#[allow(clippy::too_many_arguments)]
fn execute_playbook(
    playbook: &Playbook,
    tag_filter: &Option<Vec<&str>>,
    skip_tag_filter: &Option<Vec<&str>>,
    notes: &Option<String>,
    vars: &mut HashMap<String, String>,
    include_stack: &mut Vec<PathBuf>,
    is_top_level: bool,
    env: &RunEnv,
) -> Result<()> {
    validate_handlers(playbook)?;

    let json = env.quiet;
    let sep = "─".repeat(56);

    if !json {
        println!(
            "\n{} {} {}",
            "PLAY".bold().cyan(),
            format!("[{}]", playbook.name).bold(),
            env.playbook_dir.display().to_string().dimmed()
        );
        if let Some(desc) = &playbook.description {
            println!("     {}", desc.dimmed());
        }
        println!("{}", sep.dimmed());
        if let Some(n) = notes {
            println!("\n{}", "NOTES".bold().yellow());
            println!("{}", sep.dimmed());
            for line in n.trim_end().lines() {
                println!("{line}");
            }
            println!("{}", sep.dimmed());
        }
    }

    let mut tasks: Vec<&Task> = playbook
        .tasks
        .iter()
        .filter(|t| task_matches_tags(t, tag_filter, skip_tag_filter))
        .collect();

    // --start-at-task: only ever set on the top-level RunEnv (see RunEnv.start_at), so
    // this only skips ahead in the outermost playbook's own task list.
    if let Some(start) = &env.start_at {
        let idx = tasks.iter().position(|t| &t.name == start).ok_or_else(|| {
            anyhow::anyhow!(
                "no task named '{start}' (check `tooler play <file> --dry` for task names)"
            )
        })?;
        if idx > 0 {
            if !json {
                println!(
                    "  {}",
                    format!("(starting at task '{start}' — {idx} earlier task(s) skipped)")
                        .dimmed()
                );
            }
            tasks.drain(..idx);
        }
    }

    let total = tasks.len();
    let mut ok = 0usize;
    let mut failed = 0usize;
    let mut skipped = 0usize;
    let mut outcomes: Vec<TaskOutcome> = Vec::new();
    let mut notified: Vec<String> = Vec::new();

    for (i, task) in tasks.iter().enumerate() {
        if !json {
            println!(
                "\n{} [{}/{}] {}",
                "TASK".bold().yellow(),
                i + 1,
                total,
                task.name.bold()
            );
        }

        if let Some(w) = &task.when
            && !eval_when(w, vars)
        {
            if !json {
                println!("  {}", format!("(skipped — when: {w} was false)").dimmed());
            }
            skipped += 1;
            outcomes.push(TaskOutcome {
                name: task.name.clone(),
                status: "skipped",
                error: None,
            });
            if is_top_level && !env.dry {
                write_checkpoint(env, &playbook.name, &task.name, vars);
            }
            continue;
        }

        if task.flush_handlers {
            if env.dry {
                if !json {
                    println!("  {}", "(dry run — skipped)".dimmed());
                }
                skipped += 1;
                outcomes.push(TaskOutcome {
                    name: task.name.clone(),
                    status: "skipped",
                    error: None,
                });
            } else {
                run_notified_handlers(
                    playbook,
                    &mut notified,
                    vars,
                    include_stack,
                    env,
                    json,
                    notes,
                    &sep,
                    is_top_level,
                    &mut ok,
                    &mut failed,
                    skipped,
                    &mut outcomes,
                )?;
                ok += 1;
                outcomes.push(TaskOutcome {
                    name: task.name.clone(),
                    status: "ok",
                    error: None,
                });
            }
            if is_top_level && !env.dry {
                write_checkpoint(env, &playbook.name, &task.name, vars);
            }
            continue;
        }

        let result = run_task(task, vars, include_stack, env);

        match result {
            Ok(_) => {
                if env.dry {
                    if !json {
                        println!("  {}", "(dry run — skipped)".dimmed());
                    }
                    skipped += 1;
                    outcomes.push(TaskOutcome {
                        name: task.name.clone(),
                        status: "skipped",
                        error: None,
                    });
                } else {
                    ok += 1;
                    outcomes.push(TaskOutcome {
                        name: task.name.clone(),
                        status: "ok",
                        error: None,
                    });

                    let changed = match &task.changed_when {
                        Some(expr) => eval_when(expr, vars),
                        None => true,
                    };
                    if changed {
                        for h in &task.notify {
                            if !notified.contains(h) {
                                notified.push(h.clone());
                            }
                        }
                    }
                }
            }
            Err(e) => {
                if task.ignore_errors {
                    if !json {
                        println!("  {} failed (ignored): {e}", "!".yellow().bold());
                    }
                    skipped += 1;
                    outcomes.push(TaskOutcome {
                        name: task.name.clone(),
                        status: "ignored",
                        error: Some(e.to_string()),
                    });
                } else {
                    failed += 1;
                    outcomes.push(TaskOutcome {
                        name: task.name.clone(),
                        status: "failed",
                        error: Some(e.to_string()),
                    });

                    if json {
                        println!(
                            "{}",
                            serde_json::json!({
                                "playbook": playbook.name,
                                "dry": env.dry,
                                "notes": notes,
                                "tasks": outcomes,
                                "ok": ok,
                                "failed": failed,
                                "skipped": skipped,
                                "success": false,
                            })
                        );
                        if is_top_level {
                            std::process::exit(1);
                        }
                        bail!("playbook failed");
                    }

                    println!("  {} {}", "✗".red().bold(), e.to_string().red());
                    println!("\n{}", sep.dimmed());
                    println!(
                        "\n{} failed at task \"{}\". {}",
                        "PLAY".bold().red(),
                        task.name.bold(),
                        "Remaining tasks skipped.".dimmed()
                    );
                    print_recap(ok, failed, skipped);
                    bail!("playbook failed");
                }
            }
        }

        // Reached for every non-fatal outcome above (dry-skip, real success, ignored
        // failure) — a hard failure already returned/exited inside the match. Never
        // checkpoints in --dry, since dry mode does no real work to resume from.
        if is_top_level && !env.dry {
            write_checkpoint(env, &playbook.name, &task.name, vars);
        }
    }

    // Reached only if every regular task above succeeded (or was skipped/ignored) —
    // any unhandled failure already returned or exited above. Any handler a
    // flush_handlers: task didn't already run mid-playbook runs now.
    run_notified_handlers(
        playbook,
        &mut notified,
        vars,
        include_stack,
        env,
        json,
        notes,
        &sep,
        is_top_level,
        &mut ok,
        &mut failed,
        skipped,
        &mut outcomes,
    )?;

    // A fully-completed playbook has nothing left to resume — best-effort, never fails
    // the run over a stray delete error. Never touches the checkpoint in --dry: a dry
    // run does no real work, so it must not discard a real checkpoint from an earlier
    // failed run just because a preview happened to "succeed" afterward.
    if is_top_level
        && !env.dry
        && let Some(path) = &env.state_path
    {
        let _ = std::fs::remove_file(path);
    }

    if json {
        println!(
            "{}",
            serde_json::json!({
                "playbook": playbook.name,
                "dry": env.dry,
                "notes": notes,
                "tasks": outcomes,
                "ok": ok,
                "failed": failed,
                "skipped": skipped,
                "success": true,
            })
        );
        return Ok(());
    }

    println!("\n{}", sep.dimmed());
    print_recap(ok, failed, skipped);
    Ok(())
}

/// Runs every currently-pending `notify:`ed handler and drains `notified` — shared by
/// `execute_playbook`'s natural end-of-run flush and a mid-run `flush_handlers:` task
/// (see its dispatch point above). A handler failure fails the whole playbook the same
/// way a regular task failure does (same JSON/text reporting shape), addressed to the
/// handler instead of an indexed task.
#[allow(clippy::too_many_arguments)]
fn run_notified_handlers(
    playbook: &Playbook,
    notified: &mut Vec<String>,
    vars: &mut HashMap<String, String>,
    include_stack: &mut Vec<PathBuf>,
    env: &RunEnv,
    json: bool,
    notes: &Option<String>,
    sep: &str,
    is_top_level: bool,
    ok: &mut usize,
    failed: &mut usize,
    skipped: usize,
    outcomes: &mut Vec<TaskOutcome>,
) -> Result<()> {
    for name in notified.drain(..) {
        let handler = playbook
            .handlers
            .iter()
            .find(|h| h.name == name)
            .expect("validated by validate_handlers");

        if !json {
            println!("\n{} [{}]", "HANDLER".bold().magenta(), handler.name.bold());
        }

        match run_task(handler, vars, include_stack, env) {
            Ok(()) => {
                *ok += 1;
                outcomes.push(TaskOutcome {
                    name: handler.name.clone(),
                    status: "ok",
                    error: None,
                });
            }
            Err(e) => {
                *failed += 1;
                outcomes.push(TaskOutcome {
                    name: handler.name.clone(),
                    status: "failed",
                    error: Some(e.to_string()),
                });

                if json {
                    println!(
                        "{}",
                        serde_json::json!({
                            "playbook": playbook.name,
                            "dry": env.dry,
                            "notes": notes,
                            "tasks": outcomes,
                            "ok": *ok,
                            "failed": *failed,
                            "skipped": skipped,
                            "success": false,
                        })
                    );
                    if is_top_level {
                        std::process::exit(1);
                    }
                    bail!("playbook failed");
                }

                println!("  {} {}", "✗".red().bold(), e.to_string().red());
                println!("\n{}", sep.dimmed());
                println!(
                    "\n{} failed at handler \"{}\".",
                    "PLAY".bold().red(),
                    handler.name.bold()
                );
                print_recap(*ok, *failed, skipped);
                bail!("playbook failed");
            }
        }
    }
    Ok(())
}

/// A minimal condition language over `render()`-substituted strings: "<a> == <b>",
/// "<a> != <b>", "<a> >= <b>", "<a> <= <b>", "<a> > <b>", "<a> < <b>", or a bare truthy
/// check. Not a full expression language — matches tooler's existing plain `{{var}}`
/// templating rather than adding a new one. `>=`/`<=` are checked before the
/// single-character `>`/`<` so `"5 >= 3"` doesn't get wrongly split on the bare `>` into
/// `"5 "`/`"= 3"`.
fn eval_when(expr: &str, vars: &HashMap<String, String>) -> bool {
    let rendered = render(expr, vars);
    let rendered = rendered.trim();
    if let Some((lhs, rhs)) = rendered.split_once(">=") {
        return compare_numeric(lhs, rhs, |a, b| a >= b);
    }
    if let Some((lhs, rhs)) = rendered.split_once("<=") {
        return compare_numeric(lhs, rhs, |a, b| a <= b);
    }
    if let Some((lhs, rhs)) = rendered.split_once("!=") {
        return lhs.trim() != rhs.trim();
    }
    if let Some((lhs, rhs)) = rendered.split_once("==") {
        return lhs.trim() == rhs.trim();
    }
    if let Some((lhs, rhs)) = rendered.split_once('>') {
        return compare_numeric(lhs, rhs, |a, b| a > b);
    }
    if let Some((lhs, rhs)) = rendered.split_once('<') {
        return compare_numeric(lhs, rhs, |a, b| a < b);
    }
    !rendered.is_empty() && rendered != "false" && rendered != "0"
}

/// Backs `eval_when`'s numeric operators. Parses both sides as `f64`; if either isn't a
/// number, the comparison is `false` rather than a guess — the same "don't pretend to
/// know" default this DSL already uses elsewhere (e.g. a `scrape:` field that doesn't
/// match becomes `""`, not an error or a wrong answer).
fn compare_numeric(lhs: &str, rhs: &str, op: impl Fn(f64, f64) -> bool) -> bool {
    match (lhs.trim().parse::<f64>(), rhs.trim().parse::<f64>()) {
        (Ok(a), Ok(b)) => op(a, b),
        _ => false,
    }
}

/// Expands `loop:` (if present) into one retried-`run_task_once` call per item, with
/// `{{item}}` added to that iteration's vars. The first failing iteration (after its own
/// retries are exhausted) fails the whole task — remaining items are not attempted. If
/// `register:` is set, `<reg>` gets the *last* iteration's captured value (simplest
/// well-defined rule for a loop+register combination) and `<reg>.results` gets a JSON
/// array of every iteration's value, in order — see `Task::register`.
fn run_task(
    task: &Task,
    vars: &mut HashMap<String, String>,
    include_stack: &mut Vec<PathBuf>,
    env: &RunEnv,
) -> Result<()> {
    if task.max_parallel.is_some() && task.loop_spec.is_none() {
        bail!("max_parallel: is only supported combined with loop:");
    }
    if task.continue_on_error && task.loop_spec.is_none() {
        bail!("continue_on_error: is only supported combined with loop:");
    }
    let Some(spec) = &task.loop_spec else {
        return run_task_once_with_retries(task, vars, include_stack, env);
    };
    let items = resolve_loop_items(spec, vars);

    if let Some(chunk_size) = task.max_parallel.filter(|&n| n > 1) {
        return run_loop_parallel(
            task,
            &items,
            chunk_size,
            vars,
            include_stack.as_slice(),
            env,
        );
    }

    let mut results: Vec<String> = Vec::new();
    let mut failures: Vec<String> = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let mut loop_vars = vars.clone();
        apply_loop_item(item, &mut loop_vars, env.quiet);
        match run_task_once_with_retries(task, &mut loop_vars, include_stack, env) {
            Ok(()) => {
                if let Some(reg) = &task.register {
                    let val = loop_vars.get(reg).cloned().unwrap_or_default();
                    vars.insert(reg.clone(), val.clone());
                    results.push(val);
                }
            }
            Err(e) if task.continue_on_error => {
                failures.push(format!("{}: {e}", loop_item_label(index + 1, item)));
                if task.register.is_some() {
                    results.push(String::new());
                }
            }
            Err(e) => return Err(e),
        }
    }
    if let Some(reg) = &task.register {
        let json =
            serde_json::to_string(&results).expect("serializing a Vec<String> to JSON cannot fail");
        vars.insert(format!("{reg}.results"), json);
    }
    if !failures.is_empty() {
        bail!(
            "{} of {} loop item(s) failed: {}",
            failures.len(),
            items.len(),
            failures.join("; ")
        );
    }
    Ok(())
}

/// A short label for a loop item in a `continue_on_error:` failure summary — the item's
/// own value for a scalar, or just its position for a map (whose fields vary task to
/// task, so there's no one obviously-right field to show). Shared by `run_task`/
/// `run_loop_parallel`.
fn loop_item_label(index: usize, item: &LoopItem) -> String {
    match item {
        LoopItem::Scalar(s) => format!("item {index} ({s})"),
        LoopItem::Map(_) => format!("item {index}"),
    }
}

/// Inserts one `loop:` item's `{{item}}`/`{{item.<field>}}` var(s) into `loop_vars` and
/// echoes the `→ item=...` line — the per-iteration setup shared by both the sequential
/// and the parallel (`max_parallel:`) `loop:` paths.
fn apply_loop_item(item: &LoopItem, loop_vars: &mut HashMap<String, String>, quiet: bool) {
    match item {
        LoopItem::Scalar(s) => {
            loop_vars.insert("item".to_string(), s.clone());
            if !quiet {
                println!("  {} item={}", "→".dimmed(), s.dimmed());
            }
        }
        LoopItem::Map(m) => {
            for (k, v) in m {
                loop_vars.insert(format!("item.{k}"), v.clone());
            }
            if !quiet {
                let joined = m
                    .iter()
                    .map(|(k, v)| format!("item.{k}={v}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                println!("  {} {joined}", "→".dimmed());
            }
        }
    }
}

/// Runs `items` in chunks of `chunk_size`, all items within a chunk concurrently (one
/// thread each, via `std::thread::scope` — the same primitive `fleet::run_on_targets`'s
/// `parallel: true` branch uses), the next chunk only starting once the current one fully
/// joins. Each thread gets its own cloned `vars`/`include_stack` — a parallel loop item is
/// its own independent branch, so it doesn't need (and, for `include:`'s cycle detection,
/// shouldn't share) the others' mutable state.
///
/// Results are consumed in **original item order**, not completion order, so the
/// `register:`-captures-the-last-iteration's-value rule (and `<reg>.results`, the JSON
/// array of every iteration's value — see `Task::register`) stays deterministic despite
/// concurrent execution, and the *first* error in original order fails the task — matching
/// the sequential loop's "first failing iteration fails the task" contract as closely as
/// concurrency allows. One narrowing of that guarantee: within a chunk that contains a
/// failing item, that chunk's other already-started items still run to completion (they
/// can't be cancelled mid-flight) even though the task as a whole is reported failed.
fn run_loop_parallel(
    task: &Task,
    items: &[LoopItem],
    chunk_size: usize,
    vars: &mut HashMap<String, String>,
    include_stack: &[PathBuf],
    env: &RunEnv,
) -> Result<()> {
    let mut last_registered: Option<String> = None;
    let mut all_registered: Vec<String> = Vec::new();
    let mut failures: Vec<String> = Vec::new();
    let mut index = 0usize;
    for chunk in items.chunks(chunk_size) {
        let results: Vec<Result<Option<String>>> = std::thread::scope(|scope| {
            let handles: Vec<_> = chunk
                .iter()
                .map(|item| {
                    let mut loop_vars = vars.clone();
                    let mut stack = include_stack.to_vec();
                    apply_loop_item(item, &mut loop_vars, env.quiet);
                    scope.spawn(move || {
                        run_task_once_with_retries(task, &mut loop_vars, &mut stack, env)?;
                        Ok(task
                            .register
                            .as_ref()
                            .and_then(|r| loop_vars.get(r).cloned()))
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        for (item, r) in chunk.iter().zip(results) {
            index += 1;
            match r {
                Ok(val) => {
                    if let Some(v) = &val {
                        all_registered.push(v.clone());
                    }
                    if val.is_some() {
                        last_registered = val;
                    }
                }
                Err(e) if task.continue_on_error => {
                    failures.push(format!("{}: {e}", loop_item_label(index, item)));
                    if task.register.is_some() {
                        all_registered.push(String::new());
                    }
                }
                Err(e) => return Err(e),
            }
        }
    }
    if let Some(reg) = &task.register {
        if let Some(val) = last_registered {
            vars.insert(reg.clone(), val);
        }
        let json = serde_json::to_string(&all_registered)
            .expect("serializing a Vec<String> to JSON cannot fail");
        vars.insert(format!("{reg}.results"), json);
    }
    if !failures.is_empty() {
        bail!(
            "{} of {} loop item(s) failed: {}",
            failures.len(),
            items.len(),
            failures.join("; ")
        );
    }
    Ok(())
}

/// Retries a single (non-loop-expanded) task invocation up to `task.retries` extra times,
/// waiting `task.delay` (default 1s) between attempts. A no-op wrapper when `retries:`
/// isn't set (attempts=1) or in `--dry` (nothing ever fails in dry mode, since every
/// action's real work is itself gated on `!env.dry`). Also the single funnel every
/// concrete task attempt passes through regardless of nesting (top-level, `block:`/
/// `rescue:`/`always:`, `include:`, each `loop:` iteration — sequential or parallel) —
/// see `write_audit_entry`, called here so `--audit-log` covers all of them uniformly.
fn run_task_once_with_retries(
    task: &Task,
    vars: &mut HashMap<String, String>,
    include_stack: &mut Vec<PathBuf>,
    env: &RunEnv,
) -> Result<()> {
    let start = Instant::now();
    let attempts = task.retries.unwrap_or(0) + 1;
    for attempt in 1..=attempts {
        // failed_when: converts an otherwise-successful attempt into the same Err path
        // a real failure takes, *before* the Ok/Err match below — so retry-on-error,
        // the final "give up" bail, and the audit-log entry are all reused verbatim.
        // until: (below) is only ever checked when failed_when: did NOT trigger, same
        // precedence Ansible's failed_when/until interaction follows.
        let outcome = match run_task_once(task, vars, include_stack, env) {
            Ok(())
                if !env.dry
                    && task
                        .failed_when
                        .as_deref()
                        .is_some_and(|expr| eval_when(expr, vars)) =>
            {
                Err(anyhow!(
                    "failed_when: '{}' was true",
                    task.failed_when.as_deref().unwrap()
                ))
            }
            other => other,
        };
        match outcome {
            Ok(()) => {
                let satisfied = match &task.until {
                    None => true,
                    Some(expr) => env.dry || eval_when(expr, vars),
                };
                if satisfied {
                    let status = if env.dry { "dry" } else { "ok" };
                    write_audit_entry(env, task, status, None, start.elapsed());
                    return Ok(());
                }
                if attempt < attempts {
                    let delay = task.delay.unwrap_or(1);
                    if !env.quiet {
                        println!(
                            "  {} attempt {attempt}/{attempts}: until: '{}' not yet true — \
                             retrying in {delay}s...",
                            "!".yellow().bold(),
                            task.until.as_deref().unwrap()
                        );
                    }
                    std::thread::sleep(Duration::from_secs(delay));
                    continue;
                }
                let msg = format!(
                    "until: '{}' was still false after {attempts} attempt(s)",
                    task.until.as_deref().unwrap()
                );
                write_audit_entry(env, task, "failed", Some(&msg), start.elapsed());
                bail!("{msg}");
            }
            Err(e) if attempt < attempts && !env.dry => {
                let delay = task.delay.unwrap_or(1);
                if !env.quiet {
                    println!(
                        "  {} attempt {attempt}/{attempts} failed: {e} — retrying in {delay}s...",
                        "!".yellow().bold()
                    );
                }
                std::thread::sleep(Duration::from_secs(delay));
            }
            Err(e) => {
                write_audit_entry(env, task, "failed", Some(&e.to_string()), start.elapsed());
                return Err(e);
            }
        }
    }
    unreachable!("loop always returns on the last attempt")
}

/// Resolves an `include:` target the same way a top-level playbook argument is
/// resolved, except a literal path is relative to *this playbook's own directory*
/// (`env.playbook_dir`, matching how `run:`/`env_check:` paths already resolve) rather
/// than the process's CWD — `include: ./helpers/build.yml` means "next to me."
fn resolve_include_path(file: &str, env: &RunEnv) -> Result<PathBuf> {
    if is_literal_path(file) {
        return env
            .playbook_dir
            .join(file)
            .canonicalize()
            .with_context(|| format!("Cannot resolve include: {file}"));
    }
    let dir = env.project_root.join("playbooks");
    for ext in ["yml", "yaml"] {
        let candidate = dir.join(format!("{file}.{ext}"));
        if candidate.exists() {
            return candidate
                .canonicalize()
                .with_context(|| format!("Cannot resolve include: {}", candidate.display()));
        }
    }
    bail!(
        "No playbook named '{file}' in {} for include (looked for {file}.yml, {file}.yaml)",
        dir.display()
    );
}

/// Runs a list of tasks in order (used for `block:`/`rescue:`/`always:`), stopping at
/// the first unhandled failure — the same when:/loop:/retries:/register: support as the
/// top-level playbook loop, just without its JSON-summary/recap bookkeeping, since a
/// `block:` counts as a single outcome from its caller's perspective (see `run_block`).
fn run_task_sequence(
    tasks: &[Task],
    vars: &mut HashMap<String, String>,
    include_stack: &mut Vec<PathBuf>,
    env: &RunEnv,
) -> Result<()> {
    for task in tasks {
        if !env.quiet {
            println!("    {} {}", "•".dimmed(), task.name.dimmed());
        }
        if let Some(w) = &task.when
            && !eval_when(w, vars)
        {
            if !env.quiet {
                println!(
                    "      {}",
                    format!("(skipped — when: {w} was false)").dimmed()
                );
            }
            continue;
        }
        if let Err(e) = run_task(task, vars, include_stack, env) {
            if task.ignore_errors {
                if !env.quiet {
                    println!("      {} failed (ignored): {e}", "!".yellow().bold());
                }
            } else {
                return Err(e);
            }
        }
    }
    Ok(())
}

/// `block:` runs first; on failure, `rescue:` (if any) runs and — if it succeeds —
/// recovers the block. `always:` then runs unconditionally, and a failure there fails
/// the block even after a successful rescue.
fn run_block(
    block: &[Task],
    rescue: &[Task],
    always: &[Task],
    vars: &mut HashMap<String, String>,
    include_stack: &mut Vec<PathBuf>,
    env: &RunEnv,
) -> Result<()> {
    let result = match run_task_sequence(block, vars, include_stack, env) {
        Ok(()) => Ok(()),
        Err(e) if rescue.is_empty() => Err(e),
        Err(e) => {
            if !env.quiet {
                println!(
                    "    {} block failed: {e} — running rescue",
                    "!".yellow().bold()
                );
            }
            run_task_sequence(rescue, vars, include_stack, env)
        }
    };
    if !always.is_empty() {
        if !env.quiet {
            println!("    {} running always", "→".dimmed());
        }
        run_task_sequence(always, vars, include_stack, env)?;
    }
    result
}

/// Prints a colored unified line diff between `old` and `new` when `env.diff` (`--diff`)
/// is set — a no-op otherwise, and a no-op when the two are identical. Shared by
/// `fs_write:`/`write_file:`, called right before each applies its write.
fn print_diff_if_enabled(env: &RunEnv, old: &str, new: &str) {
    if !env.diff || old == new {
        return;
    }
    use similar::{ChangeTag, TextDiff};
    for change in TextDiff::from_lines(old, new).iter_all_changes() {
        let line = change.to_string_lossy();
        match change.tag() {
            ChangeTag::Delete => print!("  {}{}", "-".red().bold(), line.red()),
            ChangeTag::Insert => print!("  {}{}", "+".green().bold(), line.green()),
            ChangeTag::Equal => {}
        }
    }
}

/// The only place `fs_write:` actually overwrites a remote file — requires
/// `&Confirmed<FsWriteSpec>`, obtainable only via `Confirmed::require`, so this can never
/// run against an unconfirmed spec. Returns the rendered content written, for the byte
/// count / `register:`.
fn exec_fs_write(
    confirmed: &Confirmed<FsWriteSpec>,
    path: &str,
    vars: &HashMap<String, String>,
    env: &RunEnv,
) -> Result<String> {
    let server_name = render(&confirmed.server, vars);
    let content = render(&confirmed.content, vars);
    let server = crate::commands::ssh::resolve_server(env.ctx, &server_name)?;
    if env.diff {
        // A nonexistent remote file is a legitimate "new file" diff (all-added) —
        // tolerate the cat failing rather than treat it as an error.
        let old = crate::db::ssh_exec_capture(&server, &crate::commands::fs::cat_cmd(path))
            .unwrap_or_default();
        print_diff_if_enabled(env, &old, &content);
    }
    let (_, stderr, success) = crate::db::ssh_exec_with_stdin(
        &server,
        &crate::commands::fs::write_cmd(path),
        content.as_bytes(),
    )?;
    if !success {
        let err = stderr.trim();
        bail!("{}", if err.is_empty() { "write failed" } else { err });
    }
    Ok(content)
}

/// The only place `systemd_restart:` actually restarts a remote unit — see
/// `exec_fs_write`'s doc comment for why this takes `&Confirmed<SystemdRestartSpec>`.
fn exec_systemd_restart(
    confirmed: &Confirmed<SystemdRestartSpec>,
    unit: &str,
    vars: &HashMap<String, String>,
    env: &RunEnv,
) -> Result<()> {
    let server_name = render(&confirmed.server, vars);
    let sudo_pass = confirmed.sudo_pass.as_deref().map(|s| render(s, vars));
    let server = crate::commands::ssh::resolve_server(env.ctx, &server_name)?;
    crate::db::ssh_exec_capture(
        &server,
        &crate::commands::systemd::restart_cmd(unit, confirmed.sudo, sudo_pass.as_deref()),
    )?;
    Ok(())
}

/// The only place `ps_kill:` actually sends a signal to a remote process — see
/// `exec_fs_write`'s doc comment for why this takes `&Confirmed<PsKillSpec>`.
fn exec_ps_kill(
    confirmed: &Confirmed<PsKillSpec>,
    signal: &str,
    vars: &HashMap<String, String>,
    env: &RunEnv,
) -> Result<()> {
    let server_name = render(&confirmed.server, vars);
    let sudo_pass = confirmed.sudo_pass.as_deref().map(|s| render(s, vars));
    let server = crate::commands::ssh::resolve_server(env.ctx, &server_name)?;
    crate::db::ssh_exec_capture(
        &server,
        &crate::commands::ps::kill_cmd(confirmed.pid, signal, confirmed.sudo, sudo_pass.as_deref()),
    )?;
    Ok(())
}

/// The only place `db_exec:` actually runs its statement — see `exec_fs_write`'s doc
/// comment for why this takes `&Confirmed<DbExecSpec>`. Returns `db::run_exec`'s output
/// string, for `register:`.
fn exec_db_exec(
    confirmed: &Confirmed<DbExecSpec>,
    sql: &str,
    vars: &HashMap<String, String>,
    env: &RunEnv,
) -> Result<String> {
    let server_name = render(&confirmed.server, vars);
    let server = crate::commands::ssh::resolve_server(env.ctx, &server_name)?;
    let creds = resolve_conn_creds(
        &server,
        confirmed.env.as_deref(),
        confirmed.engine.as_deref(),
        confirmed.host.as_deref(),
        confirmed.port,
        confirmed.database.as_deref(),
        confirmed.user.as_deref(),
        confirmed.password.as_deref(),
        vars,
    )?;
    crate::db::run_exec(&server, &creds, sql)
}

fn run_task_once(
    task: &Task,
    vars: &mut HashMap<String, String>,
    include_stack: &mut Vec<PathBuf>,
    env: &RunEnv,
) -> Result<()> {
    // flush_handlers: only makes sense with access to execute_playbook's own
    // notified/handlers bookkeeping — reaching here means it was used inside
    // block:/rescue:/always:/run_task_sequence, which has no such state.
    if task.flush_handlers {
        bail!(
            "flush_handlers: is only supported as a direct playbook task, not inside \
             block:/rescue:/always: (task '{}')",
            task.name
        );
    }

    if task.register.is_some()
        && (task.check_url.is_some()
            || task.check_port.is_some()
            || task.env_check.is_some()
            || task.include.is_some()
            || task.assert.is_some()
            || task.block.is_some()
            || task.debug.is_some()
            || task.set_fact.is_some()
            || task.wait_for.is_some()
            || task.confirm.is_some()
            || task.include_vars.is_some())
    {
        bail!(
            "register: is not supported for check_url/check_port/env_check/include/assert/block/debug/set_fact/wait_for/confirm/include_vars tasks"
        );
    }

    if task.timeout.is_some() && task.run.is_none() {
        bail!("timeout: is only supported on run: tasks");
    }

    if let Some(spec) = &task.wait_for {
        let set_count = [
            spec.check_url.is_some(),
            spec.check_port.is_some(),
            spec.ssh.is_some(),
            spec.file_exists.is_some(),
            spec.file_absent.is_some(),
        ]
        .into_iter()
        .filter(|b| *b)
        .count();
        if set_count != 1 {
            bail!(
                "wait_for: needs exactly one of check_url/check_port/ssh/file_exists/file_absent, \
                 task '{}' has {set_count}",
                task.name
            );
        }
    }

    if let Some(msg) = &task.debug {
        println!("  {} {}", "ℹ".cyan().bold(), render(msg, vars));
        return Ok(());
    }

    if let Some(msg) = &task.confirm {
        let rendered = render(msg, vars);
        if env.dry {
            if !env.quiet {
                println!(
                    "  {} (dry run — would prompt: {rendered})",
                    "?".cyan().bold()
                );
            }
            return Ok(());
        }
        if env.auto_yes {
            if !env.quiet {
                println!("  {} {rendered} — confirmed via --yes", "?".cyan().bold());
            }
            return Ok(());
        }
        if env.quiet {
            bail!(
                "confirm: '{rendered}' requires --yes when running non-interactively \
                 (--output json, or driven by an agent over MCP) — it never blocks on stdin"
            );
        }
        print!("  {} {rendered} [y/N] ", "?".cyan().bold());
        std::io::Write::flush(&mut std::io::stdout()).ok();
        let mut input = String::new();
        std::io::stdin()
            .read_line(&mut input)
            .context("failed to read confirm: answer from stdin")?;
        let answer = input.trim().to_lowercase();
        if answer == "y" || answer == "yes" {
            return Ok(());
        }
        bail!("aborted at confirm: '{rendered}'");
    }

    if let Some(facts) = &task.set_fact {
        for (k, v) in facts {
            let rendered = render(v, vars);
            if !env.quiet {
                println!("  {} {} = {}", "ƒ".cyan().bold(), k, rendered.dimmed());
            }
            vars.insert(k.clone(), rendered);
        }
        return Ok(());
    }

    if let Some(path) = &task.include_vars {
        let rendered = render(path, vars);
        let resolved = env.playbook_dir.join(&rendered);
        if !env.quiet {
            println!(
                "  {} {}",
                "→ vars".bold(),
                resolved.display().to_string().dimmed()
            );
        }
        if !env.dry {
            let loaded = load_vars_file(&resolved)?;
            let count = loaded.len();
            vars.extend(loaded);
            if !env.quiet {
                println!("  {} {count} var(s) loaded", "✓ ok".green().bold());
            }
        }
        return Ok(());
    }

    if let Some(facts) = &task.state_set {
        for (k, v) in facts {
            let rendered = render(v, vars);
            if !env.quiet {
                println!(
                    "  {} state.{} = {}",
                    "ƒ".cyan().bold(),
                    k,
                    rendered.dimmed()
                );
            }
            vars.insert(format!("state.{k}"), rendered);
        }
        // The actual persistence, like every other action's real work, is skipped in
        // --dry — the in-memory vars.insert() above is enough to preview what the
        // resulting {{state.*}} values would be within this run.
        if !env.dry {
            write_persisted_state(env, vars);
        }
        return Ok(());
    }

    if let Some(expr) = &task.assert {
        if !eval_when(expr, vars) {
            bail!("assertion failed: {expr}");
        }
        return Ok(());
    }

    if let Some(block_tasks) = &task.block {
        return run_block(
            block_tasks,
            task.rescue.as_deref().unwrap_or(&[]),
            task.always.as_deref().unwrap_or(&[]),
            vars,
            include_stack,
            env,
        );
    }

    if let Some(cmd) = &task.run {
        let rendered_cmd = render(cmd, vars);
        if !env.quiet {
            println!(
                "  {} {}",
                "$".bold().green(),
                render_for_display(cmd, vars).dimmed()
            );
        }
        if !env.dry {
            let start = Instant::now();
            let mut cmd = std::process::Command::new("sh");
            cmd.arg("-c")
                .arg(&rendered_cmd)
                .current_dir(&env.playbook_dir);
            let (success, code, captured) =
                run_with_timeout(cmd, task.register.is_some(), task.timeout)?;
            let elapsed = start.elapsed();
            if success {
                if !env.quiet {
                    println!(
                        "  {} {}",
                        "✓ ok".green().bold(),
                        format!("({:.1}s)", elapsed.as_secs_f32()).dimmed()
                    );
                }
                if let (Some(reg), Some(val)) = (&task.register, captured) {
                    vars.insert(reg.clone(), val);
                }
            } else {
                bail!("command exited with code {}", code.unwrap_or(1));
            }
        }
        return Ok(());
    }

    if let Some(url) = &task.check_url {
        let url = render(url, vars);
        if !env.quiet {
            println!("  {} {}", "→".bold(), url.dimmed());
        }
        if !env.dry {
            check_url(&url, env.quiet)?;
        }
        return Ok(());
    }

    if let Some(spec) = &task.check_port {
        let host = render(&spec.host, vars);
        if !env.quiet {
            println!("  {} {}:{}", "→".bold(), host.dimmed(), spec.port);
        }
        if !env.dry {
            check_port(&host, spec.port, spec.timeout, env.quiet)?;
        }
        return Ok(());
    }

    if let Some(spec) = &task.http {
        let url = render(&spec.url, vars);
        // Kept as the rendered *relative* string (not the resolved absolute path) so a
        // `register:`ed value composes directly with any other path-taking task
        // (`mail: {attachments: [...]}`, `read_csv:`, ...) — all of which resolve their
        // own paths relative to this same playbook directory via `join_confined`, which
        // rejects absolute paths outright.
        let download_rel = spec.download.as_ref().map(|d| render(d, vars));
        let download_path = download_rel
            .as_ref()
            .map(|d| join_confined(&env.playbook_dir, d))
            .transpose()?;
        if !env.quiet {
            match &download_path {
                Some(p) => println!(
                    "  {} {} {} → {}",
                    spec.method.to_uppercase().bold(),
                    "→".bold(),
                    url.dimmed(),
                    p.display().to_string().dimmed()
                ),
                None => println!(
                    "  {} {} {}",
                    spec.method.to_uppercase().bold(),
                    "→".bold(),
                    url.dimmed()
                ),
            }
        }
        if !env.dry {
            if let Some(out_path) = &download_path {
                let (_bytes_written, status) = http_download(spec, &url, vars, out_path)?;
                if let Some(reg) = &task.register {
                    vars.insert(format!("{reg}.status"), status.to_string());
                    vars.insert(reg.clone(), download_rel.clone().unwrap_or_default());
                }
            } else {
                let (body, status) = http_request(spec, &url, vars)?;
                if let Some(reg) = &task.register {
                    vars.insert(format!("{reg}.status"), status.to_string());
                    vars.insert(reg.clone(), body);
                }
            }
        }
        return Ok(());
    }

    if let Some(spec) = &task.scrape {
        let url = render(&spec.url, vars);
        if !env.quiet {
            println!("  {} {}", "→".bold(), url.dimmed());
        }
        if !env.dry {
            let items = scrape(spec, &url, vars)?;
            if !env.quiet {
                println!(
                    "  {} scraped {} item(s)",
                    "✓ ok".green().bold(),
                    items.len()
                );
            }
            if let Some(reg) = &task.register {
                vars.insert(
                    reg.clone(),
                    serde_json::to_string(&items).unwrap_or_default(),
                );
            }
        }
        return Ok(());
    }

    if let Some(spec) = &task.wait_for {
        // Exactly one of these is Some — enforced upfront above.
        let file_exists_path = spec
            .file_exists
            .as_deref()
            .map(|p| join_confined(&env.playbook_dir, &render(p, vars)))
            .transpose()?;
        let file_absent_path = spec
            .file_absent
            .as_deref()
            .map(|p| join_confined(&env.playbook_dir, &render(p, vars)))
            .transpose()?;
        let describe = if let Some(url) = &spec.check_url {
            format!("{} to respond", render(url, vars))
        } else if let Some(port_spec) = &spec.check_port {
            format!(
                "{}:{} to accept connections",
                render(&port_spec.host, vars),
                port_spec.port
            )
        } else if let Some(path) = &file_exists_path {
            format!("{} to exist", path.display())
        } else if let Some(path) = &file_absent_path {
            format!("{} to no longer exist", path.display())
        } else {
            let ssh_spec = spec.ssh.as_ref().expect("validated: exactly one check set");
            format!(
                "'{}' to succeed on {}",
                render(&ssh_spec.command, vars),
                render(&ssh_spec.server, vars)
            )
        };
        if !env.quiet {
            println!(
                "  {} waiting for {describe} (up to {}s)...",
                "→".bold(),
                spec.timeout
            );
        }
        if !env.dry {
            let deadline = Instant::now() + Duration::from_secs(spec.timeout);
            loop {
                let attempt_ok = if let Some(url) = &spec.check_url {
                    check_url(&render(url, vars), true).is_ok()
                } else if let Some(port_spec) = &spec.check_port {
                    check_port(
                        &render(&port_spec.host, vars),
                        port_spec.port,
                        port_spec.timeout,
                        true,
                    )
                    .is_ok()
                } else if let Some(path) = &file_exists_path {
                    path.exists()
                } else if let Some(path) = &file_absent_path {
                    !path.exists()
                } else {
                    let ssh_spec = spec.ssh.as_ref().expect("validated: exactly one check set");
                    let server_name = render(&ssh_spec.server, vars);
                    let full_cmd = crate::commands::fleet::exec_command(
                        &render(&ssh_spec.command, vars),
                        ssh_spec.sudo,
                    );
                    crate::commands::ssh::resolve_server(env.ctx, &server_name)
                        .ok()
                        .and_then(|server| {
                            crate::db::ssh_exec_capture_lenient(&server, &full_cmd).ok()
                        })
                        .map(|(_, _, success)| success)
                        .unwrap_or(false)
                };
                if attempt_ok {
                    if !env.quiet {
                        println!("  {} {describe}", "✓ ok".green().bold());
                    }
                    break;
                }
                if Instant::now() >= deadline {
                    bail!("timed out after {}s waiting for {describe}", spec.timeout);
                }
                std::thread::sleep(Duration::from_secs(spec.interval));
            }
        }
        return Ok(());
    }

    if let Some(spec) = &task.report {
        let format = spec.format.to_lowercase();
        if !matches!(format.as_str(), "html" | "pdf" | "excel") {
            bail!(
                "report: format must be one of html/pdf/excel, task '{}' has '{}'",
                task.name,
                spec.format
            );
        }
        let title = render(&spec.title, vars);
        let out_path = env.playbook_dir.join(render(&spec.out, vars));
        if !env.quiet {
            println!(
                "  {} report format={format} out={}",
                "→".bold(),
                out_path.display()
            );
        }
        if !env.dry {
            let sources: Vec<report::Source> = spec
                .sources
                .iter()
                .map(|(name, raw)| {
                    let rendered = render(raw, vars);
                    let value = serde_json::from_str(&rendered)
                        .unwrap_or(serde_json::Value::String(rendered));
                    report::Source {
                        name: name.clone(),
                        value,
                    }
                })
                .collect();
            let bytes = match format.as_str() {
                "html" => report::html::build(&title, &sources),
                "pdf" => report::pdf::build(&title, &sources),
                "excel" => report::excel::build(&title, &sources),
                _ => unreachable!("validated above"),
            }
            .with_context(|| format!("building {format} report"))?;
            std::fs::write(&out_path, &bytes)
                .with_context(|| format!("writing report: {}", out_path.display()))?;
            if !env.quiet {
                println!(
                    "  {} {} bytes -> {}",
                    "✓ ok".green().bold(),
                    bytes.len(),
                    out_path.display()
                );
            }
            if let Some(reg) = &task.register {
                vars.insert(reg.clone(), bytes.len().to_string());
            }
        }
        return Ok(());
    }

    if let Some(spec) = &task.write_file {
        let out_path = join_confined(&env.playbook_dir, &render(&spec.path, vars))?;
        if !env.quiet {
            println!(
                "  {} {}",
                if spec.append {
                    "→ append".bold()
                } else {
                    "→ write".bold()
                },
                out_path.display().to_string().dimmed()
            );
        }
        if !env.dry {
            let content = render(&spec.content, vars);
            if env.diff {
                let old = std::fs::read(&out_path)
                    .map(|b| String::from_utf8_lossy(&b).into_owned())
                    .unwrap_or_default();
                // Appending only changes the tail — diff the resulting full content, not
                // just the fragment, so it reads as "old" -> "old + new tail".
                let new = if spec.append {
                    format!("{old}{content}")
                } else {
                    content.clone()
                };
                print_diff_if_enabled(env, &old, &new);
            }
            if let Some(parent) = out_path.parent() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!("creating parent directory for {}", out_path.display())
                })?;
            }
            let bytes_written = content.len();
            if spec.append {
                use std::io::Write;
                let mut f = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&out_path)
                    .with_context(|| format!("opening {} for append", out_path.display()))?;
                f.write_all(content.as_bytes())
                    .with_context(|| format!("appending to {}", out_path.display()))?;
            } else {
                std::fs::write(&out_path, &content)
                    .with_context(|| format!("writing {}", out_path.display()))?;
            }
            if !env.quiet {
                println!(
                    "  {} {} bytes -> {}",
                    "✓ ok".green().bold(),
                    bytes_written,
                    out_path.display()
                );
            }
            if let Some(reg) = &task.register {
                vars.insert(reg.clone(), bytes_written.to_string());
            }
        }
        return Ok(());
    }

    if let Some(spec) = &task.read_csv {
        let in_path = join_confined(&env.playbook_dir, &render(&spec.path, vars))?;
        if !env.quiet {
            println!(
                "  {} {}",
                "→ read".bold(),
                in_path.display().to_string().dimmed()
            );
        }
        if !env.dry {
            let delimiter = match &spec.delimiter {
                Some(d) if d.len() == 1 => d.as_bytes()[0],
                Some(d) => bail!("read_csv: delimiter must be a single character, got '{d}'"),
                None => b',',
            };
            let mut reader = csv::ReaderBuilder::new()
                .has_headers(spec.headers)
                .delimiter(delimiter)
                .from_path(&in_path)
                .with_context(|| format!("opening {}", in_path.display()))?;

            let rows: Vec<serde_json::Value> = if spec.headers {
                let headers = reader
                    .headers()
                    .with_context(|| format!("reading header row of {}", in_path.display()))?
                    .clone();
                reader
                    .records()
                    .map(|r| {
                        let record =
                            r.with_context(|| format!("reading a row of {}", in_path.display()))?;
                        let mut obj = serde_json::Map::new();
                        for (col, cell) in headers.iter().zip(record.iter()) {
                            obj.insert(
                                col.to_string(),
                                serde_json::Value::String(cell.to_string()),
                            );
                        }
                        Ok(serde_json::Value::Object(obj))
                    })
                    .collect::<Result<Vec<_>>>()?
            } else {
                reader
                    .records()
                    .map(|r| {
                        let record =
                            r.with_context(|| format!("reading a row of {}", in_path.display()))?;
                        Ok(serde_json::Value::Array(
                            record
                                .iter()
                                .map(|c| serde_json::Value::String(c.to_string()))
                                .collect(),
                        ))
                    })
                    .collect::<Result<Vec<_>>>()?
            };

            if !env.quiet {
                println!("  {} {} row(s)", "✓ ok".green().bold(), rows.len());
            }
            if let Some(reg) = &task.register {
                vars.insert(
                    reg.clone(),
                    serde_json::to_string(&rows).unwrap_or_default(),
                );
            }
        }
        return Ok(());
    }

    if let Some(spec) = &task.write_csv {
        let out_path = join_confined(&env.playbook_dir, &render(&spec.path, vars))?;
        if !env.quiet {
            println!(
                "  {} {}",
                "→ write".bold(),
                out_path.display().to_string().dimmed()
            );
        }
        if !env.dry {
            let delimiter = match &spec.delimiter {
                Some(d) if d.len() == 1 => d.as_bytes()[0],
                Some(d) => bail!("write_csv: delimiter must be a single character, got '{d}'"),
                None => b',',
            };
            let rendered = render(&spec.data, vars);
            let value: serde_json::Value = serde_json::from_str(&rendered)
                .with_context(|| format!("write_csv: '{}' data is not valid JSON", task.name))?;
            let serde_json::Value::Array(elements) = value else {
                bail!(
                    "write_csv: task '{}' data must be a JSON array, got {}",
                    task.name,
                    rendered.chars().take(60).collect::<String>()
                );
            };

            let mut writer = csv::WriterBuilder::new()
                .delimiter(delimiter)
                .from_writer(Vec::new());
            let row_count = elements.len();
            if let Some(serde_json::Value::Object(first)) = elements.first() {
                let cols: Vec<String> = first.keys().cloned().collect();
                if spec.headers {
                    writer
                        .write_record(&cols)
                        .context("writing CSV header row")?;
                }
                for el in &elements {
                    let obj = el.as_object();
                    let record: Vec<String> = cols
                        .iter()
                        .map(|c| {
                            obj.and_then(|o| o.get(c))
                                .map(json_cell_to_string)
                                .unwrap_or_default()
                        })
                        .collect();
                    writer.write_record(&record).context("writing CSV row")?;
                }
            } else {
                for el in &elements {
                    let record: Vec<String> = match el {
                        serde_json::Value::Array(items) => {
                            items.iter().map(json_cell_to_string).collect()
                        }
                        other => vec![json_cell_to_string(other)],
                    };
                    writer.write_record(&record).context("writing CSV row")?;
                }
            }
            let bytes = writer.into_inner().context("finalizing CSV output")?;
            if let Some(parent) = out_path.parent() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!("creating parent directory for {}", out_path.display())
                })?;
            }
            std::fs::write(&out_path, &bytes)
                .with_context(|| format!("writing {}", out_path.display()))?;

            if !env.quiet {
                println!("  {} {} row(s)", "✓ ok".green().bold(), row_count);
            }
            if let Some(reg) = &task.register {
                vars.insert(reg.clone(), row_count.to_string());
            }
        }
        return Ok(());
    }

    if task.git_summary.is_some() {
        if !env.quiet {
            println!("  {} git summary", "→".bold());
        }
        if !env.dry {
            let s = crate::commands::git::compute_summary(Some(&env.playbook_dir))?;
            if !env.quiet {
                let tag = s.tag.as_deref().unwrap_or("—");
                let ab = if s.has_upstream {
                    format!(" (↑{} ↓{})", s.ahead, s.behind)
                } else {
                    String::new()
                };
                println!(
                    "  branch: {}{ab}  tag: {}  status: {}",
                    s.branch,
                    tag,
                    if s.clean { "clean" } else { "dirty" }
                );
                for line in &s.recent {
                    println!("    {}", line.dimmed());
                }
            }
            if let Some(reg) = &task.register {
                vars.insert(reg.clone(), serde_json::to_string(&s).unwrap_or_default());
            }
        }
        return Ok(());
    }

    if let Some(spec) = &task.git_changelog {
        let from = spec.from.as_ref().map(|f| render(f, vars));
        if !env.quiet {
            println!("  {} git changelog", "→".bold());
        }
        if !env.dry {
            let c =
                crate::commands::git::compute_changelog(Some(&env.playbook_dir), from.as_deref())?;
            if !env.quiet {
                if c.features.is_empty() && c.fixes.is_empty() && c.other.is_empty() {
                    println!("  {}", "(no commits since last tag)".dimmed());
                } else {
                    for m in &c.features {
                        println!("  feat: {m}");
                    }
                    for m in &c.fixes {
                        println!("  fix: {m}");
                    }
                    for m in &c.other {
                        println!("  {m}");
                    }
                }
            }
            if let Some(reg) = &task.register {
                vars.insert(reg.clone(), serde_json::to_string(&c).unwrap_or_default());
            }
        }
        return Ok(());
    }

    if let Some(spec) = &task.gh_prs {
        let repo = spec.repo.as_ref().map(|r| render(r, vars));
        let after_s = spec.after.as_ref().map(|a| render(a, vars));
        let before_s = spec.before.as_ref().map(|b| render(b, vars));
        let state = render(&spec.state, vars);
        if !env.quiet {
            println!(
                "  {} gh pr list{}",
                "→".bold(),
                repo.as_deref()
                    .map(|r| format!(" in {r}"))
                    .unwrap_or_default()
                    .dimmed()
            );
        }
        if !env.dry {
            let after = after_s
                .as_deref()
                .map(crate::commands::gh::parse_date)
                .transpose()?;
            let before = before_s
                .as_deref()
                .map(crate::commands::gh::parse_date)
                .transpose()?;
            let prs = crate::commands::gh::fetch_prs(
                Some(&env.playbook_dir),
                repo.as_deref(),
                after,
                before,
                &state,
                spec.limit,
            )?;
            if !env.quiet {
                println!("  {} {} pr(s)", "✓ ok".green().bold(), prs.len());
            }
            if let Some(reg) = &task.register {
                vars.insert(reg.clone(), serde_json::to_string(&prs).unwrap_or_default());
            }
        }
        return Ok(());
    }

    if let Some(spec) = &task.env_check {
        let reference = env.playbook_dir.join(render(&spec.reference, vars));
        let target = env.playbook_dir.join(render(&spec.target, vars));
        if !env.quiet {
            println!(
                "  {} {} → {}",
                "→".bold(),
                reference.display().to_string().dimmed(),
                target.display().to_string().dimmed()
            );
        }
        if !env.dry {
            env_check(
                &reference.to_string_lossy(),
                &target.to_string_lossy(),
                env.quiet,
            )?;
        }
        return Ok(());
    }

    if let Some(spec) = &task.ssh {
        let server_name = render(&spec.server, vars);
        let cmd = render(&spec.command, vars);
        let full_cmd = crate::commands::fleet::exec_command(&cmd, spec.sudo);
        if !env.quiet {
            let display_cmd = crate::commands::fleet::exec_command(
                &render_for_display(&spec.command, vars),
                spec.sudo,
            );
            println!(
                "  {} {} {} {}",
                "→".bold(),
                display_cmd.dimmed(),
                "on".dimmed(),
                server_name.dimmed()
            );
        }
        if !env.dry {
            let server = crate::commands::ssh::resolve_server(env.ctx, &server_name)?;
            let (stdout, stderr, success) =
                crate::db::ssh_exec_capture_lenient(&server, &full_cmd)?;
            if success {
                let out = stdout.trim();
                if !env.quiet && !out.is_empty() {
                    println!("{out}");
                }
                if let Some(reg) = &task.register {
                    vars.insert(reg.clone(), out.to_string());
                }
            } else {
                let err = stderr.trim();
                bail!(
                    "{}",
                    if err.is_empty() {
                        "command failed"
                    } else {
                        err
                    }
                );
            }
        }
        return Ok(());
    }

    if let Some(spec) = &task.fleet {
        let command = render(&spec.command, vars);
        let servers = spec.servers.as_deref().map(|s| render(s, vars));
        let group = spec.group.as_deref().map(|s| render(s, vars));
        if !env.quiet {
            println!(
                "  {} {}",
                "→".bold(),
                render_for_display(&spec.command, vars).dimmed()
            );
        }
        if !env.dry {
            let results = crate::commands::fleet::run_on_targets(
                env.ctx,
                servers.as_deref(),
                spec.all,
                group.as_deref(),
                &command,
                spec.sudo,
                spec.parallel,
                spec.batch_size,
            )?;
            let ok_count = results.iter().filter(|r| r.success).count();
            let total = results.len();
            let all_succeeded = ok_count == total;
            if !env.quiet {
                for r in &results {
                    if r.success {
                        println!("    {} {}", "✓".green().bold(), r.server.cyan());
                    } else {
                        println!("    {} {}", "✗".red().bold(), r.server.cyan());
                    }
                }
            }
            if let Some(reg) = &task.register {
                vars.insert(reg.clone(), format!("{ok_count}/{total}"));
            }
            if !all_succeeded {
                bail!("{ok_count}/{total} servers succeeded");
            }
        }
        return Ok(());
    }

    if let Some(spec) = &task.fs_cat {
        let server_name = render(&spec.server, vars);
        let path = render(&spec.path, vars);
        if !env.quiet {
            println!(
                "  {} {} on {}",
                "→ cat".bold(),
                path.dimmed(),
                server_name.dimmed()
            );
        }
        if !env.dry {
            let server = crate::commands::ssh::resolve_server(env.ctx, &server_name)?;
            let content =
                crate::db::ssh_exec_capture(&server, &crate::commands::fs::cat_cmd(&path))?;
            if !env.quiet {
                println!("  {} {} byte(s)", "✓ ok".green().bold(), content.len());
            }
            if let Some(reg) = &task.register {
                vars.insert(reg.clone(), content);
            }
        }
        return Ok(());
    }

    if let Some(spec) = &task.fs_write {
        let server_name = render(&spec.server, vars);
        let path = render(&spec.path, vars);
        if !env.quiet {
            println!(
                "  {} {} on {}",
                "→ write".bold(),
                path.dimmed(),
                server_name.dimmed()
            );
        }
        if !env.dry {
            let confirmed = Confirmed::require(spec, "fs_write", &task.name)?;
            let content = exec_fs_write(&confirmed, &path, vars, env)?;
            if !env.quiet {
                println!("  {} {} byte(s)", "✓ ok".green().bold(), content.len());
            }
            if let Some(reg) = &task.register {
                vars.insert(reg.clone(), content.len().to_string());
            }
        }
        return Ok(());
    }

    if let Some(spec) = &task.systemd_restart {
        let server_name = render(&spec.server, vars);
        let unit = render(&spec.unit, vars);
        if !env.quiet {
            println!(
                "  {} restart {} on {}",
                "→".bold(),
                unit.dimmed(),
                server_name.dimmed()
            );
        }
        if !env.dry {
            let confirmed = Confirmed::require(spec, "systemd_restart", &task.name)?;
            exec_systemd_restart(&confirmed, &unit, vars, env)?;
            if !env.quiet {
                println!("  {} {} restarted", "✓ ok".green().bold(), unit);
            }
            if let Some(reg) = &task.register {
                vars.insert(reg.clone(), "true".to_string());
            }
        }
        return Ok(());
    }

    if let Some(spec) = &task.systemd_status {
        let server_name = render(&spec.server, vars);
        let unit = render(&spec.unit, vars);
        if !env.quiet {
            println!(
                "  {} status {} on {}",
                "→".bold(),
                unit.dimmed(),
                server_name.dimmed()
            );
        }
        if !env.dry {
            let server = crate::commands::ssh::resolve_server(env.ctx, &server_name)?;
            let (stdout, stderr, active) = crate::db::ssh_exec_capture_lenient(
                &server,
                &crate::commands::systemd::status_cmd(&unit),
            )?;
            let output = crate::commands::systemd::merge_output(stdout, stderr);
            if !env.quiet {
                let marker = if active {
                    "●".green().bold()
                } else {
                    "●".red().bold()
                };
                println!("{marker} {unit}");
                print!("{output}");
            }
            if let Some(reg) = &task.register {
                vars.insert(format!("{reg}.active"), active.to_string());
                vars.insert(reg.clone(), output);
            }
        }
        return Ok(());
    }

    if let Some(spec) = &task.logs_tail {
        let server_name = render(&spec.server, vars);
        let path = render(&spec.path, vars);
        if !env.quiet {
            println!(
                "  {} {} on {}",
                "→ tail".bold(),
                path.dimmed(),
                server_name.dimmed()
            );
        }
        if !env.dry {
            let server = crate::commands::ssh::resolve_server(env.ctx, &server_name)?;
            let output = crate::db::ssh_exec_capture(
                &server,
                &crate::commands::logs::tail_cmd(&path, spec.lines),
            )?;
            let lines: Vec<&str> = output.lines().collect();
            if !env.quiet {
                println!("  {} {} line(s)", "✓ ok".green().bold(), lines.len());
            }
            if let Some(reg) = &task.register {
                vars.insert(
                    reg.clone(),
                    serde_json::to_string(&lines).unwrap_or_default(),
                );
            }
        }
        return Ok(());
    }

    if let Some(spec) = &task.logs_grep {
        let server_name = render(&spec.server, vars);
        let path = render(&spec.path, vars);
        let pattern = render(&spec.pattern, vars);
        if !env.quiet {
            println!(
                "  {} '{}' in {} on {}",
                "→ grep".bold(),
                pattern.dimmed(),
                path.dimmed(),
                server_name.dimmed()
            );
        }
        if !env.dry {
            let server = crate::commands::ssh::resolve_server(env.ctx, &server_name)?;
            let (stdout, stderr, success) = crate::db::ssh_exec_capture_lenient(
                &server,
                &crate::commands::logs::grep_cmd(&path, &pattern),
            )?;
            let output = if success {
                stdout
            } else if stderr.trim().is_empty() {
                String::new()
            } else {
                bail!("{}", stderr.trim());
            };
            let (lines, truncated) = crate::commands::logs::cap_lines(output, spec.max_lines);
            if !env.quiet {
                let suffix = if truncated { ", truncated" } else { "" };
                println!(
                    "  {} {} match(es){suffix}",
                    "✓ ok".green().bold(),
                    lines.len()
                );
            }
            if let Some(reg) = &task.register {
                vars.insert(
                    reg.clone(),
                    serde_json::to_string(&lines).unwrap_or_default(),
                );
            }
        }
        return Ok(());
    }

    if let Some(spec) = &task.ps_list {
        let server_name = render(&spec.server, vars);
        let filter = spec.filter.as_deref().map(|f| render(f, vars));
        if !env.quiet {
            println!("  {} processes on {}", "→".bold(), server_name.dimmed());
        }
        if !env.dry {
            let server = crate::commands::ssh::resolve_server(env.ctx, &server_name)?;
            let output = crate::db::ssh_exec_capture(&server, "ps aux")?;
            let rows = crate::commands::ps::apply_filter(
                crate::commands::ps::parse_ps_aux(&output),
                filter.as_deref(),
            );
            if !env.quiet {
                println!("  {} {} process(es)", "✓ ok".green().bold(), rows.len());
            }
            if let Some(reg) = &task.register {
                vars.insert(
                    reg.clone(),
                    serde_json::to_string(&rows).unwrap_or_default(),
                );
            }
        }
        return Ok(());
    }

    if let Some(spec) = &task.ps_kill {
        let server_name = render(&spec.server, vars);
        let signal = render(&spec.signal, vars);
        if !env.quiet {
            println!(
                "  {} SIG{} to pid {} on {}",
                "→".bold(),
                signal.dimmed(),
                spec.pid,
                server_name.dimmed()
            );
        }
        if !env.dry {
            let confirmed = Confirmed::require(spec, "ps_kill", &task.name)?;
            exec_ps_kill(&confirmed, &signal, vars, env)?;
            if !env.quiet {
                println!("  {} sent", "✓ ok".green().bold());
            }
            if let Some(reg) = &task.register {
                vars.insert(reg.clone(), "true".to_string());
            }
        }
        return Ok(());
    }

    if let Some(spec) = &task.stat {
        let server_name = render(&spec.server, vars);
        if !env.quiet {
            println!("  {} stat on {}", "→".bold(), server_name.dimmed());
        }
        if !env.dry {
            let server = crate::commands::ssh::resolve_server(env.ctx, &server_name)?;
            let output = crate::db::ssh_exec_capture(&server, &crate::commands::stat::stat_cmd())?;
            let (uptime, memory, disk) = crate::commands::stat::parse_sections(&output);
            if !env.quiet {
                println!("{uptime}\n{memory}\n{disk}");
            }
            if let Some(reg) = &task.register {
                vars.insert(
                    reg.clone(),
                    serde_json::json!({"uptime": uptime, "memory": memory, "disk": disk})
                        .to_string(),
                );
            }
        }
        return Ok(());
    }

    if let Some(spec) = &task.sync_db {
        let server_name = render(&spec.server, vars);
        let describe_side = |side: &DbSyncSide| -> String {
            side.database
                .as_deref()
                .or(side.env.as_deref())
                .map(|s| render(s, vars))
                .unwrap_or_default()
        };
        let from_creds_preview = describe_side(&spec.from);
        let to_creds_preview = describe_side(&spec.to);
        if !env.quiet {
            println!(
                "  {} db sync {} → {} on {}",
                "→".bold(),
                from_creds_preview.dimmed(),
                to_creds_preview.dimmed(),
                server_name.dimmed()
            );
        }
        if !env.dry {
            let server = crate::commands::ssh::resolve_server(env.ctx, &server_name)?;
            let from_creds = resolve_db_sync_creds(&server, &spec.from, vars)?;
            let to_creds = resolve_db_sync_creds(&server, &spec.to, vars)?;
            let dump_cmd = crate::db::dump_command(&from_creds, true);
            let bytes = crate::db::ssh_exec_capture_bytes(&server, &dump_cmd)?;
            let restore_cmd = crate::db::restore_command(&to_creds, true);
            let (_, stderr, success) =
                crate::db::ssh_exec_with_stdin(&server, &restore_cmd, &bytes)?;
            if !success {
                bail!("db sync failed: {}", stderr.trim());
            }
            if !env.quiet {
                println!(
                    "  {} synced {} bytes into {}",
                    "✓ ok".green().bold(),
                    bytes.len(),
                    to_creds.database.cyan()
                );
            }
            if let Some(reg) = &task.register {
                vars.insert(reg.clone(), bytes.len().to_string());
            }
        }
        return Ok(());
    }

    if let Some(spec) = &task.sync_files {
        let server_name = render(&spec.server, vars);
        let from = ensure_trailing_slash(&render(&spec.from, vars));
        let to = render(&spec.to, vars);
        if !env.quiet {
            println!(
                "  {} rsync {} → {} on {}",
                "→".bold(),
                ensure_trailing_slash(&render_for_display(&spec.from, vars)).dimmed(),
                render_for_display(&spec.to, vars).dimmed(),
                server_name.dimmed()
            );
        }
        if !env.dry {
            let server = crate::commands::ssh::resolve_server(env.ctx, &server_name)?;
            let cmd = format!(
                "rsync -a{} {} {}",
                if spec.delete { " --delete" } else { "" },
                crate::db::shell_quote(&from),
                crate::db::shell_quote(&to),
            );
            let (stdout, stderr, success) = crate::db::ssh_exec_capture_lenient(&server, &cmd)?;
            if success {
                let out = stdout.trim();
                if !env.quiet {
                    if !out.is_empty() {
                        println!("{out}");
                    }
                    println!("  {} synced", "✓ ok".green().bold());
                }
                if let Some(reg) = &task.register {
                    vars.insert(reg.clone(), out.to_string());
                }
            } else {
                let err = stderr.trim();
                bail!("{}", if err.is_empty() { "rsync failed" } else { err });
            }
        }
        return Ok(());
    }

    if let Some(spec) = &task.db_query {
        let server_name = render(&spec.server, vars);
        let sql = render(&spec.sql, vars);
        if !env.quiet {
            println!(
                "  {} {} on {}",
                "→".bold(),
                render_for_display(&spec.sql, vars).dimmed(),
                server_name.dimmed()
            );
        }
        if !env.dry {
            let server = crate::commands::ssh::resolve_server(env.ctx, &server_name)?;
            let creds = resolve_conn_creds(
                &server,
                spec.env.as_deref(),
                spec.engine.as_deref(),
                spec.host.as_deref(),
                spec.port,
                spec.database.as_deref(),
                spec.user.as_deref(),
                spec.password.as_deref(),
                vars,
            )?;
            let (rows, truncated) = crate::db::run_query(&server, &creds, &sql, spec.max_rows)?;
            if !env.quiet {
                println!(
                    "  {} {} row(s){}",
                    "✓ ok".green().bold(),
                    rows.len(),
                    if truncated { " (truncated)" } else { "" }
                );
            }
            if let Some(reg) = &task.register {
                vars.insert(
                    reg.clone(),
                    serde_json::to_string(&rows).unwrap_or_default(),
                );
            }
        }
        return Ok(());
    }

    if let Some(spec) = &task.db_exec {
        let server_name = render(&spec.server, vars);
        let sql = render(&spec.sql, vars);
        if !env.quiet {
            println!(
                "  {} {} on {}",
                "→".bold(),
                render_for_display(&spec.sql, vars).dimmed(),
                server_name.dimmed()
            );
        }
        if !env.dry {
            let confirmed = Confirmed::require(spec, "db_exec", &task.name)?;
            let output = exec_db_exec(&confirmed, &sql, vars, env)?;
            if !env.quiet {
                println!("  {} {}", "✓ ok".green().bold(), output.dimmed());
            }
            if let Some(reg) = &task.register {
                vars.insert(reg.clone(), output);
            }
        }
        return Ok(());
    }

    if let Some(spec) = &task.mail {
        let to = render(&spec.to, vars);
        let subject = render(&spec.subject, vars);
        if !env.quiet {
            println!(
                "  {} mail to {} — {}",
                "→".bold(),
                render_for_display(&spec.to, vars).dimmed(),
                render_for_display(&spec.subject, vars).dimmed()
            );
        }
        if !env.dry {
            let cc = spec.cc.as_deref().map(|s| render(s, vars));
            let bcc = spec.bcc.as_deref().map(|s| render(s, vars));
            let body = render(&spec.body, vars);
            let from = spec.from.as_deref().map(|s| render(s, vars));
            let host = spec.host.as_deref().map(|s| render(s, vars));
            let user = spec.user.as_deref().map(|s| render(s, vars));
            let password = spec.password.as_deref().map(|s| render(s, vars));
            let server = spec.server.as_deref().map(|s| render(s, vars));
            let creds = resolve_mail_creds(
                env.ctx,
                server.as_deref(),
                host.as_deref(),
                spec.port,
                user.as_deref(),
                password.as_deref(),
                from.as_deref(),
                spec.tls.as_deref(),
            )?;
            let attachments = spec
                .attachments
                .iter()
                .map(|p| join_confined(&env.playbook_dir, &render(p, vars)))
                .collect::<Result<Vec<_>>>()?;
            let count = send_mail(
                &creds,
                &to,
                cc.as_deref(),
                bcc.as_deref(),
                &subject,
                &body,
                spec.html,
                &attachments,
            )?;
            if !env.quiet {
                println!("  {} sent to {} recipient(s)", "✓ ok".green().bold(), count);
            }
            if let Some(reg) = &task.register {
                vars.insert(reg.clone(), "true".to_string());
            }
        }
        return Ok(());
    }

    if let Some(spec) = &task.mail_check {
        let server_name = render(&spec.server, vars);
        let folder = render(&spec.folder, vars);
        if !env.quiet {
            println!(
                "  {} checking {} on {} ({})",
                "→".bold(),
                folder.dimmed(),
                server_name.dimmed(),
                if spec.unseen_only { "unseen" } else { "all" }
            );
        }
        if !env.dry {
            let creds = resolve_imap_creds(env.ctx, &server_name)?;
            let messages = crate::commands::mail::fetch_mail(
                &creds,
                &folder,
                spec.unseen_only,
                spec.limit,
                spec.include_body,
                spec.mark_seen,
            )?;
            if !env.quiet {
                println!("  {} {} message(s)", "✓ ok".green().bold(), messages.len());
            }
            if let Some(reg) = &task.register {
                vars.insert(
                    reg.clone(),
                    serde_json::to_string(&messages).unwrap_or_default(),
                );
            }
        }
        return Ok(());
    }

    if let Some(spec) = &task.include {
        let rendered = render(spec.file(), vars);
        let include_path = resolve_include_path(&rendered, env)?;
        if include_stack.contains(&include_path) {
            let chain = include_stack
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(" -> ");
            bail!(
                "include cycle detected: '{}' is already being included ({chain})",
                include_path.display()
            );
        }
        if !env.quiet {
            println!(
                "  {} include {}",
                "→".bold(),
                include_path.display().to_string().dimmed()
            );
        }
        if !env.dry {
            let content = std::fs::read_to_string(&include_path).with_context(|| {
                format!("Cannot read included playbook: {}", include_path.display())
            })?;
            let sub_playbook: Playbook = serde_yaml::from_str(&content)
                .with_context(|| format!("Invalid YAML in {}", include_path.display()))?;
            let sub_playbook_dir = include_path
                .parent()
                .unwrap_or(Path::new("."))
                .to_path_buf();

            // Render this include:'s vars: overrides against the *parent's* vars, before
            // any mutation below, and remember each overridden key's prior value (or that
            // it was absent) so it can be restored once the sub-playbook returns —
            // otherwise calling the same include: repeatedly with different vars: (e.g.
            // once per service in a per-service deploy) would leak the last call's values
            // into sibling tasks afterward.
            let rendered_overrides: Vec<(String, String)> = spec
                .vars()
                .iter()
                .map(|(k, v)| (k.clone(), render(v, vars)))
                .collect();
            let saved: Vec<(String, Option<String>)> = rendered_overrides
                .iter()
                .map(|(k, _)| (k.clone(), vars.get(k).cloned()))
                .collect();

            let sub_own_vars = load_playbook_vars(&sub_playbook, &sub_playbook_dir)?;
            for (k, v) in &sub_own_vars {
                vars.entry(k.clone()).or_insert_with(|| v.clone());
            }
            for (k, v) in &rendered_overrides {
                vars.insert(k.clone(), v.clone());
            }

            let sub_notes = read_notes(&include_path);
            let sub_env = RunEnv {
                playbook_dir: sub_playbook_dir,
                playbook_name: sub_playbook.name.clone(),
                audit_log: env.audit_log.clone(),
                project_root: env.project_root.clone(),
                dry: env.dry,
                diff: env.diff,
                quiet: env.quiet,
                auto_yes: env.auto_yes,
                start_at: None,
                state_path: None,
                data_path: None,
                ctx: env.ctx,
            };
            include_stack.push(include_path);
            let result = execute_playbook(
                &sub_playbook,
                &None,
                &None,
                &sub_notes,
                vars,
                include_stack,
                false,
                &sub_env,
            );
            include_stack.pop();

            // Restore each overridden key, regardless of outcome, so this include: call's
            // overrides don't leak into sibling tasks that follow it.
            for (k, prior) in saved {
                match prior {
                    Some(v) => {
                        vars.insert(k, v);
                    }
                    None => {
                        vars.remove(&k);
                    }
                }
            }

            result?;
        }
        return Ok(());
    }

    bail!(
        "task '{}' has no action (run, check_url, check_port, http, scrape, wait_for, \
         report, env_check, ssh, fleet, fs_cat, fs_write, systemd_restart, systemd_status, \
         logs_tail, logs_grep, ps_list, ps_kill, stat, include, assert, block, debug, \
         confirm, set_fact, include_vars, state_set, sync_db, sync_files, write_file, \
         read_csv, write_csv, db_query, db_exec, mail, mail_check, git_summary, \
         git_changelog, gh_prs)",
        task.name
    );
}

/// Best-effort action label for a task, used only by `--audit-log` entries (see
/// `write_audit_entry`) — not for dispatch, that's `run_task_once`'s own if-chain above.
/// Same field list its "no action" error enumerates, checked in the same order; a task
/// with no action field set never reaches here in practice (`run_task_once` rejects it
/// first), so `"unknown"` is just a safe fallback, not an expected case.
fn task_action_label(task: &Task) -> &'static str {
    // A plain bool, not an Option<T>, so it doesn't fit the check! macro below.
    if task.flush_handlers {
        return "flush_handlers";
    }
    macro_rules! check {
        ($($field:ident),+ $(,)?) => {
            $(if task.$field.is_some() { return stringify!($field); })+
        };
    }
    check!(
        run,
        check_url,
        check_port,
        http,
        scrape,
        wait_for,
        report,
        env_check,
        ssh,
        fleet,
        fs_cat,
        fs_write,
        systemd_restart,
        systemd_status,
        logs_tail,
        logs_grep,
        ps_list,
        ps_kill,
        stat,
        include,
        assert,
        block,
        debug,
        confirm,
        set_fact,
        include_vars,
        state_set,
        sync_db,
        sync_files,
        write_file,
        read_csv,
        write_csv,
        db_query,
        db_exec,
        mail,
        mail_check,
        git_summary,
        git_changelog,
        gh_prs,
    );
    "unknown"
}

/// Appends one JSON line per task attempt to `env.audit_log`, if set — see
/// `RunEnv::audit_log`/`PlayArgs::audit_log`. Called from `run_task_once_with_retries`,
/// the single funnel every concrete attempt passes through, so this covers top-level
/// tasks, `block:`/`rescue:`/`always:`, `include:`, and each `loop:` iteration
/// uniformly — but not a task skipped via `when:` (that check happens one level up,
/// before `run_task_once_with_retries` is ever called), since a skipped task never
/// touched anything. A plain blocking append is intentional, same reasoning as
/// `commands::mcp::ToolerMcp::write_audit`: one line per task attempt isn't a hot path,
/// and a write failure here must never fail the task itself — it's only reported to
/// stderr.
fn write_audit_entry(
    env: &RunEnv,
    task: &Task,
    status: &str,
    error: Option<&str>,
    duration: Duration,
) {
    let Some(path) = &env.audit_log else {
        return;
    };
    let line = serde_json::json!({
        "ts": chrono::Utc::now().to_rfc3339(),
        "playbook": env.playbook_name,
        "task": task.name,
        "action": task_action_label(task),
        "status": status,
        "duration_ms": duration.as_millis(),
        "error": error,
    });
    use std::io::Write;
    let result = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| writeln!(f, "{line}"));
    if let Err(e) = result {
        eprintln!(
            "tooler play: failed to write audit log {}: {e}",
            path.display()
        );
    }
}

/// Builds `Credentials` for one side of a `sync_db:` task, rendering each field through
/// `render()` first, then delegating to `commands::db::resolve_credentials` — the exact
/// same engine/host/port/database/user/password resolution `tooler db backup`/`restore`
/// already use, so `sync_db:` inherits the same validation and `--env`-file support.
fn resolve_db_sync_creds(
    server: &crate::config::Server,
    side: &DbSyncSide,
    vars: &HashMap<String, String>,
) -> Result<crate::db::Credentials> {
    resolve_conn_creds(
        server,
        side.env.as_deref(),
        side.engine.as_deref(),
        side.host.as_deref(),
        side.port,
        side.database.as_deref(),
        side.user.as_deref(),
        side.password.as_deref(),
        vars,
    )
}

/// Renders each (possibly-`{{var}}`-templated) connection field against `vars` and
/// delegates to `commands::db::resolve_credentials` — the exact engine/host/port/
/// database/user/password resolution `tooler db backup`/`restore`/`query` already use, so
/// both `sync_db:` (via `resolve_db_sync_creds`) and `db_query:` inherit the same
/// validation and `--env`-file support from one place.
#[allow(clippy::too_many_arguments)]
fn resolve_conn_creds(
    server: &crate::config::Server,
    env: Option<&str>,
    engine: Option<&str>,
    host: Option<&str>,
    port: Option<u16>,
    database: Option<&str>,
    user: Option<&str>,
    password: Option<&str>,
    vars: &HashMap<String, String>,
) -> Result<crate::db::Credentials> {
    let env = env.map(|s| render(s, vars));
    let engine = engine.map(|s| render(s, vars));
    let host = host.map(|s| render(s, vars));
    let database = database.map(|s| render(s, vars));
    let user = user.map(|s| render(s, vars));
    let password = password.map(|s| render(s, vars));
    crate::commands::db::resolve_credentials(
        server,
        &crate::commands::db::ConnOpts {
            env: env.as_deref(),
            engine: engine.as_deref(),
            host: host.as_deref(),
            port,
            database: database.as_deref(),
            user: user.as_deref(),
            password: password.as_deref(),
        },
    )
}

/// Resolved SMTP connection details for a `mail:` task or `tooler mail send` — always the
/// output of `resolve_mail_creds`, never built directly.
#[derive(Debug)]
pub(crate) struct MailCreds {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) user: String,
    pub(crate) password: String,
    pub(crate) from: String,
    pub(crate) tls: String,
}

/// Resolves SMTP connection details for `mail:`/`tooler mail send`: explicit fields win,
/// falling back to the named `server:` profile's config fields (`config.mail.<name>`),
/// falling back to `TOOLER_MAIL_PASSWORD` for the password specifically — mirrors
/// `db_query:`'s `TOOLER_DB_PASSWORD` pattern in `commands::db`. TLS mode: explicit `tls`
/// wins, else the profile's `tls`, else inferred from `port` (465 -> "tls", else
/// "starttls").
#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve_mail_creds(
    ctx: &Context,
    server: Option<&str>,
    host: Option<&str>,
    port: Option<u16>,
    user: Option<&str>,
    password: Option<&str>,
    from: Option<&str>,
    tls: Option<&str>,
) -> Result<MailCreds> {
    let profile = server.and_then(|name| ctx.config.mail.get(name));
    if let Some(name) = server
        && profile.is_none()
        && host.is_none()
    {
        bail!(
            "No mail profile '{name}' configured. Set it with: tooler config set mail.{name}.host <host>"
        );
    }

    let host = host
        .map(String::from)
        .or_else(|| profile.map(|p| p.host.clone()))
        .filter(|h| !h.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("mail needs 'host' or a 'server:' mail profile with a host set")
        })?;
    let user = user
        .map(String::from)
        .or_else(|| profile.map(|p| p.user.clone()))
        .filter(|u| !u.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("mail needs 'user' or a 'server:' mail profile with a user set")
        })?;
    let port = port
        .or_else(|| profile.map(|p| p.port))
        .filter(|&p| p != 0)
        .unwrap_or(587);
    let from = from
        .map(String::from)
        .or_else(|| profile.and_then(|p| p.from.clone()))
        .unwrap_or_else(|| user.clone());
    let tls = tls
        .map(String::from)
        .or_else(|| profile.and_then(|p| p.tls.clone()))
        .unwrap_or_else(|| {
            if port == 465 {
                "tls".to_string()
            } else {
                "starttls".to_string()
            }
        });

    let password = if let Some(p) = password {
        p.to_string()
    } else if let Some(name) = server {
        match crate::secrets::get_secret(&format!("mail:{name}"), "password")? {
            Some(p) => p,
            None => std::env::var("TOOLER_MAIL_PASSWORD").map_err(|_| {
                anyhow::anyhow!(
                    "No password for mail profile '{name}' (or the OS keychain is locked) — \
                     set it with: tooler config set mail.{name}.password <value>, or set \
                     TOOLER_MAIL_PASSWORD"
                )
            })?,
        }
    } else {
        std::env::var("TOOLER_MAIL_PASSWORD").map_err(|_| {
            anyhow::anyhow!(
                "mail needs 'password', a 'server:' profile's stored password, or \
                 TOOLER_MAIL_PASSWORD"
            )
        })?
    };

    Ok(MailCreds {
        host,
        port,
        user,
        password,
        from,
        tls,
    })
}

/// Resolved IMAP connection details for `mail_check:`/`tooler mail check` — always the
/// output of `resolve_imap_creds`, never built directly.
#[derive(Debug)]
pub(crate) struct ImapCreds {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) user: String,
    pub(crate) password: String,
}

/// Resolves IMAP connection details for a mail profile — profile-only (no inline
/// host/user/password override the way `resolve_mail_creds` allows for SMTP, since
/// `mail_check:`/`tooler mail check` are narrower/newer and a profile is the only
/// supported path in v1). `imap_host` defaults to the profile's SMTP `host` (the common
/// case: one mailbox, two protocols, same server); `imap_port` defaults to `993`. The
/// password is the exact same keychain entry `resolve_mail_creds` reads (`mail:<name>`) —
/// one login shared by both protocols.
/// The pure, keychain-free half of `resolve_imap_creds`'s resolution: `imap_host`
/// defaults to the profile's SMTP `host` (one mailbox, two protocols, same server —
/// exactly the case with every mail profile set up so far), `imap_port` defaults to
/// `993`. Split out so this defaulting logic is unit-testable without a real OS
/// credential store in the loop.
fn resolve_imap_host_port(profile: &crate::config::MailServer) -> (Option<String>, u16) {
    let host = profile
        .imap_host
        .clone()
        .filter(|h| !h.is_empty())
        .or_else(|| Some(profile.host.clone()).filter(|h| !h.is_empty()));
    let port = profile.imap_port.filter(|&p| p != 0).unwrap_or(993);
    (host, port)
}

pub(crate) fn resolve_imap_creds(ctx: &Context, server: &str) -> Result<ImapCreds> {
    let profile = ctx.config.mail.get(server).ok_or_else(|| {
        anyhow::anyhow!(
            "No mail profile '{server}' configured. Set it with: tooler config set mail.{server}.host <host>"
        )
    })?;
    let (host, port) = resolve_imap_host_port(profile);
    let host = host.ok_or_else(|| {
        anyhow::anyhow!(
            "mail profile '{server}' has no host set (imap_host or host) -- set it with: \
             tooler config set mail.{server}.host <host>"
        )
    })?;
    let user = if profile.user.is_empty() {
        bail!(
            "mail profile '{server}' has no user set -- set it with: tooler config set \
             mail.{server}.user <user>"
        );
    } else {
        profile.user.clone()
    };
    let password = crate::secrets::get_secret(&format!("mail:{server}"), "password")?
        .or_else(|| std::env::var("TOOLER_MAIL_PASSWORD").ok())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "No password for mail profile '{server}' (or the OS keychain is locked) — \
                 set it with: tooler config set mail.{server}.password <value>, or set \
                 TOOLER_MAIL_PASSWORD"
            )
        })?;

    Ok(ImapCreds {
        host,
        port,
        user,
        password,
    })
}

/// The one place `lettre` is touched. Builds a `Message` from `,`-separated to/cc/bcc
/// lists and sends it over SMTP per `creds.tls` — "starttls" -> `starttls_relay` (upgrade
/// an unencrypted connection, port 587 territory), "tls" -> `relay` (implicit/wrapper TLS,
/// port 465 territory), "none" -> `builder_dangerous` (no TLS at all — an escape hatch for
/// a local, unauthenticated relay only). Returns the number of recipients (to+cc+bcc).
// One parameter per distinct email field (to/cc/bcc/subject/body/html/attachments) plus
// creds -- splitting this into a builder/options struct wouldn't reduce real complexity,
// just move the same 8 fields into a different shape.
#[allow(clippy::too_many_arguments)]
pub(crate) fn send_mail(
    creds: &MailCreds,
    to: &str,
    cc: Option<&str>,
    bcc: Option<&str>,
    subject: &str,
    body: &str,
    html: bool,
    attachments: &[PathBuf],
) -> Result<usize> {
    use lettre::message::{Attachment, Mailbox, MultiPart, SinglePart};
    use lettre::transport::smtp::authentication::Credentials;
    use lettre::{Message, SmtpTransport, Transport};

    // Read every attachment up front, before opening any network connection, so a typo'd
    // path fails fast and clearly instead of after a (possibly slow) SMTP handshake.
    let attachment_bytes: Vec<(String, Vec<u8>, &'static str)> = attachments
        .iter()
        .map(|path| {
            let bytes = std::fs::read(path)
                .with_context(|| format!("reading attachment '{}'", path.display()))?;
            let filename = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "attachment".to_string());
            Ok((filename, bytes, guess_mime(path)))
        })
        .collect::<Result<Vec<_>>>()?;

    let mut builder = Message::builder()
        .from(
            creds
                .from
                .parse::<Mailbox>()
                .with_context(|| format!("invalid from address '{}'", creds.from))?,
        )
        .subject(subject);

    let mut recipient_count = 0usize;
    for addr in to.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        builder = builder.to(addr
            .parse::<Mailbox>()
            .with_context(|| format!("invalid to address '{addr}'"))?);
        recipient_count += 1;
    }
    if recipient_count == 0 {
        bail!("mail: 'to' has no addresses");
    }
    if let Some(cc) = cc {
        for addr in cc.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            builder = builder.cc(addr
                .parse::<Mailbox>()
                .with_context(|| format!("invalid cc address '{addr}'"))?);
            recipient_count += 1;
        }
    }
    if let Some(bcc) = bcc {
        for addr in bcc.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            builder = builder.bcc(
                addr.parse::<Mailbox>()
                    .with_context(|| format!("invalid bcc address '{addr}'"))?,
            );
            recipient_count += 1;
        }
    }

    let part = if html {
        SinglePart::html(body.to_string())
    } else {
        SinglePart::plain(body.to_string())
    };
    let message = if attachment_bytes.is_empty() {
        builder.singlepart(part).context("building email message")?
    } else {
        let mut multipart = MultiPart::mixed().singlepart(part);
        for (filename, bytes, mime) in attachment_bytes {
            let content_type = lettre::message::header::ContentType::parse(mime)
                .with_context(|| format!("invalid content type '{mime}' for '{filename}'"))?;
            multipart = multipart.singlepart(Attachment::new(filename).body(bytes, content_type));
        }
        builder
            .multipart(multipart)
            .context("building email message")?
    };

    let transport = match creds.tls.as_str() {
        "starttls" => SmtpTransport::starttls_relay(&creds.host)?.port(creds.port),
        "tls" => SmtpTransport::relay(&creds.host)?.port(creds.port),
        "none" => SmtpTransport::builder_dangerous(&creds.host).port(creds.port),
        other => bail!("mail: unknown tls mode '{other}' — use starttls, tls, or none"),
    }
    .credentials(Credentials::new(creds.user.clone(), creds.password.clone()))
    .build();

    transport.send(&message).context("sending email")?;
    Ok(recipient_count)
}

/// Best-effort content type from a file extension, for `mail:` attachments. Unknown or
/// missing extensions fall back to a generic binary type — attachments still work, mail
/// clients just won't show a specific icon/preview for them.
fn guess_mime(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("pdf") => "application/pdf",
        Some("csv") => "text/csv",
        Some("txt") => "text/plain",
        Some("json") => "application/json",
        Some("html") | Some("htm") => "text/html",
        Some("xlsx") => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        Some("xls") => "application/vnd.ms-excel",
        Some("docx") => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        Some("zip") => "application/zip",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        _ => "application/octet-stream",
    }
}

/// Spawns `cmd`, optionally capturing stdout (draining it on a concurrent reader thread
/// so a chatty child can't deadlock against an undrained pipe while the caller is only
/// polling `try_wait()`), and kills it if `timeout` elapses first. `timeout: None` means
/// wait indefinitely, same as the plain `.status()`/`.output()` this replaces.
fn run_with_timeout(
    mut cmd: std::process::Command,
    capture: bool,
    timeout: Option<u64>,
) -> Result<(bool, Option<i32>, Option<String>)> {
    if capture {
        cmd.stdout(std::process::Stdio::piped());
    }
    let mut child = cmd.spawn()?;
    let reader = capture.then(|| {
        let mut out = child.stdout.take().expect("stdout was piped");
        std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = String::new();
            let _ = out.read_to_string(&mut buf);
            buf
        })
    });

    let deadline = timeout.map(|secs| Instant::now() + Duration::from_secs(secs));
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if let Some(dl) = deadline
            && Instant::now() >= dl
        {
            let _ = child.kill();
            let _ = child.wait();
            bail!("command timed out after {}s", timeout.unwrap());
        }
        std::thread::sleep(Duration::from_millis(100));
    };

    let captured = reader.map(|r| r.join().unwrap_or_default().trim().to_string());
    Ok((status.success(), status.code(), captured))
}

// ── Actions ───────────────────────────────────────────────────────────────────

fn check_url(url: &str, quiet: bool) -> Result<()> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;
    match client.get(url).send() {
        Ok(r) if r.status().is_success() => {
            if !quiet {
                println!(
                    "  {} {} ({})",
                    "✓".green().bold(),
                    url,
                    r.status().as_u16().to_string().green()
                );
            }
            Ok(())
        }
        Ok(r) => bail!("HTTP {}", r.status().as_u16()),
        Err(e) => bail!("{e}"),
    }
}

/// Executes an `http:` task's request: renders headers/body against `vars`, sends, and
/// returns the response's (body text, status code) — mirrors `check_url`'s
/// client-builder pattern. Bails on a network error, or (unless `spec.ignore_status`) a
/// non-2xx status, with the response body (truncated) in the error message.
/// Builds the request (method, headers, body — all rendered) shared by `http_request`
/// and `http_download`. Neither `.send()`s it nor decides how to read the response body,
/// since that differs between the two (text vs. binary-safe bytes).
fn build_http_request(
    spec: &HttpSpec,
    url: &str,
    vars: &HashMap<String, String>,
) -> Result<reqwest::blocking::RequestBuilder> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(spec.timeout))
        .build()?;
    let method = reqwest::Method::from_bytes(spec.method.to_uppercase().as_bytes())
        .map_err(|_| anyhow::anyhow!("invalid http method: {}", spec.method))?;
    let mut req = client.request(method, url);
    for (k, v) in &spec.headers {
        req = req.header(render(k, vars), render(v, vars));
    }
    if let Some(body) = &spec.body {
        req = req.body(render(body, vars));
    }
    Ok(req)
}

fn http_request(
    spec: &HttpSpec,
    url: &str,
    vars: &HashMap<String, String>,
) -> Result<(String, u16)> {
    let resp = build_http_request(spec, url, vars)?
        .send()
        .with_context(|| format!("http request failed: {url}"))?;
    let status = resp.status();
    let body = resp.text().unwrap_or_default();
    if !spec.ignore_status && !status.is_success() {
        let snippet: String = body.chars().take(300).collect();
        if snippet.trim().is_empty() {
            bail!("HTTP {}", status.as_u16());
        }
        bail!("HTTP {}: {snippet}", status.as_u16());
    }
    Ok((body, status.as_u16()))
}

/// `http: {download: ...}`'s engine — same request as `http_request`, but reads the
/// response as raw bytes (`resp.bytes()`, never `.text()`) and writes them straight to
/// `out_path`, so a binary response (PDF/zip/image) survives intact instead of being
/// mangled through lossy UTF-8 decoding. On a non-2xx status, still surfaces a
/// best-effort text snippet in the error (lossy-decoded, for diagnostics only) without
/// writing anything to disk.
fn http_download(
    spec: &HttpSpec,
    url: &str,
    vars: &HashMap<String, String>,
    out_path: &Path,
) -> Result<(u64, u16)> {
    let resp = build_http_request(spec, url, vars)?
        .send()
        .with_context(|| format!("http request failed: {url}"))?;
    let status = resp.status();
    let bytes = resp
        .bytes()
        .with_context(|| format!("reading response body: {url}"))?;
    if !spec.ignore_status && !status.is_success() {
        let snippet = String::from_utf8_lossy(&bytes[..bytes.len().min(300)]).into_owned();
        if snippet.trim().is_empty() {
            bail!("HTTP {}", status.as_u16());
        }
        bail!("HTTP {}: {snippet}", status.as_u16());
    }
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating parent directory for {}", out_path.display()))?;
    }
    std::fs::write(out_path, &bytes)
        .with_context(|| format!("writing downloaded file: {}", out_path.display()))?;
    Ok((bytes.len() as u64, status.as_u16()))
}

/// Executes a `scrape:` task: GETs `scrape_spec.url`, parses the HTML, and extracts one
/// `serde_json::Map` per `each:` match (or a single implicit whole-document match if
/// `each:` is absent). Each `fields:` entry is a CSS selector, optionally suffixed with
/// `@<attr>` (see `parse_field_selector`) to grab an attribute instead of trimmed text
/// content; a selector with no match in a given scope yields an empty string rather than
/// failing the task. Same client-builder pattern as `check_url`/`http_request`, with an
/// explicit User-Agent — a well-behaved client, not an evasive one: same trust model as
/// `check_url`/`http:` already have, the user supplies the URL, `tooler` doesn't target
/// sites, rotate proxies, or bypass bot detection.
fn scrape(
    scrape_spec: &ScrapeSpec,
    url: &str,
    vars: &HashMap<String, String>,
) -> Result<Vec<serde_json::Map<String, serde_json::Value>>> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(scrape_spec.timeout))
        .user_agent(format!("tooler/{}", env!("CARGO_PKG_VERSION")))
        .build()?;
    let mut req = client.get(url);
    for (k, v) in &scrape_spec.headers {
        req = req.header(render(k, vars), render(v, vars));
    }
    let resp = req
        .send()
        .with_context(|| format!("scrape request failed: {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        bail!("HTTP {} scraping {url}", status.as_u16());
    }
    let body = resp.text().context("scrape response was not valid text")?;
    let document = scraper::Html::parse_document(&body);

    let field_selectors: Vec<(String, scraper::Selector, Option<String>)> = scrape_spec
        .fields
        .iter()
        .map(|(name, field_spec)| {
            let (css, attr) = parse_field_selector(field_spec);
            let selector = scraper::Selector::parse(css).map_err(|e| {
                anyhow::anyhow!("invalid CSS selector '{css}' for field '{name}': {e:?}")
            })?;
            Ok((name.clone(), selector, attr.map(str::to_string)))
        })
        .collect::<Result<Vec<_>>>()?;

    let extract = |scope: scraper::ElementRef<'_>| -> serde_json::Map<String, serde_json::Value> {
        let mut obj = serde_json::Map::new();
        for (name, selector, attr) in &field_selectors {
            let value = scope
                .select(selector)
                .next()
                .map(|el| match attr {
                    Some(a) => el.value().attr(a).unwrap_or_default().to_string(),
                    None => el.text().collect::<String>().trim().to_string(),
                })
                .unwrap_or_default();
            obj.insert(name.clone(), serde_json::Value::String(value));
        }
        obj
    };

    match &scrape_spec.each {
        Some(each) => {
            let row_selector = scraper::Selector::parse(each)
                .map_err(|e| anyhow::anyhow!("invalid CSS selector '{each}' for each: {e:?}"))?;
            Ok(document.select(&row_selector).map(extract).collect())
        }
        None => Ok(vec![extract(document.root_element())]),
    }
}

/// Splits a `fields:` value like `"a.title@href"` into a CSS selector and an optional
/// attribute name — `"a.title"` alone means "trimmed text content". Splits on the last
/// `@`, so a plain selector with no `@` (or an empty piece on either side) is left
/// untouched with no attribute.
fn parse_field_selector(spec: &str) -> (&str, Option<&str>) {
    match spec.rsplit_once('@') {
        Some((css, attr)) if !css.is_empty() && !attr.is_empty() => (css, Some(attr)),
        _ => (spec, None),
    }
}

fn check_port(host: &str, port: u16, timeout_secs: u64, quiet: bool) -> Result<()> {
    use std::net::ToSocketAddrs;
    let addr = format!("{host}:{port}");
    let socket = addr
        .to_socket_addrs()
        .with_context(|| format!("Cannot resolve '{addr}'"))?
        .next()
        .with_context(|| format!("No address for '{addr}'"))?;

    std::net::TcpStream::connect_timeout(&socket, Duration::from_secs(timeout_secs))
        .map(|_| {
            if !quiet {
                println!("  {} {host}:{port} is open", "✓".green().bold());
            }
        })
        .map_err(|e| anyhow::anyhow!("{host}:{port} — {e}"))
}

fn env_check(reference: &str, target: &str, quiet: bool) -> Result<()> {
    let parse = |path: &str| -> Result<std::collections::HashSet<String>> {
        let content =
            std::fs::read_to_string(path).with_context(|| format!("Cannot read {path}"))?;
        Ok(content
            .lines()
            .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
            .filter_map(|l| l.split_once('=').map(|(k, _)| k.trim().to_string()))
            .collect())
    };

    let ref_keys = parse(reference)?;
    let tgt_keys = parse(target)?;
    let mut missing: Vec<&String> = ref_keys.iter().filter(|k| !tgt_keys.contains(*k)).collect();

    if missing.is_empty() {
        if !quiet {
            println!("  {} all keys present in {target}", "✓".green().bold());
        }
        Ok(())
    } else {
        missing.sort();
        bail!(
            "missing in {target}: {}",
            missing
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Resolves a single `{{token}}`: the playbook's own vars first, then `env.<name>` (the
/// process environment) and `secret.<profile>.<key>` (the OS keychain, same store as
/// `tooler config set profile.<name>.token`/OAuth2 profiles — see `secrets::get_secret`).
/// `None` means "leave the token literal" — covers both a genuinely unknown name and a
/// secret lookup that failed (unset key, or the keychain itself being unreachable), so a
/// playbook never crashes over a missing secret, it just doesn't get substituted.
fn resolve_token(token: &str, vars: &HashMap<String, String>) -> Option<String> {
    if let Some(v) = vars.get(token) {
        return Some(v.clone());
    }
    if let Some(name) = token.strip_prefix("env.") {
        return std::env::var(name).ok();
    }
    if let Some(rest) = token.strip_prefix("secret.") {
        let (profile, key) = rest.split_once('.')?;
        return crate::secrets::get_secret(profile, key).ok().flatten();
    }
    None
}

/// Single-pass `{{token}}` substitution shared by `render()` and `render_for_display()` —
/// the scan is identical, only how a resolved *token* (post `split_filter`) is turned into
/// a replacement string differs (real value vs. masked). A trailing `| json:<path>` or
/// `| quote` filter (see `split_filter`/`apply_json_filter`/`shell_quote`) is applied
/// uniformly regardless of `resolve`, so `render_for_display` masks-then-filters too: a
/// masked secret piped through `| json:...` never parses as JSON and just stays
/// unresolved, while one piped through `| quote` becomes `'***'` — either way the real
/// value never leaks. Unresolvable tokens (unknown name, bad filter) are left exactly as
/// written, same as the old known-vars-only replace loop this superseded.
fn render_with(s: &str, resolve: impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            out.push_str("{{");
            rest = after;
            continue;
        };
        let inner = after[..end].trim();
        let (token, filter) = split_filter(inner);
        let resolved = resolve(token).and_then(|v| match filter {
            Some(Filter::Json(path)) => apply_json_filter(&v, path),
            Some(Filter::Quote) => Some(shell_quote(&v)),
            None => Some(v),
        });
        out.push_str(&resolved.unwrap_or_else(|| format!("{{{{{inner}}}}}")));
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    out
}

/// A `{{token | ...}}` render filter — see `split_filter`. Only one filter is recognized
/// per token (no chaining `json:` into `quote`); a value that needs both goes through
/// `set_fact:` first to compute an intermediate var, same two-step workaround the DSL
/// already uses for anything else that needs more than one transform.
enum Filter<'a> {
    /// `| json:<path>` — see `apply_json_filter`.
    Json(&'a str),
    /// `| quote` — see `shell_quote`.
    Quote,
}

/// Splits a `{{...}}` token's trimmed inner text on an optional trailing `| json:<path>`
/// or `| quote` filter — e.g. `"resp | json:data.id"` -> `("resp", Some(Filter::Json("data.id")))`,
/// `"item | quote"` -> `("item", Some(Filter::Quote))`. Only these two filters are
/// recognized; anything else after a `|` is left as part of the token name (so a stray
/// `|` doesn't silently vanish) and will simply fail to resolve like any unknown token.
fn split_filter(inner: &str) -> (&str, Option<Filter<'_>>) {
    if let Some((token, filter)) = inner.split_once('|') {
        let filter = filter.trim();
        if let Some(path) = filter.strip_prefix("json:") {
            return (token.trim(), Some(Filter::Json(path.trim())));
        }
        if filter == "quote" {
            return (token.trim(), Some(Filter::Quote));
        }
    }
    (inner, None)
}

/// POSIX single-quote escaping for a value about to be interpolated into a `run:`/`ssh:`/
/// `fleet:` shell command line via `| quote`: wraps `value` in single quotes, escaping any
/// embedded `'` as `'\''` (close the quote, emit an escaped literal quote, reopen it) —
/// the standard shlex-safe technique. Targets the `sh -c` `run:` already shells out to
/// (see `run_task_once`'s `task.run` branch), not a non-POSIX shell. Always succeeds
/// (unlike `apply_json_filter`, there's no "doesn't match" case), so `| quote` never
/// leaves a token unresolved the way a bad `json:` path can. See the README's "Trust
/// model" section for why this exists: a `{{var}}` sourced from untrusted external data
/// (`scrape:`, `http:` + `json:`, a `db_query:` row, a dynamic `loop: {from: ...}` item)
/// can otherwise inject shell metacharacters straight into `run:`'s command line.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// Applies a `json:<path>` filter to `value` (parsed as JSON), walking dot-separated
/// `path` segments, each optionally suffixed with one or more `[N]` array indices (e.g.
/// `data.items[0].title`, `[2]`). A final segment of exactly `length` returns the
/// current value's element/key/char count instead of doing a field lookup (arrays have
/// no literal `"length"` field to `.get()`) — e.g. `{{prs | json:length}}`,
/// `{{resp | json:data.items.length}}`. A string leaf renders raw (unquoted); any other
/// JSON value (number/bool/object/array/null) renders via its JSON text form. Returns
/// `None` on invalid JSON, a path that doesn't match, or `length` on a value that has no
/// length (number/bool/null) — `render_with` then leaves the whole `{{...}}` token
/// literal, same as any other unresolvable token.
fn apply_json_filter(value: &str, path: &str) -> Option<String> {
    let root: serde_json::Value = serde_json::from_str(value).ok()?;
    let mut cur = &root;
    let segments: Vec<&str> = path.split('.').filter(|s| !s.is_empty()).collect();
    for (i, segment) in segments.iter().enumerate() {
        if *segment == "length" && i == segments.len() - 1 {
            let len = match cur {
                serde_json::Value::Array(a) => a.len(),
                serde_json::Value::Object(o) => o.len(),
                serde_json::Value::String(s) => s.chars().count(),
                _ => return None,
            };
            return Some(len.to_string());
        }
        let (field, indices) = parse_path_segment(segment);
        if !field.is_empty() {
            cur = cur.get(field)?;
        }
        for idx in indices {
            cur = cur.get(idx)?;
        }
    }
    Some(match cur {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    })
}

/// Splits one `.`-separated path segment like `items[0]` or `[2]` into an optional field
/// name and zero or more array indices, so `data.items[0][1]` chains cleanly.
fn parse_path_segment(segment: &str) -> (&str, Vec<usize>) {
    let bracket = segment.find('[');
    let field = &segment[..bracket.unwrap_or(segment.len())];
    let mut rest = bracket.map(|b| &segment[b..]).unwrap_or("");
    let mut indices = Vec::new();
    while let Some(stripped) = rest.strip_prefix('[') {
        let Some(close) = stripped.find(']') else {
            break;
        };
        if let Ok(idx) = stripped[..close].parse::<usize>() {
            indices.push(idx);
        }
        rest = &stripped[close + 1..];
    }
    (field, indices)
}

/// Resolves and substitutes every `{{token}}` in `s` for real — see `resolve_token` for
/// resolution order. This is the value actually used to run a command / build a request;
/// for a copy meant only to be printed, use `render_for_display` instead so a secret isn't
/// echoed in cleartext.
fn render(s: &str, vars: &HashMap<String, String>) -> String {
    render_with(s, |t| resolve_token(t, vars))
}

/// Same substitution as `render()`, except a `{{secret.<profile>.<key>}}` token resolves to
/// the literal `***` instead of its real value. Used only for lines that get `println!`'d
/// (echoing a `run:`/`ssh:`/`fleet:`/`sync_files:` command) — never for the string actually
/// executed, which must stay `render()`'s real, unmasked output. `debug:` is a deliberate
/// exception and stays on plain `render()`: printing *is* its entire purpose, so masking it
/// would defeat the point of the action.
fn render_for_display(s: &str, vars: &HashMap<String, String>) -> String {
    render_with(s, |t| {
        if t.starts_with("secret.") {
            Some("***".to_string())
        } else {
            resolve_token(t, vars)
        }
    })
}

fn print_recap(ok: usize, failed: usize, skipped: usize) {
    println!(
        "\n{}  {}  {}  {}",
        "RECAP".bold(),
        format!("ok={ok}").green().bold(),
        if failed > 0 {
            format!("failed={failed}").red().bold()
        } else {
            format!("failed={failed}").dimmed()
        },
        format!("skipped={skipped}").dimmed(),
    );
}

// ── Sample playbook ───────────────────────────────────────────────────────────

fn write_sample(path: &Path, ctx: &Context) -> Result<()> {
    let json = ctx.output == OutputFormat::Json;
    let path_str = path.display().to_string();
    if path.exists() {
        let message = format!("'{}' already exists.", path_str);
        if json {
            println!(
                "{}",
                serde_json::json!({"path": path_str, "error": message})
            );
            std::process::exit(1);
        }
        bail!(message);
    }

    let sample = r#"name: My Playbook
description: Sample tooler playbook

vars:
  host: localhost
  port: "8080"
  env_file: .env

tasks:
  - name: Check env file is complete
    env_check:
      reference: .env.example
      target: "{{env_file}}"

  - name: Build project
    run: cargo build --release
    tags: [build]

  - name: Run tests
    run: cargo test
    tags: [test]
    ignore_errors: true

  - name: Health check
    check_url: http://{{host}}:{{port}}/health
    tags: [deploy]

  - name: Verify database port
    check_port:
      host: "{{host}}"
      port: 5432
      timeout: 3
    tags: [deploy]
"#;

    std::fs::write(path, sample)?;
    if json {
        println!("{}", serde_json::json!({"created": path_str}));
        return Ok(());
    }
    println!("{} {}", "created".green().bold(), path_str.cyan());
    let invocation =
        if path.parent().and_then(Path::file_name) == Some(std::ffi::OsStr::new("playbooks")) {
            path.file_stem().unwrap().to_string_lossy().to_string()
        } else {
            path_str
        };
    println!(
        "{}",
        format!("Run it with: tooler play {invocation}").dimmed()
    );
    Ok(())
}

// ── Listing ───────────────────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
struct PlaybookHeader {
    #[serde(default)]
    description: Option<String>,
}

fn list_playbooks(dir: &Path, ctx: &Context) -> Result<()> {
    let json = ctx.output == OutputFormat::Json;

    let mut entries: Vec<(String, PathBuf)> = Vec::new();
    if dir.is_dir() {
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            if path.extension().is_some_and(|e| e == "yml" || e == "yaml") {
                let name = path.file_stem().unwrap().to_string_lossy().to_string();
                entries.push((name, path));
            }
        }
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let describe = |path: &Path| -> Option<String> {
        let content = std::fs::read_to_string(path).ok()?;
        serde_yaml::from_str::<PlaybookHeader>(&content)
            .ok()?
            .description
    };

    if json {
        let playbooks: Vec<_> = entries
            .iter()
            .map(|(name, path)| {
                serde_json::json!({
                    "name": name,
                    "file": path.display().to_string(),
                    "description": describe(path),
                    "has_notes": notes_path(path).exists(),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::json!({"dir": dir.display().to_string(), "playbooks": playbooks})
        );
        return Ok(());
    }

    if entries.is_empty() {
        println!("{}", "No playbooks found.".dimmed());
        println!("Create one with: {}", "tooler play --init <name>".dimmed());
        return Ok(());
    }

    println!(
        "{} {}",
        "playbooks:".bold().cyan(),
        dir.display().to_string().dimmed()
    );
    println!("{}", "─".repeat(40).dimmed());
    for (name, path) in &entries {
        let desc = describe(path).unwrap_or_default();
        let notes_marker = if notes_path(path).exists() {
            " [notes]".dimmed().to_string()
        } else {
            String::new()
        };
        println!("  {:20} {}{}", name.bold(), desc.dimmed(), notes_marker);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_literal_path_detects_slash_and_extensions() {
        assert!(is_literal_path("playbook.yml"));
        assert!(is_literal_path("playbook.yaml"));
        assert!(is_literal_path("./playbooks/deploy.yml"));
        assert!(is_literal_path("sub/deploy"));
    }

    #[test]
    fn is_literal_path_false_for_bare_names() {
        assert!(!is_literal_path("deploy"));
        assert!(!is_literal_path("smoke-test"));
    }

    fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn eval_when_equality() {
        let v = vars(&[("env", "prod")]);
        assert!(eval_when("{{env}} == prod", &v));
        assert!(!eval_when("{{env}} == staging", &v));
    }

    #[test]
    fn eval_when_inequality() {
        let v = vars(&[("env", "prod")]);
        assert!(eval_when("{{env}} != staging", &v));
        assert!(!eval_when("{{env}} != prod", &v));
    }

    #[test]
    fn eval_when_truthy_bare_value() {
        let v = vars(&[("enabled", "yes")]);
        assert!(eval_when("{{enabled}}", &v));
        let v = vars(&[("enabled", "false")]);
        assert!(!eval_when("{{enabled}}", &v));
        let v = vars(&[("enabled", "")]);
        assert!(!eval_when("{{enabled}}", &v));
    }

    #[test]
    fn eval_when_greater_than() {
        let v = vars(&[("count", "3")]);
        assert!(!eval_when("{{count}} > 5", &v));
        let v = vars(&[("count", "9")]);
        assert!(eval_when("{{count}} > 5", &v));
    }

    #[test]
    fn eval_when_less_than() {
        let v = vars(&[("count", "3")]);
        assert!(eval_when("{{count}} < 5", &v));
        let v = vars(&[("count", "9")]);
        assert!(!eval_when("{{count}} < 5", &v));
    }

    #[test]
    fn eval_when_greater_or_equal() {
        let v = vars(&[("count", "5")]);
        assert!(eval_when("{{count}} >= 5", &v));
        let v = vars(&[("count", "4")]);
        assert!(!eval_when("{{count}} >= 5", &v));
    }

    #[test]
    fn eval_when_less_or_equal() {
        let v = vars(&[("count", "5")]);
        assert!(eval_when("{{count}} <= 5", &v));
        let v = vars(&[("count", "6")]);
        assert!(!eval_when("{{count}} <= 5", &v));
    }

    #[test]
    fn eval_when_comparison_is_false_when_either_side_is_not_numeric() {
        // The exact bug this fix closes: a non-numeric comparison must not silently
        // fall through to "truthy" (which would make it always true).
        let v = vars(&[("count", "not-a-number")]);
        assert!(!eval_when("{{count}} > 5", &v));
        assert!(!eval_when("{{count}} < 5", &v));
        assert!(!eval_when("{{count}} >= 5", &v));
        assert!(!eval_when("{{count}} <= 5", &v));
    }

    #[test]
    fn notes_path_swaps_yml_extension_for_md() {
        assert_eq!(
            notes_path(Path::new("playbooks/deploy.yml")),
            PathBuf::from("playbooks/deploy.md")
        );
        assert_eq!(
            notes_path(Path::new("playbooks/deploy.yaml")),
            PathBuf::from("playbooks/deploy.md")
        );
    }

    #[test]
    fn register_is_rejected_for_check_and_include_actions() {
        let ctx = Context::new(
            OutputFormat::Json,
            "default".to_string(),
            crate::config::Config::default(),
        );
        let env = dry_env(&ctx);
        let mut vars = HashMap::new();
        let mut include_stack = Vec::new();

        let task = Task {
            register: Some("x".to_string()),
            check_url: Some("http://example.com".to_string()),
            ..Default::default()
        };
        assert!(run_task_once(&task, &mut vars, &mut include_stack, &env).is_err());

        let task = Task {
            register: Some("x".to_string()),
            include: Some(IncludeSpec::Simple("sub.yml".to_string())),
            ..Default::default()
        };
        assert!(run_task_once(&task, &mut vars, &mut include_stack, &env).is_err());
    }

    #[test]
    fn register_is_allowed_for_run_action() {
        let ctx = Context::new(
            OutputFormat::Json,
            "default".to_string(),
            crate::config::Config::default(),
        );
        // dry: true — no real subprocess runs, so this only exercises the upfront
        // register-validation guard, not actual command execution.
        let env = dry_env(&ctx);
        let mut vars = HashMap::new();
        let mut include_stack = Vec::new();
        let task = Task {
            register: Some("x".to_string()),
            run: Some("echo hi".to_string()),
            ..Default::default()
        };
        assert!(run_task_once(&task, &mut vars, &mut include_stack, &env).is_ok());
    }

    fn dry_env(ctx: &Context) -> RunEnv<'_> {
        RunEnv {
            playbook_dir: PathBuf::from("."),
            playbook_name: "test".to_string(),
            audit_log: None,
            project_root: PathBuf::from("."),
            dry: true,
            diff: false,
            quiet: true,
            auto_yes: false,
            start_at: None,
            state_path: None,
            data_path: None,
            ctx,
        }
    }

    #[test]
    fn assert_true_succeeds_and_assert_false_fails() {
        let ctx = Context::new(
            OutputFormat::Json,
            "default".to_string(),
            crate::config::Config::default(),
        );
        let env = dry_env(&ctx);
        let mut vars = vars(&[("env", "prod")]);
        let mut include_stack = Vec::new();

        let task = Task {
            assert: Some("{{env}} == prod".to_string()),
            ..Default::default()
        };
        assert!(run_task_once(&task, &mut vars, &mut include_stack, &env).is_ok());

        let task = Task {
            assert: Some("{{env}} == staging".to_string()),
            ..Default::default()
        };
        assert!(run_task_once(&task, &mut vars, &mut include_stack, &env).is_err());
    }

    #[test]
    fn render_still_substitutes_known_vars() {
        let v = vars(&[("name", "world")]);
        assert_eq!(render("hello {{name}}", &v), "hello world");
    }

    #[test]
    fn render_for_display_masks_secret_tokens_but_not_others() {
        let v = vars(&[("name", "world"), ("env.PATH_LIKE", "unused")]);
        // A secret token is masked for display...
        assert_eq!(
            render_for_display("token={{secret.myprofile.api_key}}", &v),
            "token=***"
        );
        // ...but the real render() still substitutes it for actual execution.
        // (secret.myprofile.api_key isn't stored, so it stays literal here — this
        // just confirms render_for_display's masking is independent of resolve_token.)
        assert_eq!(
            render("token={{secret.myprofile.api_key}}", &v),
            "token={{secret.myprofile.api_key}}"
        );
        // Plain vars are unaffected by render_for_display.
        assert_eq!(render_for_display("hello {{name}}", &v), "hello world");
    }

    #[test]
    fn shell_quote_wraps_a_plain_value() {
        assert_eq!(shell_quote("hello world"), "'hello world'");
    }

    #[test]
    fn shell_quote_escapes_embedded_single_quotes() {
        assert_eq!(shell_quote("it's here"), r"'it'\''s here'");
    }

    #[test]
    fn quote_filter_resolves_a_var() {
        let v = vars(&[("item", "; rm -rf /")]);
        assert_eq!(render("echo {{item | quote}}", &v), "echo '; rm -rf /'");
    }

    #[test]
    fn quote_filter_on_an_unknown_token_stays_literal() {
        let v = vars(&[]);
        assert_eq!(render("{{missing | quote}}", &v), "{{missing | quote}}");
    }

    #[test]
    fn render_for_display_quotes_a_masked_secret_without_leaking() {
        let v = vars(&[]);
        // The real value never resolves (not stored), but the point stands even when
        // it would: render_for_display masks to "***" before the filter runs, so the
        // filtered result is always the masked placeholder, quoted -- never the secret.
        assert_eq!(render_for_display("{{secret.p.k | quote}}", &v), "'***'");
    }

    #[test]
    fn json_filter_extracts_object_field_and_array_index() {
        let v = vars(&[("resp", r#"{"data":{"id":42,"items":["a","b","c"]}}"#)]);
        assert_eq!(render("{{resp | json:data.id}}", &v), "42");
        assert_eq!(render("{{resp | json:data.items[1]}}", &v), "b");
    }

    #[test]
    fn json_filter_length_of_array() {
        let v = vars(&[("resp", r#"["a","b","c"]"#)]);
        assert_eq!(render("{{resp | json:length}}", &v), "3");
    }

    #[test]
    fn json_filter_length_of_object() {
        let v = vars(&[("resp", r#"{"a":1,"b":2}"#)]);
        assert_eq!(render("{{resp | json:length}}", &v), "2");
    }

    #[test]
    fn json_filter_length_of_string() {
        let v = vars(&[("resp", r#""hello""#)]);
        assert_eq!(render("{{resp | json:length}}", &v), "5");
    }

    #[test]
    fn json_filter_length_after_a_nested_path() {
        let v = vars(&[("resp", r#"{"data":{"items":["a","b"]}}"#)]);
        assert_eq!(render("{{resp | json:data.items.length}}", &v), "2");
    }

    #[test]
    fn json_filter_length_on_a_scalar_stays_literal() {
        let v = vars(&[("resp", "42")]);
        assert_eq!(
            render("{{resp | json:length}}", &v),
            "{{resp | json:length}}"
        );
    }

    #[test]
    fn json_filter_stays_literal_on_bad_json_or_missing_path() {
        let v = vars(&[("resp", "not json")]);
        assert_eq!(
            render("{{resp | json:data.id}}", &v),
            "{{resp | json:data.id}}"
        );
        let v = vars(&[("resp", r#"{"data":{}}"#)]);
        assert_eq!(
            render("{{resp | json:data.missing}}", &v),
            "{{resp | json:data.missing}}"
        );
    }

    #[test]
    fn json_filter_on_a_masked_secret_never_resolves() {
        // render_for_display masks {{secret.*}} to "***", which isn't valid JSON — a
        // `| json:` filter piped onto it must stay unresolved, never leak partial data.
        let v = vars(&[]);
        assert_eq!(
            render_for_display("{{secret.p.k | json:token}}", &v),
            "{{secret.p.k | json:token}}"
        );
    }

    #[test]
    fn http_spec_deserializes_with_defaults() {
        let spec: HttpSpec = serde_yaml::from_str("url: https://example.com\n").unwrap();
        assert_eq!(spec.method, "GET");
        assert_eq!(spec.timeout, 5);
        assert!(!spec.ignore_status);
        assert!(spec.headers.is_empty());
    }

    #[test]
    fn http_spec_deserializes_full_fields() {
        let spec: HttpSpec = serde_yaml::from_str(
            "method: POST\nurl: https://example.com\nheaders:\n  X-Test: \"1\"\nbody: '{}'\ntimeout: 10\nignore_status: true\n",
        )
        .unwrap();
        assert_eq!(spec.method, "POST");
        assert_eq!(spec.headers.get("X-Test").map(String::as_str), Some("1"));
        assert_eq!(spec.body.as_deref(), Some("{}"));
        assert_eq!(spec.timeout, 10);
        assert!(spec.ignore_status);
    }

    #[test]
    fn http_spec_deserializes_download_field() {
        let spec: HttpSpec =
            serde_yaml::from_str("url: https://example.com/f.pdf\ndownload: out/f.pdf\n").unwrap();
        assert_eq!(spec.download.as_deref(), Some("out/f.pdf"));
    }

    #[test]
    fn write_file_spec_deserializes_with_default_append() {
        let spec: WriteFileSpec = serde_yaml::from_str("path: out.txt\ncontent: hello\n").unwrap();
        assert_eq!(spec.path, "out.txt");
        assert_eq!(spec.content, "hello");
        assert!(!spec.append);
    }

    #[test]
    fn write_file_spec_deserializes_with_append() {
        let spec: WriteFileSpec =
            serde_yaml::from_str("path: out.txt\ncontent: hello\nappend: true\n").unwrap();
        assert!(spec.append);
    }

    #[test]
    fn read_csv_spec_deserializes_with_defaults() {
        let spec: ReadCsvSpec = serde_yaml::from_str("path: data.csv\n").unwrap();
        assert_eq!(spec.path, "data.csv");
        assert!(spec.headers);
        assert!(spec.delimiter.is_none());
    }

    #[test]
    fn read_csv_spec_deserializes_explicit_fields() {
        let spec: ReadCsvSpec =
            serde_yaml::from_str("path: data.tsv\nheaders: false\ndelimiter: \"\\t\"\n").unwrap();
        assert!(!spec.headers);
        assert_eq!(spec.delimiter.as_deref(), Some("\t"));
    }

    #[test]
    fn write_csv_spec_deserializes_with_defaults() {
        let spec: WriteCsvSpec =
            serde_yaml::from_str("path: out.csv\ndata: \"{{rows}}\"\n").unwrap();
        assert_eq!(spec.path, "out.csv");
        assert!(spec.headers);
        assert!(spec.delimiter.is_none());
    }

    #[test]
    fn write_csv_spec_deserializes_explicit_fields() {
        let spec: WriteCsvSpec = serde_yaml::from_str(
            "path: out.tsv\ndata: \"{{rows}}\"\nheaders: false\ndelimiter: \"\\t\"\n",
        )
        .unwrap();
        assert!(!spec.headers);
        assert_eq!(spec.delimiter.as_deref(), Some("\t"));
    }

    #[test]
    fn json_cell_to_string_unwraps_strings_and_stringifies_others() {
        assert_eq!(
            json_cell_to_string(&serde_json::Value::String("hi".to_string())),
            "hi"
        );
        assert_eq!(
            json_cell_to_string(&serde_json::Value::Number(3.into())),
            "3"
        );
        assert_eq!(json_cell_to_string(&serde_json::Value::Null), "");
    }

    #[test]
    fn guess_mime_covers_common_extensions_and_falls_back() {
        assert_eq!(guess_mime(Path::new("report.pdf")), "application/pdf");
        assert_eq!(guess_mime(Path::new("data.CSV")), "text/csv");
        assert_eq!(
            guess_mime(Path::new("mystery.xyz")),
            "application/octet-stream"
        );
        assert_eq!(
            guess_mime(Path::new("noextension")),
            "application/octet-stream"
        );
    }

    #[test]
    fn task_action_label_identifies_run_and_debug_and_falls_back_to_unknown() {
        let run_task = Task {
            run: Some("echo hi".to_string()),
            ..Default::default()
        };
        assert_eq!(task_action_label(&run_task), "run");

        let debug_task = Task {
            debug: Some("hi".to_string()),
            ..Default::default()
        };
        assert_eq!(task_action_label(&debug_task), "debug");

        let no_action_task = Task::default();
        assert_eq!(task_action_label(&no_action_task), "unknown");
    }

    #[test]
    fn fs_cat_spec_deserializes() {
        let spec: FsCatSpec = serde_yaml::from_str("server: web1\npath: /etc/app/.env\n").unwrap();
        assert_eq!(spec.server, "web1");
        assert_eq!(spec.path, "/etc/app/.env");
    }

    #[test]
    fn fs_write_spec_deserializes_with_default_confirm() {
        let spec: FsWriteSpec =
            serde_yaml::from_str("server: web1\npath: /tmp/x\ncontent: hi\n").unwrap();
        assert!(!spec.confirm);
    }

    #[test]
    fn confirmed_require_rejects_an_unconfirmed_spec_with_a_clear_message() {
        let spec: FsWriteSpec =
            serde_yaml::from_str("server: web1\npath: /tmp/x\ncontent: hi\n").unwrap();
        let err = Confirmed::require(&spec, "fs_write", "overwrite it").unwrap_err();
        assert!(
            err.to_string()
                .contains("fs_write: refused to run without confirm: true (task 'overwrite it')"),
            "error was: {err}"
        );
    }

    #[test]
    fn confirmed_require_accepts_a_confirmed_spec() {
        let spec: FsWriteSpec =
            serde_yaml::from_str("server: web1\npath: /tmp/x\ncontent: hi\nconfirm: true\n")
                .unwrap();
        let confirmed = Confirmed::require(&spec, "fs_write", "overwrite it").unwrap();
        // Deref gives access to the spec's own fields through the proof wrapper.
        assert_eq!(confirmed.path, "/tmp/x");
    }

    #[test]
    fn fs_write_spec_rejects_an_unknown_field_instead_of_silently_dropping_it() {
        let err = serde_yaml::from_str::<FsWriteSpec>(
            "server: web1\npath: /tmp/x\ncontent: hi\nconfrim: true\n",
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("unknown field `confrim`"),
            "error was: {err}"
        );
    }

    #[test]
    fn systemd_restart_spec_deserializes_with_defaults() {
        let spec: SystemdRestartSpec = serde_yaml::from_str("server: web1\nunit: nginx\n").unwrap();
        assert!(!spec.sudo);
        assert!(spec.sudo_pass.is_none());
        assert!(!spec.confirm);
    }

    #[test]
    fn systemd_restart_spec_deserializes_explicit_confirm() {
        let spec: SystemdRestartSpec =
            serde_yaml::from_str("server: web1\nunit: nginx\nconfirm: true\n").unwrap();
        assert!(spec.confirm);
    }

    #[test]
    fn systemd_status_spec_deserializes() {
        let spec: SystemdStatusSpec = serde_yaml::from_str("server: web1\nunit: nginx\n").unwrap();
        assert_eq!(spec.unit, "nginx");
    }

    #[test]
    fn logs_tail_spec_deserializes_with_default_lines() {
        let spec: LogsTailSpec =
            serde_yaml::from_str("server: web1\npath: /var/log/app.log\n").unwrap();
        assert_eq!(spec.lines, 100);
    }

    #[test]
    fn logs_grep_spec_deserializes_with_default_max_lines() {
        let spec: LogsGrepSpec =
            serde_yaml::from_str("server: web1\npath: /var/log/app.log\npattern: ERROR\n").unwrap();
        assert_eq!(spec.max_lines, 200);
    }

    #[test]
    fn ps_list_spec_deserializes_with_default_filter() {
        let spec: PsListSpec = serde_yaml::from_str("server: web1\n").unwrap();
        assert!(spec.filter.is_none());
    }

    #[test]
    fn ps_kill_spec_deserializes_with_defaults() {
        let spec: PsKillSpec = serde_yaml::from_str("server: web1\npid: 1234\n").unwrap();
        assert_eq!(spec.signal, "TERM");
        assert!(!spec.sudo);
        assert!(!spec.confirm);
    }

    #[test]
    fn ps_kill_spec_rejects_an_unknown_field_instead_of_silently_dropping_it() {
        let err = serde_yaml::from_str::<PsKillSpec>("server: web1\npid: 1234\nconfrim: true\n")
            .unwrap_err();
        assert!(
            err.to_string().contains("unknown field `confrim`"),
            "error was: {err}"
        );
    }

    #[test]
    fn stat_spec_deserializes() {
        let spec: StatSpec = serde_yaml::from_str("server: web1\n").unwrap();
        assert_eq!(spec.server, "web1");
    }

    #[test]
    fn git_summary_spec_deserializes_from_an_empty_map() {
        serde_yaml::from_str::<GitSummarySpec>("{}").unwrap();
    }

    #[test]
    fn git_changelog_spec_deserializes_with_default_from() {
        let spec: GitChangelogSpec = serde_yaml::from_str("{}").unwrap();
        assert!(spec.from.is_none());
    }

    #[test]
    fn gh_prs_spec_deserializes_with_defaults() {
        let spec: GhPrsSpec = serde_yaml::from_str("{}").unwrap();
        assert_eq!(spec.state, "all");
        assert_eq!(spec.limit, 500);
        assert!(spec.repo.is_none());
    }

    #[test]
    fn db_query_spec_deserializes_with_default_max_rows() {
        let spec: DbQuerySpec =
            serde_yaml::from_str("server: db1\nsql: SELECT 1\nenv: /var/www/.env\n").unwrap();
        assert_eq!(spec.server, "db1");
        assert_eq!(spec.sql, "SELECT 1");
        assert_eq!(spec.env.as_deref(), Some("/var/www/.env"));
        assert_eq!(spec.max_rows, 1000);
    }

    #[test]
    fn db_query_spec_deserializes_explicit_fields_and_max_rows() {
        let spec: DbQuerySpec = serde_yaml::from_str(
            "server: db1\nsql: SELECT 1\nengine: mysql\nhost: 127.0.0.1\nport: 3306\n\
             database: app\nuser: root\npassword: secret\nmax_rows: 50\n",
        )
        .unwrap();
        assert_eq!(spec.engine.as_deref(), Some("mysql"));
        assert_eq!(spec.port, Some(3306));
        assert_eq!(spec.max_rows, 50);
    }

    #[test]
    fn mail_spec_deserializes_with_server_profile() {
        let spec: MailSpec =
            serde_yaml::from_str("server: notif\nto: a@example.com\nsubject: hi\nbody: hello\n")
                .unwrap();
        assert_eq!(spec.server.as_deref(), Some("notif"));
        assert_eq!(spec.to, "a@example.com");
        assert!(!spec.html);
        assert!(spec.host.is_none());
    }

    #[test]
    fn mail_spec_deserializes_with_inline_host_fields() {
        let spec: MailSpec = serde_yaml::from_str(
            "to: a@example.com\nsubject: hi\nbody: hello\nhtml: true\n\
             host: smtp.example.com\nport: 465\nuser: u\npassword: p\ntls: tls\n",
        )
        .unwrap();
        assert!(spec.server.is_none());
        assert_eq!(spec.host.as_deref(), Some("smtp.example.com"));
        assert_eq!(spec.port, Some(465));
        assert!(spec.html);
        assert_eq!(spec.tls.as_deref(), Some("tls"));
    }

    #[test]
    fn mail_spec_deserializes_attachments() {
        let spec: MailSpec = serde_yaml::from_str(
            "server: notif\nto: a@example.com\nsubject: hi\nbody: hello\n\
             attachments:\n  - report.pdf\n  - data.csv\n",
        )
        .unwrap();
        assert_eq!(spec.attachments, vec!["report.pdf", "data.csv"]);
    }

    #[test]
    fn mail_spec_attachments_default_to_empty() {
        let spec: MailSpec =
            serde_yaml::from_str("server: notif\nto: a@example.com\nsubject: hi\nbody: hello\n")
                .unwrap();
        assert!(spec.attachments.is_empty());
    }

    #[test]
    fn mail_spec_rejects_an_unknown_field_instead_of_silently_dropping_it() {
        let err = serde_yaml::from_str::<MailSpec>(
            "servre: notif\nto: a@example.com\nsubject: hi\nbody: hello\n",
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("unknown field `servre`"),
            "error was: {err}"
        );
    }

    #[test]
    fn task_rejects_an_unknown_field_instead_of_silently_dropping_it() {
        // A typo'd delya:/registerr: alongside a valid run: used to succeed silently,
        // ignoring both -- see the deny_unknown_fields sweep across Task and every
        // *Spec struct.
        let err =
            serde_yaml::from_str::<Task>("name: t\nrun: echo hi\ndelya: 5\nregisterr: oops\n")
                .unwrap_err();
        assert!(
            err.to_string().contains("unknown field `delya`"),
            "error was: {err}"
        );
    }

    #[test]
    fn mail_check_spec_deserializes_with_defaults() {
        let spec: MailCheckSpec = serde_yaml::from_str("server: notif\n").unwrap();
        assert_eq!(spec.server, "notif");
        assert_eq!(spec.folder, "INBOX");
        assert!(spec.unseen_only);
        assert_eq!(spec.limit, 10);
        assert!(!spec.include_body);
        assert!(!spec.mark_seen);
    }

    #[test]
    fn mail_check_spec_deserializes_explicit_fields() {
        let spec: MailCheckSpec = serde_yaml::from_str(
            "server: notif\nfolder: Archive\nunseen_only: false\nlimit: 5\n\
             include_body: true\nmark_seen: true\n",
        )
        .unwrap();
        assert_eq!(spec.folder, "Archive");
        assert!(!spec.unseen_only);
        assert_eq!(spec.limit, 5);
        assert!(spec.include_body);
        assert!(spec.mark_seen);
    }

    #[test]
    fn db_exec_spec_deserializes_with_default_confirm() {
        let spec: DbExecSpec =
            serde_yaml::from_str("server: db1\nsql: UPDATE t SET x=1\nenv: /var/www/.env\n")
                .unwrap();
        assert_eq!(spec.server, "db1");
        assert_eq!(spec.sql, "UPDATE t SET x=1");
        assert!(!spec.confirm);
    }

    #[test]
    fn db_exec_spec_deserializes_explicit_confirm() {
        let spec: DbExecSpec = serde_yaml::from_str(
            "server: db1\nsql: DELETE FROM t WHERE id=1\nengine: mysql\nhost: 127.0.0.1\n\
             user: root\npassword: secret\nconfirm: true\n",
        )
        .unwrap();
        assert!(spec.confirm);
        assert_eq!(spec.engine.as_deref(), Some("mysql"));
    }

    #[test]
    fn resolve_mail_creds_prefers_explicit_over_profile() {
        let mut cfg = crate::config::Config::default();
        cfg.mail.insert(
            "notif".to_string(),
            crate::config::MailServer {
                host: "profile.example.com".to_string(),
                port: 465,
                user: "profileuser@example.com".to_string(),
                from: Some("profile-from@example.com".to_string()),
                tls: None,
                imap_host: None,
                imap_port: None,
            },
        );
        let ctx = Context::new(OutputFormat::Json, "default".to_string(), cfg);
        let creds = resolve_mail_creds(
            &ctx,
            Some("notif"),
            Some("explicit.example.com"),
            None,
            None,
            Some("secretpw"),
            None,
            None,
        )
        .unwrap();
        // Explicit `host` wins over the profile's.
        assert_eq!(creds.host, "explicit.example.com");
        // Unspecified fields fall back to the profile.
        assert_eq!(creds.user, "profileuser@example.com");
        assert_eq!(creds.from, "profile-from@example.com");
        assert_eq!(creds.port, 465);
        // Explicit password always wins (never touches the keychain).
        assert_eq!(creds.password, "secretpw");
        // tls inferred from the profile's port (465) since neither side set `tls`.
        assert_eq!(creds.tls, "tls");
    }

    #[test]
    fn resolve_mail_creds_infers_starttls_from_587_and_tls_from_465() {
        let ctx = Context::new(
            OutputFormat::Json,
            "default".to_string(),
            crate::config::Config::default(),
        );
        let creds_587 = resolve_mail_creds(
            &ctx,
            None,
            Some("h"),
            Some(587),
            Some("u"),
            Some("p"),
            None,
            None,
        )
        .unwrap();
        assert_eq!(creds_587.tls, "starttls");

        let creds_465 = resolve_mail_creds(
            &ctx,
            None,
            Some("h"),
            Some(465),
            Some("u"),
            Some("p"),
            None,
            None,
        )
        .unwrap();
        assert_eq!(creds_465.tls, "tls");
    }

    #[test]
    fn resolve_mail_creds_errors_clearly_with_no_creds() {
        let ctx = Context::new(
            OutputFormat::Json,
            "default".to_string(),
            crate::config::Config::default(),
        );
        let err = resolve_mail_creds(&ctx, None, None, None, None, None, None, None).unwrap_err();
        assert!(err.to_string().contains("host"));
    }

    #[test]
    fn resolve_mail_creds_errors_on_unknown_profile() {
        let ctx = Context::new(
            OutputFormat::Json,
            "default".to_string(),
            crate::config::Config::default(),
        );
        let err = resolve_mail_creds(&ctx, Some("ghost"), None, None, None, None, None, None)
            .unwrap_err();
        assert!(err.to_string().contains("No mail profile 'ghost'"));
    }

    #[test]
    fn resolve_imap_creds_errors_on_unknown_profile() {
        let ctx = Context::new(
            OutputFormat::Json,
            "default".to_string(),
            crate::config::Config::default(),
        );
        let err = resolve_imap_creds(&ctx, "ghost").unwrap_err();
        assert!(err.to_string().contains("No mail profile 'ghost'"));
    }

    #[test]
    fn resolve_imap_host_port_defaults_from_smtp_host_and_993() {
        let profile = crate::config::MailServer {
            host: "mail16.serv00.com".to_string(),
            port: 587,
            user: "notification@example.com".to_string(),
            from: None,
            tls: None,
            imap_host: None,
            imap_port: None,
        };
        let (host, port) = resolve_imap_host_port(&profile);
        assert_eq!(host.as_deref(), Some("mail16.serv00.com"));
        assert_eq!(port, 993);
    }

    #[test]
    fn resolve_imap_host_port_prefers_explicit_imap_fields() {
        let profile = crate::config::MailServer {
            host: "smtp.example.com".to_string(),
            port: 587,
            user: "notification@example.com".to_string(),
            from: None,
            tls: None,
            imap_host: Some("imap.example.com".to_string()),
            imap_port: Some(143),
        };
        let (host, port) = resolve_imap_host_port(&profile);
        assert_eq!(host.as_deref(), Some("imap.example.com"));
        assert_eq!(port, 143);
    }

    #[test]
    fn max_parallel_without_loop_is_rejected() {
        let ctx = Context::new(
            OutputFormat::Json,
            "default".to_string(),
            crate::config::Config::default(),
        );
        let env = dry_env(&ctx);
        let mut vars = HashMap::new();
        let mut include_stack = Vec::new();
        let task = Task {
            debug: Some("hi".to_string()),
            max_parallel: Some(4),
            ..Default::default()
        };
        let err = run_task(&task, &mut vars, &mut include_stack, &env).unwrap_err();
        assert!(
            err.to_string().contains("max_parallel:"),
            "error was: {err}"
        );
    }

    #[test]
    fn merge_repl_name_adds_a_name_when_absent() {
        let mut value: serde_yaml::Value = serde_yaml::from_str("run: echo hi").unwrap();
        merge_repl_name(&mut value, "repl-1");
        assert_eq!(value["name"].as_str(), Some("repl-1"));
        assert_eq!(value["run"].as_str(), Some("echo hi"));
    }

    #[test]
    fn merge_repl_name_leaves_an_explicit_name_alone() {
        let mut value: serde_yaml::Value =
            serde_yaml::from_str("name: my task\nrun: echo hi\n").unwrap();
        merge_repl_name(&mut value, "repl-1");
        assert_eq!(value["name"].as_str(), Some("my task"));
    }

    #[test]
    fn set_fact_stores_rendered_values_into_vars() {
        let ctx = Context::new(
            OutputFormat::Json,
            "default".to_string(),
            crate::config::Config::default(),
        );
        let env = dry_env(&ctx);
        let mut vars = vars(&[("name", "world")]);
        let mut include_stack = Vec::new();
        let mut facts = HashMap::new();
        facts.insert("greeting".to_string(), "hello {{name}}".to_string());
        let task = Task {
            set_fact: Some(facts),
            ..Default::default()
        };
        assert!(run_task_once(&task, &mut vars, &mut include_stack, &env).is_ok());
        assert_eq!(
            vars.get("greeting").map(String::as_str),
            Some("hello world")
        );
    }

    #[test]
    fn set_fact_rejects_register() {
        let ctx = Context::new(
            OutputFormat::Json,
            "default".to_string(),
            crate::config::Config::default(),
        );
        let env = dry_env(&ctx);
        let mut vars = HashMap::new();
        let mut include_stack = Vec::new();
        let task = Task {
            register: Some("x".to_string()),
            set_fact: Some(HashMap::new()),
            ..Default::default()
        };
        assert!(run_task_once(&task, &mut vars, &mut include_stack, &env).is_err());
    }

    #[test]
    fn render_leaves_unknown_and_unresolvable_tokens_literal() {
        let v = vars(&[]);
        assert_eq!(render("{{nope}}", &v), "{{nope}}");
        // A profile/key that was never stored resolves to None the same way an unknown
        // plain var does — never crashes, just leaves the token as-is.
        assert_eq!(
            render("{{secret.nonexistent_profile.nonexistent_key}}", &v),
            "{{secret.nonexistent_profile.nonexistent_key}}"
        );
    }

    #[test]
    fn validate_handlers_rejects_unknown_notify_target() {
        let playbook = Playbook {
            name: "t".to_string(),
            description: None,
            vars_files: Vec::new(),
            vars: HashMap::new(),
            handlers: vec![],
            tasks: vec![Task {
                name: "t1".to_string(),
                notify: vec!["nonexistent_handler".to_string()],
                ..Default::default()
            }],
        };
        assert!(validate_handlers(&playbook).is_err());
    }

    #[test]
    fn validate_handlers_accepts_a_matching_notify_target() {
        let playbook = Playbook {
            name: "t".to_string(),
            description: None,
            vars_files: Vec::new(),
            vars: HashMap::new(),
            handlers: vec![Task {
                name: "restart".to_string(),
                ..Default::default()
            }],
            tasks: vec![Task {
                name: "t1".to_string(),
                notify: vec!["restart".to_string()],
                ..Default::default()
            }],
        };
        assert!(validate_handlers(&playbook).is_ok());
    }

    #[test]
    fn parse_field_selector_splits_on_last_at() {
        assert_eq!(parse_field_selector("a.title"), ("a.title", None));
        assert_eq!(
            parse_field_selector("a.title@href"),
            ("a.title", Some("href"))
        );
        // No plain-CSS attribute selector like `[data-x]` starts or ends with '@', so an
        // empty side just falls back to "no attribute" rather than misparsing.
        assert_eq!(parse_field_selector("@href"), ("@href", None));
        assert_eq!(parse_field_selector("a.title@"), ("a.title@", None));
    }

    #[test]
    fn resolve_loop_items_dynamic_parses_json_array_of_objects() {
        let v = vars(&[(
            "jobs",
            r#"[{"title":"Dev","company":"Acme"},{"title":"Lead","company":"Beta"}]"#,
        )]);
        let spec = LoopSpec::Dynamic {
            from: "{{jobs}}".to_string(),
            split: None,
        };
        let items = resolve_loop_items(&spec, &v);
        assert_eq!(items.len(), 2);
        let LoopItem::Map(m) = &items[0] else {
            panic!("expected a map item");
        };
        assert_eq!(m.get("title"), Some(&"Dev".to_string()));
        assert_eq!(m.get("company"), Some(&"Acme".to_string()));
    }

    #[test]
    fn resolve_loop_items_dynamic_falls_back_to_text_split() {
        let v = vars(&[("names", "alice\nbob\n\ncarol")]);
        let spec = LoopSpec::Dynamic {
            from: "{{names}}".to_string(),
            split: None,
        };
        let items = resolve_loop_items(&spec, &v);
        assert_eq!(items.len(), 3);
        assert!(matches!(&items[0], LoopItem::Scalar(s) if s == "alice"));
        assert!(matches!(&items[2], LoopItem::Scalar(s) if s == "carol"));
    }

    #[test]
    fn resolve_loop_items_dynamic_respects_custom_split() {
        let v = vars(&[("names", "alice,bob,carol")]);
        let spec = LoopSpec::Dynamic {
            from: "{{names}}".to_string(),
            split: Some(",".to_string()),
        };
        let items = resolve_loop_items(&spec, &v);
        assert_eq!(items.len(), 3);
        assert!(matches!(&items[1], LoopItem::Scalar(s) if s == "bob"));
    }

    #[test]
    fn scrape_spec_deserializes() {
        let spec: ScrapeSpec = serde_yaml::from_str(
            "url: https://example.com\neach: .row\nfields:\n  title: .title\n  link: a@href\n",
        )
        .unwrap();
        assert_eq!(spec.url, "https://example.com");
        assert_eq!(spec.each.as_deref(), Some(".row"));
        assert_eq!(spec.fields.get("link").map(String::as_str), Some("a@href"));
    }

    #[test]
    fn loop_item_deserializes_scalar_list() {
        let items: Vec<LoopItem> = serde_yaml::from_str("[a, b, c]").unwrap();
        assert!(matches!(&items[0], LoopItem::Scalar(s) if s == "a"));
        assert!(matches!(&items[2], LoopItem::Scalar(s) if s == "c"));
    }

    #[test]
    fn loop_item_deserializes_map_list() {
        let items: Vec<LoopItem> =
            serde_yaml::from_str("- name: a\n  port: \"1\"\n- name: b\n  port: \"2\"\n").unwrap();
        let LoopItem::Map(m) = &items[0] else {
            panic!("expected a map item");
        };
        assert_eq!(m.get("name"), Some(&"a".to_string()));
        assert_eq!(m.get("port"), Some(&"1".to_string()));
    }

    #[test]
    fn confirm_dry_run_is_a_noop() {
        let ctx = Context::new(
            OutputFormat::Json,
            "default".to_string(),
            crate::config::Config::default(),
        );
        let env = dry_env(&ctx); // dry: true
        let mut vars = HashMap::new();
        let mut include_stack = Vec::new();
        let task = Task {
            confirm: Some("proceed?".to_string()),
            ..Default::default()
        };
        assert!(run_task_once(&task, &mut vars, &mut include_stack, &env).is_ok());
    }

    #[test]
    fn confirm_bails_when_quiet_and_not_auto_yes() {
        let ctx = Context::new(
            OutputFormat::Json,
            "default".to_string(),
            crate::config::Config::default(),
        );
        // quiet: true (inherited from dry_env), dry: false — the non-interactive/agent
        // path with no --yes must fail fast instead of blocking on stdin.
        let env = RunEnv {
            dry: false,
            ..dry_env(&ctx)
        };
        let mut vars = HashMap::new();
        let mut include_stack = Vec::new();
        let task = Task {
            confirm: Some("proceed?".to_string()),
            ..Default::default()
        };
        let err = run_task_once(&task, &mut vars, &mut include_stack, &env).unwrap_err();
        assert!(err.to_string().contains("--yes"), "error was: {err}");
    }

    #[test]
    fn confirm_succeeds_with_auto_yes_without_prompting() {
        let ctx = Context::new(
            OutputFormat::Json,
            "default".to_string(),
            crate::config::Config::default(),
        );
        let env = RunEnv {
            dry: false,
            auto_yes: true,
            ..dry_env(&ctx)
        };
        let mut vars = HashMap::new();
        let mut include_stack = Vec::new();
        let task = Task {
            confirm: Some("proceed?".to_string()),
            ..Default::default()
        };
        assert!(run_task_once(&task, &mut vars, &mut include_stack, &env).is_ok());
    }

    #[test]
    fn confirm_rejects_register() {
        let ctx = Context::new(
            OutputFormat::Json,
            "default".to_string(),
            crate::config::Config::default(),
        );
        let env = dry_env(&ctx);
        let mut vars = HashMap::new();
        let mut include_stack = Vec::new();
        let task = Task {
            register: Some("x".to_string()),
            confirm: Some("proceed?".to_string()),
            ..Default::default()
        };
        assert!(run_task_once(&task, &mut vars, &mut include_stack, &env).is_err());
    }

    #[test]
    fn report_spec_deserializes_with_default_title() {
        let spec: ReportSpec = serde_yaml::from_str(
            "format: html\nsources:\n  data: \"{{resp}}\"\nout: report.html\n",
        )
        .unwrap();
        assert_eq!(spec.format, "html");
        assert_eq!(spec.title, "Tooler Report");
        assert_eq!(
            spec.sources.get("data").map(String::as_str),
            Some("{{resp}}")
        );
        assert_eq!(spec.out, "report.html");
    }

    #[test]
    fn report_rejects_an_unknown_format() {
        let ctx = Context::new(
            OutputFormat::Json,
            "default".to_string(),
            crate::config::Config::default(),
        );
        let env = dry_env(&ctx);
        let mut vars = HashMap::new();
        let mut include_stack = Vec::new();
        let task = Task {
            report: Some(ReportSpec {
                format: "csv".to_string(),
                title: default_report_title(),
                sources: HashMap::new(),
                out: "out.csv".to_string(),
            }),
            ..Default::default()
        };
        let err = run_task_once(&task, &mut vars, &mut include_stack, &env).unwrap_err();
        assert!(
            err.to_string().contains("html/pdf/excel"),
            "error was: {err}"
        );
    }

    #[test]
    fn report_accepts_a_known_format_case_insensitively() {
        let ctx = Context::new(
            OutputFormat::Json,
            "default".to_string(),
            crate::config::Config::default(),
        );
        let env = dry_env(&ctx); // dry: true — validates format without writing a file
        let mut vars = HashMap::new();
        let mut include_stack = Vec::new();
        let task = Task {
            report: Some(ReportSpec {
                format: "HTML".to_string(),
                title: default_report_title(),
                sources: HashMap::new(),
                out: "out.html".to_string(),
            }),
            ..Default::default()
        };
        assert!(run_task_once(&task, &mut vars, &mut include_stack, &env).is_ok());
    }

    #[test]
    fn wait_for_requires_exactly_one_check() {
        let ctx = Context::new(
            OutputFormat::Json,
            "default".to_string(),
            crate::config::Config::default(),
        );
        let env = dry_env(&ctx);
        let mut vars = HashMap::new();
        let mut include_stack = Vec::new();

        // Zero checks set.
        let task = Task {
            wait_for: Some(WaitForSpec::default()),
            ..Default::default()
        };
        assert!(run_task_once(&task, &mut vars, &mut include_stack, &env).is_err());

        // Exactly one — accepted (dry: true, so no real polling happens).
        let task = Task {
            wait_for: Some(WaitForSpec {
                check_url: Some("http://example.com".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(run_task_once(&task, &mut vars, &mut include_stack, &env).is_ok());

        // Two checks set at once.
        let task = Task {
            wait_for: Some(WaitForSpec {
                check_url: Some("http://example.com".to_string()),
                check_port: Some(CheckPortSpec {
                    host: "example.com".to_string(),
                    port: 80,
                    timeout: 5,
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(run_task_once(&task, &mut vars, &mut include_stack, &env).is_err());

        // file_exists alone — accepted.
        let task = Task {
            wait_for: Some(WaitForSpec {
                file_exists: Some("flag.txt".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(run_task_once(&task, &mut vars, &mut include_stack, &env).is_ok());

        // file_exists and file_absent set together.
        let task = Task {
            wait_for: Some(WaitForSpec {
                file_exists: Some("flag.txt".to_string()),
                file_absent: Some("lock.txt".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(run_task_once(&task, &mut vars, &mut include_stack, &env).is_err());
    }

    #[test]
    fn wait_for_rejects_register() {
        let ctx = Context::new(
            OutputFormat::Json,
            "default".to_string(),
            crate::config::Config::default(),
        );
        let env = dry_env(&ctx);
        let mut vars = HashMap::new();
        let mut include_stack = Vec::new();
        let task = Task {
            register: Some("x".to_string()),
            wait_for: Some(WaitForSpec {
                check_url: Some("http://example.com".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(run_task_once(&task, &mut vars, &mut include_stack, &env).is_err());
    }

    #[test]
    fn timeout_without_run_is_rejected() {
        let ctx = Context::new(
            OutputFormat::Json,
            "default".to_string(),
            crate::config::Config::default(),
        );
        let env = dry_env(&ctx);
        let mut vars = HashMap::new();
        let mut include_stack = Vec::new();
        let task = Task {
            timeout: Some(5),
            check_url: Some("http://example.com".to_string()),
            ..Default::default()
        };
        assert!(run_task_once(&task, &mut vars, &mut include_stack, &env).is_err());
    }

    #[test]
    fn timeout_with_run_is_accepted() {
        let ctx = Context::new(
            OutputFormat::Json,
            "default".to_string(),
            crate::config::Config::default(),
        );
        // dry: true — only the upfront validation runs, no real subprocess/timeout.
        let env = dry_env(&ctx);
        let mut vars = HashMap::new();
        let mut include_stack = Vec::new();
        let task = Task {
            timeout: Some(5),
            run: Some("echo hi".to_string()),
            ..Default::default()
        };
        assert!(run_task_once(&task, &mut vars, &mut include_stack, &env).is_ok());
    }

    #[test]
    fn ensure_trailing_slash_appends_when_missing_and_is_idempotent() {
        assert_eq!(ensure_trailing_slash("/a/b"), "/a/b/");
        assert_eq!(ensure_trailing_slash("/a/b/"), "/a/b/");
    }

    #[test]
    fn join_confined_allows_plain_relative_paths() {
        let base = Path::new("/pb/dir");
        assert_eq!(
            join_confined(base, "out.txt").unwrap(),
            PathBuf::from("/pb/dir/out.txt")
        );
        assert_eq!(
            join_confined(base, "sub/out.txt").unwrap(),
            PathBuf::from("/pb/dir/sub/out.txt")
        );
    }

    #[test]
    fn join_confined_allows_a_dotdot_that_nets_back_inside_base() {
        let base = Path::new("/pb/dir");
        // Wanders outside and back, but never nets below `base`.
        assert_eq!(
            join_confined(base, "a/../b").unwrap(),
            PathBuf::from("/pb/dir/b")
        );
    }

    #[test]
    fn join_confined_rejects_absolute_paths() {
        let base = Path::new("/pb/dir");
        assert!(join_confined(base, "/etc/passwd").is_err());
    }

    #[test]
    fn join_confined_rejects_dotdot_that_escapes_base() {
        let base = Path::new("/pb/dir");
        assert!(join_confined(base, "../outside.txt").is_err());
        assert!(join_confined(base, "a/../../outside.txt").is_err());
    }

    #[test]
    fn data_path_for_uses_a_different_suffix_than_state_path_for() {
        let file = Path::new("/pb/dir/playbook.yml");
        let data_path = data_path_for(file);
        let state_path = state_path_for(file);
        assert_eq!(data_path, PathBuf::from("/pb/dir/playbook.yml.data.json"));
        assert_ne!(data_path, state_path);
    }

    #[test]
    fn load_persisted_state_returns_an_empty_map_for_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.data.json");
        let state = load_persisted_state(&missing).unwrap();
        assert!(state.is_empty());
    }

    #[test]
    fn load_persisted_state_reads_a_real_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("playbook.yml.data.json");
        std::fs::write(&path, r#"{"last_uid": "42"}"#).unwrap();
        let state = load_persisted_state(&path).unwrap();
        assert_eq!(state.get("last_uid"), Some(&"42".to_string()));
    }

    #[test]
    fn db_sync_spec_deserializes_env_and_explicit_sides() {
        let spec: DbSyncSpec = serde_yaml::from_str(
            "server: serv00\n\
             from:\n  env: backend_prod/.env\n\
             to:\n  engine: mysql\n  host: localhost\n  database: dev_db\n  user: root\n",
        )
        .unwrap();
        assert_eq!(spec.server, "serv00");
        assert_eq!(spec.from.env.as_deref(), Some("backend_prod/.env"));
        assert_eq!(spec.to.database.as_deref(), Some("dev_db"));
        assert_eq!(spec.to.engine.as_deref(), Some("mysql"));
    }

    #[test]
    fn sync_files_spec_deserializes() {
        let spec: SyncFilesSpec = serde_yaml::from_str(
            "server: serv00\nfrom: /a/prod/storage\nto: /a/dev/storage\ndelete: true\n",
        )
        .unwrap();
        assert_eq!(spec.from, "/a/prod/storage");
        assert!(spec.delete);
    }

    #[test]
    fn sync_db_and_sync_files_support_register() {
        let ctx = Context::new(
            OutputFormat::Json,
            "default".to_string(),
            crate::config::Config::default(),
        );
        // dry: true — only the upfront register-validation guard runs, no real SSH.
        let env = dry_env(&ctx);
        let mut vars = HashMap::new();
        let mut include_stack = Vec::new();

        let task = Task {
            register: Some("x".to_string()),
            sync_db: Some(DbSyncSpec {
                server: "serv00".to_string(),
                from: DbSyncSide::default(),
                to: DbSyncSide::default(),
            }),
            ..Default::default()
        };
        assert!(run_task_once(&task, &mut vars, &mut include_stack, &env).is_ok());

        let task = Task {
            register: Some("x".to_string()),
            sync_files: Some(SyncFilesSpec {
                server: "serv00".to_string(),
                from: "/a".to_string(),
                to: "/b".to_string(),
                delete: false,
            }),
            ..Default::default()
        };
        assert!(run_task_once(&task, &mut vars, &mut include_stack, &env).is_ok());
    }

    #[test]
    fn sync_files_rejects_timeout() {
        let ctx = Context::new(
            OutputFormat::Json,
            "default".to_string(),
            crate::config::Config::default(),
        );
        let env = dry_env(&ctx);
        let mut vars = HashMap::new();
        let mut include_stack = Vec::new();
        let task = Task {
            timeout: Some(5),
            sync_files: Some(SyncFilesSpec {
                server: "serv00".to_string(),
                from: "/a".to_string(),
                to: "/b".to_string(),
                delete: false,
            }),
            ..Default::default()
        };
        assert!(run_task_once(&task, &mut vars, &mut include_stack, &env).is_err());
    }

    #[test]
    fn include_spec_deserializes_both_shapes() {
        let simple: IncludeSpec = serde_yaml::from_str("sub.yml").unwrap();
        assert_eq!(simple.file(), "sub.yml");
        assert!(simple.vars().is_empty());

        let with_vars: IncludeSpec =
            serde_yaml::from_str("file: sub.yml\nvars:\n  service: api\n").unwrap();
        assert_eq!(with_vars.file(), "sub.yml");
        assert_eq!(
            with_vars.vars().get("service").map(String::as_str),
            Some("api")
        );
    }

    #[test]
    fn load_playbook_vars_with_no_vars_files_passes_inline_vars_through() {
        let playbook = Playbook {
            name: "t".to_string(),
            description: None,
            vars_files: Vec::new(),
            vars: vars(&[("host", "localhost")]),
            handlers: vec![],
            tasks: vec![],
        };
        let merged = load_playbook_vars(&playbook, Path::new(".")).unwrap();
        assert_eq!(merged.get("host").map(String::as_str), Some("localhost"));
    }

    #[test]
    fn start_at_task_rejects_an_unknown_task_name() {
        let ctx = Context::new(
            OutputFormat::Json,
            "default".to_string(),
            crate::config::Config::default(),
        );
        let env = RunEnv {
            start_at: Some("nonexistent".to_string()),
            ..dry_env(&ctx)
        };
        let playbook = Playbook {
            name: "t".to_string(),
            description: None,
            vars_files: Vec::new(),
            vars: HashMap::new(),
            handlers: vec![],
            tasks: vec![Task {
                name: "only task".to_string(),
                debug: Some("hi".to_string()),
                ..Default::default()
            }],
        };
        let mut vars = HashMap::new();
        let mut include_stack = Vec::new();
        let err = execute_playbook(
            &playbook,
            &None,
            &None,
            &None,
            &mut vars,
            &mut include_stack,
            true,
            &env,
        )
        .unwrap_err();
        assert!(err.to_string().contains("nonexistent"), "error was: {err}");
    }
}
