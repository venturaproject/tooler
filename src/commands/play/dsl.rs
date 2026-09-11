//! The playbook DSL itself: `Playbook`, `Task`, and every task action's `*Spec` type,
//! the `RequiresConfirm`/`Confirmed<T>` compile-time confirm-gate, `loop:`'s item/spec
//! resolution, `mcp_tool:`/`params:` (playbook-as-MCP-tool), and the path-confinement
//! helpers (`ConfinedPath`/`join_confined`/`is_literal_path`) every local-file action
//! uses. Pure data shape and parsing — no I/O of its own beyond `playbook_tool_defs`'
//! directory scan and the OS keychain reads it needs for a `confirm:` schema property.
use super::*;
use anyhow::{Result, bail};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

// ── YAML schema ───────────────────────────────────────────────────────────────

/// The playbook DSL's root. Every field here and on `Task`/its nested `*Spec` structs
/// is dumped as a formal JSON Schema document by `tooler play --schema` (see `run()`).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Playbook {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) description: Option<String>,
    /// External var files (paths relative to this playbook's own directory), each a flat
    /// `key: value` YAML map — same shape as `vars:`, no new format. Loaded in order
    /// (a later file overrides an earlier one); `vars:` then overrides all of them; a CLI
    /// `--var` overrides everything. See `run()`.
    #[serde(default)]
    pub(crate) vars_files: Vec<String>,
    #[serde(default)]
    pub(crate) vars: HashMap<String, String>,
    pub(crate) tasks: Vec<Task>,
    /// Tasks triggered by `notify:`, run at most once each after all regular tasks
    /// succeed, in first-notified order. Matched by `name` — see `validate_handlers`.
    #[serde(default)]
    pub(crate) handlers: Vec<Task>,
    /// Tasks run when this playbook's task loop hits a non-ignored failure, right before
    /// it reports and bails — a playbook-level counterpart to `block:`'s `rescue:`, for
    /// "if anything fails, send an alert / hit a webhook / record it in `state_set:`".
    /// Best-effort: a failing `on_failure:` task is logged and the remaining ones still
    /// run — it never re-triggers itself and never changes the original failure (the run
    /// still exits non-zero). Not fired by a handler failure or in `--dry`. Two vars are
    /// set for these tasks: `{{failed_task}}` and `{{failure_reason}}`. See
    /// `run_failure_hook`.
    #[serde(default)]
    pub(crate) on_failure: Vec<Task>,
    /// Opt this playbook in as its own first-class MCP tool: `tooler mcp` (started from
    /// the project root) then exposes `tooler_pb_<name>` alongside the generic
    /// `tooler_play`, so an agent calls a proven process by name with typed arguments
    /// instead of hand-writing YAML. See `McpToolSpec` / `params:` / `playbook_tool_defs`.
    #[serde(default)]
    pub(crate) mcp_tool: Option<McpToolSpec>,
    /// Typed parameters — the input schema for `mcp_tool:` (and a place to document a
    /// playbook's `--var` inputs even without it). Each becomes a `{{var}}` at run time;
    /// one with a `default:` also seeds `vars:` so `tooler play <pb> --var k=v` works
    /// from the plain CLI. See `ParamSpec`.
    #[serde(default)]
    pub(crate) params: HashMap<String, ParamSpec>,
    /// When true, a real (non-dry) top-level run of this playbook file takes an exclusive
    /// lock (`<file>.lock`) for its duration. A second `tooler play` of the same file
    /// while that lock is held and still fresh exits non-zero with a clear message
    /// instead of racing the first run's `state_set:` writes — the guard a
    /// `tooler cron local`-scheduled playbook needs when a run occasionally overruns its
    /// interval. A lock older than `lock_timeout` seconds is assumed abandoned (the
    /// previous run was killed) and taken over with a warning. Ignored in `--dry` and for
    /// `include:`d sub-playbooks (only the top-level run locks). See `run()`/`LockGuard`.
    #[serde(default)]
    pub(crate) single_instance: bool,
    /// Seconds after which a held `<file>.lock` is treated as stale and taken over —
    /// only meaningful with `single_instance: true`. Default 21600 (6h).
    #[serde(default = "default_lock_timeout")]
    pub(crate) lock_timeout: u64,
}

pub(crate) fn default_lock_timeout() -> u64 {
    21600
}

/// `mcp_tool:` — see `Playbook::mcp_tool`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct McpToolSpec {
    /// The tool is exposed as `tooler_pb_<name>`. Defaults to a slug of the playbook's
    /// file name.
    #[serde(default)]
    pub(crate) name: Option<String>,
    /// One-line description shown to the calling agent.
    pub(crate) description: String,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ParamType {
    #[default]
    String,
    Number,
    Boolean,
}

/// One `params:` entry — see `Playbook::params`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ParamSpec {
    #[serde(default, rename = "type")]
    pub(crate) param_type: ParamType,
    #[serde(default)]
    pub(crate) description: Option<String>,
    #[serde(default)]
    pub(crate) required: bool,
    #[serde(default, rename = "enum")]
    pub(crate) enum_values: Option<Vec<String>>,
    /// Default value — seeds `vars:` so the playbook also runs from the plain CLI.
    #[serde(default)]
    pub(crate) default: Option<serde_json::Value>,
}

impl ParamSpec {
    /// The `default:` rendered as the string a `{{var}}` would see (a scalar as-is,
    /// anything else as its JSON text). `None` when there's no default.
    pub(crate) fn default_as_string(&self) -> Option<String> {
        self.default.as_ref().map(json_cell_to_string)
    }
}

/// A playbook that opted in as an MCP tool, reduced to what `tooler mcp` needs to
/// register it — see `playbook_tool_defs`.
pub(crate) struct PlaybookToolDef {
    /// The full MCP tool name, `tooler_pb_<slug>`.
    pub name: String,
    pub description: String,
    pub file: PathBuf,
    /// A JSON Schema object for the tool's arguments.
    pub input_schema: serde_json::Value,
}

/// Scans `playbooks_dir` for `*.yml`/`*.yaml` declaring `mcp_tool:` and builds one
/// `PlaybookToolDef` each. Best-effort: an unparseable playbook is skipped. Called once
/// at `tooler mcp` startup.
pub(crate) fn playbook_tool_defs(playbooks_dir: &Path) -> Vec<PlaybookToolDef> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(playbooks_dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if ext != "yml" && ext != "yaml" {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(playbook) = serde_yaml::from_str::<Playbook>(&content) else {
            continue;
        };
        let Some(mcp) = &playbook.mcp_tool else {
            continue;
        };
        let slug = mcp.name.clone().unwrap_or_else(|| {
            path.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("playbook")
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                .collect()
        });

        let mut properties = serde_json::Map::new();
        let mut required = Vec::new();
        for (pname, p) in &playbook.params {
            let mut schema = serde_json::Map::new();
            schema.insert(
                "type".into(),
                match p.param_type {
                    ParamType::String => "string",
                    ParamType::Number => "number",
                    ParamType::Boolean => "boolean",
                }
                .into(),
            );
            if let Some(d) = &p.description {
                schema.insert("description".into(), d.clone().into());
            }
            if let Some(e) = &p.enum_values {
                schema.insert("enum".into(), serde_json::json!(e));
            }
            properties.insert(pname.clone(), serde_json::Value::Object(schema));
            if p.required {
                required.push(pname.clone());
            }
        }
        if playbook_has_confirm_gated(&playbook) {
            properties.insert(
                "confirm".into(),
                serde_json::json!({
                    "type": "boolean",
                    "description": "Set true to allow this playbook's confirm-gated actions (db_exec:/fs_write:/deploy:/…) to run.",
                }),
            );
        }

        out.push(PlaybookToolDef {
            name: format!("tooler_pb_{slug}"),
            description: mcp.description.clone(),
            file: path.clone(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": properties,
                "required": required,
            }),
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Whether any task in `pb` (including nested `block:`/`rescue:`/`always:`/`parallel:`)
/// is a `confirm:`-gated destructive action — see `confirm_gate`.
pub(crate) fn playbook_has_confirm_gated(pb: &Playbook) -> bool {
    fn walk(tasks: &[Task]) -> bool {
        tasks.iter().any(|t| {
            confirm_gate(t).is_some()
                || [&t.block, &t.rescue, &t.always, &t.parallel]
                    .into_iter()
                    .flatten()
                    .any(|b| walk(b))
        })
    }
    walk(&pb.tasks) || walk(&pb.on_failure)
}

#[derive(Debug, Deserialize, Default, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Task {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) ignore_errors: bool,
    #[serde(default)]
    pub(crate) tags: Vec<String>,
    /// Simple condition evaluated once against the playbook's vars, before any `loop:`
    /// expansion (does not see `{{item}}`). Supports "<a> == <b>", "<a> != <b>", or a
    /// bare truthy check after `{{var}}` substitution — not a full expression language.
    #[serde(default)]
    pub(crate) when: Option<String>,
    /// Run this task once per item. A scalar item is available as `{{item}}`; a map
    /// item exposes `{{item.<field>}}` per key (bare `{{item}}` stays literal for a map
    /// item). The first failing iteration fails the task (and, unless `ignore_errors`,
    /// the whole playbook) — remaining items are not attempted. Either a static YAML list
    /// (`loop: [a, b, c]`) or a dynamic source resolved at runtime from a var (typically a
    /// `register:`ed `http:`/`scrape:` result) — see `LoopSpec`.
    #[serde(default, rename = "loop")]
    pub(crate) loop_spec: Option<LoopSpec>,
    /// Capture this task's output into a variable, usable by later tasks via
    /// `{{name}}`. Supported on run/ssh/fleet only (see `run_task_once`). Inside a
    /// `loop:`, `<name>` still holds only the last iteration's value, but `<name>.results`
    /// is also set to a JSON array of every iteration's value in order — see
    /// `run_task`/`run_loop_parallel`.
    #[serde(default)]
    pub(crate) register: Option<String>,
    /// Retry this task up to N times (total attempts = retries + 1) before giving up.
    /// Applies per `loop:` iteration if combined with `loop:`. Ignored in `--dry`.
    #[serde(default)]
    pub(crate) retries: Option<u32>,
    /// Seconds to wait between retry attempts (default 1 if `retries:` is set).
    #[serde(default)]
    pub(crate) delay: Option<u64>,
    /// Retry this task (same syntax as `when:`) until this condition on its current vars
    /// (typically its own `register:`ed value) is true, or `retries:` attempts are
    /// exhausted — unlike plain `retries:`, which only retries on *failure*, `until:`
    /// also retries a *successful* task whose result doesn't satisfy the condition yet
    /// (e.g. polling a `run:`/`http:` result for "ready"). Only meaningful combined with
    /// `retries:` — without it, it's checked once, same as an `assert:` right after the
    /// task. Ignored in `--dry`, same as `retries:` (a dry run never really registers a
    /// value to check).
    #[serde(default)]
    pub(crate) until: Option<String>,
    /// Handler names (matching an entry in the playbook's `handlers:`) to trigger when
    /// this task succeeds and is considered "changed" (see `changed_when`). Deduplicated
    /// and run at most once each, after all regular tasks succeed.
    #[serde(default)]
    pub(crate) notify: Vec<String>,
    /// Condition (same syntax as `when:`) deciding whether this task's success counts as
    /// "changed" for `notify:` purposes. Absent means always changed on success —
    /// matches how a plain shell command has no built-in idempotency signal.
    #[serde(default)]
    pub(crate) changed_when: Option<String>,
    /// Condition (same syntax as `when:`) that overrides a task's outcome to failed even
    /// though its exit code says otherwise — e.g. a `run:` that always exits 0 but whose
    /// `register:`ed output contains an error marker. Evaluated independently of
    /// `changed_when:` (a task can be both "changed" and "failed"); checked before
    /// `until:`, so a `failed_when:`-triggered failure is retried by `retries:`/`delay:`
    /// like any other failure, not treated as "succeeded but not yet satisfied." Ignored
    /// in `--dry`, same as `until:` — a dry run never really registers a value to check.
    #[serde(default)]
    pub(crate) failed_when: Option<String>,

    // Actions — only one should be set per task
    /// A shell command — either a bare string, or `{command: "...", env: {...}}` to also
    /// inject extra environment variables into the subprocess. See `RunSpec`.
    pub(crate) run: Option<RunSpec>,
    pub(crate) check_url: Option<String>,
    pub(crate) check_port: Option<CheckPortSpec>,
    /// Make an HTTP request. `register:` (if set) captures two vars: `<reg>` = the
    /// response body text, `<reg>.status` = the status code as a string — the same
    /// dotted-key convention `loop:`'s map items already use for `item.<field>`. See
    /// `HttpSpec`; combine with the `| json:<path>` render filter to pull a field out of
    /// a JSON response, e.g. `{{resp | json:data.id}}`.
    pub(crate) http: Option<HttpSpec>,
    /// Scrape a page with CSS selectors. `register:` (if set) captures a JSON array of
    /// `fields` objects, one per `each:` match — directly loopable via a dynamic
    /// `loop: {from: "{{reg}}"}`. See `ScrapeSpec`.
    pub(crate) scrape: Option<ScrapeSpec>,
    /// Poll a check until it succeeds or times out — see `WaitForSpec`. Exactly one of
    /// `check_url`/`check_port`/`ssh` must be set within it (validated upfront).
    pub(crate) wait_for: Option<WaitForSpec>,
    /// Generate a PDF/Excel/HTML report from inline data — see `ReportSpec`. `register:`
    /// (if set) captures the output file's byte size, same convention as `sync_db:`.
    pub(crate) report: Option<ReportSpec>,
    pub(crate) env_check: Option<EnvCheckSpec>,
    pub(crate) ssh: Option<SshSpec>,
    pub(crate) fleet: Option<FleetSpec>,
    /// Read a remote file over SSH — the same `commands::fs::cat_cmd` `tooler fs cat`
    /// uses. See `FsCatSpec`.
    pub(crate) fs_cat: Option<FsCatSpec>,
    /// Overwrite a remote file over SSH — the same `commands::fs::write_cmd` `tooler fs
    /// write` uses. See `FsWriteSpec`.
    pub(crate) fs_write: Option<FsWriteSpec>,
    /// Restart a remote systemd unit — the same `commands::systemd::restart_cmd` `tooler
    /// systemd restart` uses. See `SystemdRestartSpec`.
    pub(crate) systemd_restart: Option<SystemdRestartSpec>,
    /// Check a remote systemd unit's status — the same `commands::systemd::status_cmd`
    /// `tooler systemd status` uses. See `SystemdStatusSpec`.
    pub(crate) systemd_status: Option<SystemdStatusSpec>,
    /// Tail a remote file over SSH — the same `commands::logs::tail_cmd` `tooler logs
    /// tail` uses. See `LogsTailSpec`.
    pub(crate) logs_tail: Option<LogsTailSpec>,
    /// Search a remote file over SSH — the same `commands::logs::grep_cmd` `tooler logs
    /// grep` uses. See `LogsGrepSpec`.
    pub(crate) logs_grep: Option<LogsGrepSpec>,
    /// List remote processes over SSH — the same `commands::ps::parse_ps_aux`/
    /// `apply_filter` `tooler ps list` uses. See `PsListSpec`.
    pub(crate) ps_list: Option<PsListSpec>,
    /// Send a signal to a remote process over SSH — the same `commands::ps::kill_cmd`
    /// `tooler ps kill` uses. See `PsKillSpec`.
    pub(crate) ps_kill: Option<PsKillSpec>,
    /// A remote server's uptime/memory/disk snapshot — the same `commands::stat`
    /// engine `tooler stat` uses. See `StatSpec`.
    pub(crate) stat: Option<StatSpec>,
    /// Run another whole playbook (by bare playbooks/ name, or a path relative to this
    /// playbook's own directory) as a single task — either bare (`include: sub.yml`) or
    /// with per-call var overrides (`include: {file: sub.yml, vars: {...}}`). See
    /// `IncludeSpec`, `resolve_include_path`.
    pub(crate) include: Option<IncludeSpec>,
    /// Fails the task immediately (not skips) unless the condition (same syntax as
    /// `when:`) holds — either a bare condition string, or `{that: [...], msg: "..."}`
    /// to check several conditions in one task with a custom failure message. See
    /// `AssertSpec`.
    pub(crate) assert: Option<AssertSpec>,
    /// Run these tasks in order as a single unit; see `rescue`/`always`. Counts as one
    /// outcome in the parent's recap — its own tasks aren't flattened into the parent's
    /// totals (same scope line as `include:`).
    pub(crate) block: Option<Vec<Task>>,
    /// Run only if `block:` failed; if these succeed, the block is considered recovered.
    pub(crate) rescue: Option<Vec<Task>>,
    /// Always run after `block:`/`rescue:`, regardless of outcome; a failure here fails
    /// the block even after a successful rescue.
    pub(crate) always: Option<Vec<Task>>,
    /// Run these tasks concurrently (each its own thread with a cloned vars/include_stack,
    /// the same model as a `max_parallel:` `loop:`), joining before the next task —
    /// e.g. extract from several sources at once. `max_parallel:` on this same task caps
    /// how many run at a time (default: all). Each child's `register:`ed var is merged
    /// back into the parent afterwards, in child order (deterministic despite the
    /// concurrency); the first child that fails in that order fails the `parallel:` task.
    /// Counts as one outcome in the recap, like `block:`. A `confirm:` (interactive
    /// pause) child isn't allowed — an agent-run playbook uses `--yes` anyway.
    pub(crate) parallel: Option<Vec<Task>>,
    /// Print a rendered message; no side effects.
    pub(crate) debug: Option<String>,
    /// Compute/override vars from rendered expressions (supports the `| json:<path>`
    /// filter — see `render()`). Side-effect-only, like `debug:` — runs even in `--dry`,
    /// since setting a var has no external effect. Keys within one `set_fact:` block
    /// don't see each other (`HashMap` iteration order isn't defined) — split into
    /// separate tasks if one fact needs to build on another.
    pub(crate) set_fact: Option<HashMap<String, String>>,
    /// Like `set_fact:`, but persisted to `<file>.data.json` (a sibling of the playbook,
    /// never auto-deleted) so `{{state.<key>}}` is readable in *later, separate*
    /// `tooler play` invocations too, not just later tasks in this same run -- the
    /// memory a `tooler cron local`-scheduled playbook needs across runs (e.g. "last
    /// processed row ID"). No symmetric `state_get:`: reading is just `{{state.<key>}}`
    /// in any field, the same way there's no `get_fact:` for `set_fact:`.
    pub(crate) state_set: Option<HashMap<String, String>>,
    /// Pause for a human `y`/`N` confirmation before continuing; the rendered message is
    /// the prompt. Never blocks when driven non-interactively (`--output json`, which is
    /// also the MCP/agent path) unless `--yes` was passed — it fails fast instead, so an
    /// agent-driven `tooler play` can't hang forever on stdin. See `RunEnv.auto_yes`.
    pub(crate) confirm: Option<String>,
    /// Kill the task if it runs longer than this many seconds. Supported on `run:`
    /// (kills the local subprocess) and `ssh:`/`fleet:` (kills the `ssh` process,
    /// ending the remote command's connection — see `db::run_with_deadline`). Every
    /// other action rejects `timeout:` upfront rather than silently not honoring it.
    #[serde(default)]
    pub(crate) timeout: Option<u64>,
    /// Dump `from`'s database and restore it into `to`'s, both reached through the same
    /// `server:` SSH profile. Dump bytes stay in memory the whole way — never written to
    /// local disk.
    pub(crate) sync_db: Option<DbSyncSpec>,
    /// Rsync a directory from one path to another on the same `server:`. `from` gets a
    /// trailing slash appended if missing, so it always copies contents, not the
    /// directory itself (see `ensure_trailing_slash`).
    pub(crate) sync_files: Option<SyncFilesSpec>,
    /// Write rendered `content` to a local file at `path` (relative to this playbook's
    /// own directory). Only the destination path and byte count are ever printed — never
    /// the content — since `content` may itself resolve `{{secret.*}}` tokens (e.g.
    /// writing a `.env` file). `register:` (if set) captures the byte count written. See
    /// `WriteFileSpec`.
    pub(crate) write_file: Option<WriteFileSpec>,
    /// Render a Jinja-style template file (`{% for %}`/`{% if %}`) against the playbook's
    /// vars and write it locally, or push it to a remote path (`server:`, requires
    /// `confirm: true`). See `TemplateSpec`.
    pub(crate) template: Option<TemplateSpec>,
    /// Parse a local CSV file at `path` (relative to this playbook's own directory).
    /// `register:` (if set) captures a JSON array of rows — same directly-`loop:
    /// {from: "{{reg}}"}`-chainable convention `db_query:`/`scrape:`/`mail_check:` all
    /// use. See `ReadCsvSpec`.
    pub(crate) read_csv: Option<ReadCsvSpec>,
    /// Write a registered JSON array (from db_query:/read_csv:/http:+`| json:` filter) to
    /// a local CSV file at `path` (relative to this playbook's own directory) — the
    /// inverse of `read_csv:`. See `WriteCsvSpec`.
    pub(crate) write_csv: Option<WriteCsvSpec>,
    /// The playbook's own repo's branch/tag/status/recent-commits summary — the same
    /// `commands::git::compute_summary` `tooler git summary` uses. No fields; invoke as
    /// `git_summary: {}`. See `GitSummarySpec`.
    pub(crate) git_summary: Option<GitSummarySpec>,
    /// Commits since the last tag (or `from:`), categorized into features/fixes/other —
    /// the same `commands::git::compute_changelog` `tooler git changelog` uses. See
    /// `GitChangelogSpec`.
    pub(crate) git_changelog: Option<GitChangelogSpec>,
    /// List GitHub pull requests via the `gh` CLI — the same `commands::gh::fetch_prs`
    /// `tooler gh prs` uses. See `GhPrsSpec`.
    pub(crate) gh_prs: Option<GhPrsSpec>,
    /// Run a read-only SQL query against a database over SSH and capture the rows.
    /// `register:` (if set) captures a JSON array of row objects, same convention as
    /// `scrape:` — directly chainable into `loop: {from: "{{reg}}"}` or `report:`. See
    /// `DbQuerySpec`.
    pub(crate) db_query: Option<DbQuerySpec>,
    /// Send an email over SMTP, either through a configured `server:` profile
    /// (`tooler config set mail.<name>.host ...` + `mail.<name>.password`, the latter in
    /// the OS keychain) or fully inline `host`/`user`/`password` fields. See `MailSpec`.
    pub(crate) mail: Option<MailSpec>,
    /// Read a mail profile's inbox over IMAP (defaults to unseen messages only).
    /// `register:` (if set) captures a JSON array of messages — same
    /// `loop: {from: "{{reg}}"}`-chainable convention as `db_query:`/`scrape:`. See
    /// `MailCheckSpec`.
    pub(crate) mail_check: Option<MailCheckSpec>,
    /// Run a single INSERT/UPDATE/DELETE statement against a database over SSH.
    /// Deliberately requires `confirm: true` in the YAML itself — never runs silently.
    /// See `DbExecSpec`.
    pub(crate) db_exec: Option<DbExecSpec>,
    /// Bulk-load a local file into a remote table over SSH (`psql \copy` / `mysql LOAD
    /// DATA LOCAL INFILE`) — the ETL "L". Requires `confirm: true`. See `DbLoadSpec`.
    pub(crate) db_load: Option<DbLoadSpec>,
    /// Write a value into the OS keychain (`{{secret.<profile>.<key>}}`'s own backing
    /// store) — the write-side counterpart to reading `{{secret.*}}`, so a playbook can
    /// generate/rotate a credential end-to-end without dropping out to `tooler config
    /// set`. `value:` renders normally, so `{{secret.old.key}}` (copying/rotating
    /// between profiles) works with no special-casing. Deliberately requires `confirm:
    /// true` in the YAML itself, same non-negotiable gate `db_exec:`/`fs_write:` use.
    /// See `SecretSetSpec`.
    pub(crate) secret_set: Option<SecretSetSpec>,
    /// Pull the latest code, run a build step, restart a service, and health-check it,
    /// each step optional — the native playbook equivalent of `tooler deploy run`.
    /// Deliberately requires `confirm: true`, same gate every other action that
    /// mutates or restarts remote state already has. See `DeploySpec`.
    pub(crate) deploy: Option<DeploySpec>,
    /// Push a local file to an absolute remote path over scp — the same
    /// `commands::ssh::run_scp` `tooler ssh copy` uses. Deliberately requires
    /// `confirm: true`, same gate `fs_write:` uses. See `UploadSpec`.
    pub(crate) upload: Option<UploadSpec>,
    /// Manage a remote server's crontab — the same `tooler cron` engine. Exactly one of
    /// `add`/`remove`/`list`; `add`/`remove` require `confirm: true`, `list` is
    /// read-only. See `CronSpec`.
    pub(crate) cron: Option<CronSpec>,
    /// Cap concurrent `loop:` iterations to N at a time (processed in chunks of N) instead
    /// of the default strictly-sequential execution. Only valid combined with `loop:`. See
    /// `run_loop_parallel`.
    #[serde(default)]
    pub(crate) max_parallel: Option<usize>,
    /// Only valid combined with `loop:`: attempt every item regardless of an earlier one
    /// failing (each item still respects its own `retries:`/`until:`/`failed_when:`),
    /// instead of aborting on the first failure and leaving the rest untried. The task
    /// itself still fails at the end (respecting `ignore_errors:`, same as any other
    /// failure) if any item failed, with a summary naming which ones. `register:`'s
    /// `.results` keeps one entry per item either way — a failed item's slot is an empty
    /// string, so positions still line up with the original item order. See `run_task`,
    /// `run_loop_parallel`.
    #[serde(default)]
    pub(crate) continue_on_error: bool,
    /// Loads a flat `key: value` vars file mid-playbook (same file shape and loader as
    /// `vars_files:`/`--vars-file`, including transparent decryption of a `tooler
    /// vault`-encrypted file) — for loading vars based on something computed during this
    /// run, rather than only ever upfront via `vars_files:`. Path resolves relative to
    /// this playbook's own directory, same as `vars_files:` (also unconfined, same as
    /// `vars_files:` — this is an author-time path, not untrusted input). No `register:`
    /// support — like `set_fact:`, its job is setting vars directly.
    #[serde(default)]
    pub(crate) include_vars: Option<String>,
    /// Run any handlers `notify:`ed so far, right now, instead of waiting for them to run
    /// once at the very end of the playbook — Ansible's `meta: flush_handlers`. Only
    /// meaningful as a direct task in the top-level playbook's own `tasks:` (or an
    /// `include:`d sub-playbook's own `tasks:`, which has its own handler state); used
    /// inside `block:`/`rescue:`/`always:` it fails clearly instead of silently doing
    /// nothing, since those run outside `execute_playbook`'s handler bookkeeping. A no-op
    /// (still counts as `ok`) when nothing is pending. See `execute_playbook`,
    /// `run_notified_handlers`.
    #[serde(default)]
    pub(crate) flush_handlers: bool,
}

/// One `loop:` item — a plain scalar (`{{item}}`) or a map (`{{item.<field>}}` per key).
#[derive(Debug, Deserialize, Clone, schemars::JsonSchema)]
#[serde(untagged)]
pub(crate) enum LoopItem {
    Scalar(String),
    Map(HashMap<String, String>),
}

/// `loop:`'s two shapes — a static YAML list (unchanged, existing behavior) or a dynamic
/// source resolved at task-run time from a rendered var. `serde`'s untagged matching tries
/// `Static` first; a YAML sequence (`loop: [a, b, c]`) parses as `Static`, and a mapping
/// with a `from:` key (`loop: {from: "{{items}}"}`) parses as `Dynamic`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub(crate) enum LoopSpec {
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
        /// Process the resolved items in groups of N instead of one at a time. Each
        /// iteration then exposes the whole group as `{{batch}}` (a JSON array, the same
        /// shape `from:` accepted), plus `{{batch_index}}` (0-based) and `{{batch_size}}`
        /// (that group's actual count — the last group may be smaller). Lets a task do
        /// one bulk operation per group — a chunked `run:`/`db_load:` instead of N
        /// per-row round trips. Composes with `max_parallel:` (the groups become the
        /// units run concurrently).
        #[serde(default)]
        batch: Option<usize>,
    },
}

/// One unit of `loop:` work: a single item (the default), or — with `batch: N` — a group
/// of up to N items handled together. See `resolve_loop_units`.
pub(crate) enum LoopUnit {
    Item(LoopItem),
    Batch { items: Vec<LoopItem>, index: usize },
}

/// `assert:`'s two shapes — a bare condition string (unchanged), or a structured form
/// checking several conditions in one task with its own failure message. `serde`'s
/// untagged matching tries `Simple` first; a bare YAML string parses as `Simple`, and a
/// mapping with a `that:` key parses as `Structured`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub(crate) enum AssertSpec {
    Simple(String),
    Structured {
        that: Vec<String>,
        #[serde(default)]
        msg: Option<String>,
    },
}

/// `run:`'s two shapes — a bare shell command string (unchanged), or a structured form
/// adding `env:` to inject extra environment variables into the spawned subprocess
/// directly, instead of interpolating them into the command string by hand (which needs
/// `| quote` to be safe and doesn't compose well with values containing spaces/quotes).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(untagged, deny_unknown_fields)]
pub(crate) enum RunSpec {
    Simple(String),
    Structured {
        command: String,
        #[serde(default)]
        env: HashMap<String, String>,
    },
}

/// Resolves a `LoopSpec` into the `Vec<LoopItem>` `run_task` actually iterates —
/// `Static` is used as-is; `Dynamic` renders `from` against `vars` and either parses it as
/// a JSON array or falls back to a plain-text split. See `LoopSpec::Dynamic`'s doc comment
/// for the exact rules.
pub(crate) fn resolve_loop_items(spec: &LoopSpec, vars: &HashMap<String, String>) -> Vec<LoopItem> {
    match spec {
        LoopSpec::Static(items) => items.clone(),
        LoopSpec::Dynamic { from, split, .. } => {
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

/// The batch size a `LoopSpec` asks for, if any (`Some(n)` only for `Dynamic { batch:
/// Some(n) }` with `n >= 1`).
pub(crate) fn loop_batch_size(spec: &LoopSpec) -> Option<usize> {
    match spec {
        LoopSpec::Dynamic { batch: Some(n), .. } if *n >= 1 => Some(*n),
        _ => None,
    }
}

/// Resolves a `LoopSpec` into the units `run_task`/`run_loop_parallel` iterate: one
/// `LoopUnit::Item` per item normally, or — with `batch: N` — the items regrouped into
/// `LoopUnit::Batch` chunks of up to N (the last chunk may be smaller).
pub(crate) fn resolve_loop_units(spec: &LoopSpec, vars: &HashMap<String, String>) -> Vec<LoopUnit> {
    let items = resolve_loop_items(spec, vars);
    match loop_batch_size(spec) {
        None => items.into_iter().map(LoopUnit::Item).collect(),
        Some(n) => items
            .chunks(n)
            .enumerate()
            .map(|(index, chunk)| LoopUnit::Batch {
                items: chunk.to_vec(),
                index,
            })
            .collect(),
    }
}

/// Rebuilds a `LoopItem` back into the JSON value it came from — the inverse of
/// `resolve_loop_items`' own parse — so `{{batch}}` round-trips a `db_query:`/`scrape:`
/// result unchanged.
pub(crate) fn loop_item_to_json(item: &LoopItem) -> serde_json::Value {
    match item {
        LoopItem::Scalar(s) => serde_json::Value::String(s.clone()),
        LoopItem::Map(m) => serde_json::Value::Object(
            m.iter()
                .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
                .collect(),
        ),
    }
}

/// `include:`'s two shapes — a bare playbook reference (existing, unchanged behavior) or
/// a mapping with per-call `vars:` overrides. `serde` tries `Simple` first: a bare scalar
/// (`include: sub.yml`) parses as `Simple`; a mapping (`include: {file: sub.yml, vars:
/// {...}}`) parses as `WithVars`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub(crate) enum IncludeSpec {
    Simple(String),
    WithVars {
        file: String,
        #[serde(default)]
        vars: HashMap<String, String>,
    },
}

impl IncludeSpec {
    pub(crate) fn file(&self) -> &str {
        match self {
            IncludeSpec::Simple(f) => f,
            IncludeSpec::WithVars { file, .. } => file,
        }
    }

    pub(crate) fn vars(&self) -> &HashMap<String, String> {
        static EMPTY: std::sync::OnceLock<HashMap<String, String>> = std::sync::OnceLock::new();
        match self {
            IncludeSpec::Simple(_) => EMPTY.get_or_init(HashMap::new),
            IncludeSpec::WithVars { vars, .. } => vars,
        }
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SshSpec {
    /// Server profile name (see: tooler server list)
    pub(crate) server: String,
    pub(crate) command: String,
    #[serde(default)]
    pub(crate) sudo: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct FleetSpec {
    /// Comma-separated server profile names (mutually exclusive with all/group)
    #[serde(default)]
    pub(crate) servers: Option<String>,
    /// Named server group (see: tooler group list)
    #[serde(default)]
    pub(crate) group: Option<String>,
    #[serde(default)]
    pub(crate) all: bool,
    pub(crate) command: String,
    #[serde(default)]
    pub(crate) sudo: bool,
    /// Run on all targeted servers concurrently instead of one at a time
    #[serde(default)]
    pub(crate) parallel: bool,
    /// Only meaningful combined with `parallel: true` — runs targets in chunks of this
    /// size (one chunk fully finishes before the next starts) instead of all-at-once, a
    /// canary/rolling pattern (e.g. restart nginx 3 servers at a time across a
    /// 20-server fleet) rather than either strictly one-at-a-time or all-at-once.
    /// Ignored when `parallel` isn't set.
    #[serde(default)]
    pub(crate) batch_size: Option<usize>,
}

/// `fs_cat:` — reads a remote file over SSH via `commands::fs::cat_cmd`, the same
/// command builder `tooler fs cat` uses.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct FsCatSpec {
    /// Server profile name (see: tooler server list)
    pub(crate) server: String,
    /// Remote file path
    pub(crate) path: String,
}

/// A destructive task spec gated behind `confirm: true` in the YAML — implemented by
/// `FsWriteSpec`/`SystemdRestartSpec`/`PsKillSpec`/`DbExecSpec`. See `Confirmed`.
pub(crate) trait RequiresConfirm {
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
pub(crate) struct Confirmed<'a, T>(&'a T);

impl<'a, T: RequiresConfirm> Confirmed<'a, T> {
    pub(crate) fn require(spec: &'a T, action: &str, task_name: &str) -> Result<Self> {
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
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct FsWriteSpec {
    pub(crate) server: String,
    /// Remote file path
    pub(crate) path: String,
    pub(crate) content: String,
    #[serde(default)]
    pub(crate) confirm: bool,
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
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SystemdRestartSpec {
    pub(crate) server: String,
    /// Unit name, e.g. nginx or myapp.service
    pub(crate) unit: String,
    #[serde(default)]
    pub(crate) sudo: bool,
    /// Sudo password (only used with sudo: true; omit to rely on NOPASSWD)
    #[serde(default)]
    pub(crate) sudo_pass: Option<String>,
    #[serde(default)]
    pub(crate) confirm: bool,
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
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SystemdStatusSpec {
    pub(crate) server: String,
    pub(crate) unit: String,
}

/// `logs_tail:` — tails a remote file over SSH via `commands::logs::tail_cmd`. Only the
/// line count is ever printed (never the content, which could contain sensitive data) —
/// `register:` (if set) captures a JSON array of lines, same `loop: {from: "{{reg}}"}`
/// -chainable convention as `db_query:`/`scrape:`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct LogsTailSpec {
    pub(crate) server: String,
    /// Remote file path
    pub(crate) path: String,
    #[serde(default = "default_tail_lines")]
    pub(crate) lines: u32,
}

pub(crate) fn default_tail_lines() -> u32 {
    100
}

/// `logs_grep:` — searches a remote file over SSH via `commands::logs::grep_cmd` (a
/// fixed-substring match, not a regex). Same content-hiding convention as `logs_tail:`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct LogsGrepSpec {
    pub(crate) server: String,
    /// Remote file path
    pub(crate) path: String,
    /// Fixed substring to match (not a regex)
    pub(crate) pattern: String,
    #[serde(default = "default_grep_max_lines")]
    pub(crate) max_lines: usize,
}

pub(crate) fn default_grep_max_lines() -> usize {
    200
}

/// `ps_list:` — lists remote processes over SSH via `commands::ps::parse_ps_aux`/
/// `apply_filter`. Same content-hiding convention as `fs_cat:`/`logs_tail:`: row-shaped
/// data, so only the count prints; `register:` captures the JSON array.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct PsListSpec {
    pub(crate) server: String,
    /// Only include processes whose command line (or PID) matches this substring
    #[serde(default)]
    pub(crate) filter: Option<String>,
}

/// `ps_kill:` — sends a signal to a remote process via `commands::ps::kill_cmd`.
/// Deliberately requires `confirm: true` in the YAML itself, same non-negotiable gate
/// `db_exec:`/`fs_write:` use.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct PsKillSpec {
    pub(crate) server: String,
    pub(crate) pid: u32,
    #[serde(default = "default_kill_signal")]
    pub(crate) signal: String,
    #[serde(default)]
    pub(crate) sudo: bool,
    #[serde(default)]
    pub(crate) sudo_pass: Option<String>,
    #[serde(default)]
    pub(crate) confirm: bool,
}

impl RequiresConfirm for PsKillSpec {
    fn is_confirmed(&self) -> bool {
        self.confirm
    }
}

pub(crate) fn default_kill_signal() -> String {
    "TERM".to_string()
}

/// `stat:` — a remote server's uptime/memory/disk snapshot via `commands::stat::stat_cmd`/
/// `parse_sections`. A single small operational status blob, not row-shaped bulk data,
/// so it prints directly (same as `systemd_status:`); `register:` captures
/// `{uptime, memory, disk}` as JSON for `report:`/`mail:` chaining.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct StatSpec {
    pub(crate) server: String,
}

/// `git_summary:` — the local repo's branch/tag/status/recent-commits summary via
/// `commands::git::compute_summary`, run against the playbook's own directory (same cwd
/// convention `run:` already has). No fields; invoked as `git_summary: {}`.
#[derive(Debug, Deserialize, Default, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct GitSummarySpec {}

/// `git_changelog:` — commits since the last tag (or `from:`), categorized into
/// features/fixes/other, via `commands::git::compute_changelog`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct GitChangelogSpec {
    /// Starting tag or commit (defaults to the latest tag)
    #[serde(default)]
    pub(crate) from: Option<String>,
}

/// `gh_prs:` — lists GitHub pull requests via `commands::gh::fetch_prs` (shells out to
/// the `gh` CLI). Row-shaped external data like `db_query:`, so only the count prints;
/// `register:` captures the JSON array, directly chainable into `report:`/`loop:`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct GhPrsSpec {
    /// Repository as owner/name (defaults to the repo in the playbook's own directory)
    #[serde(default)]
    pub(crate) repo: Option<String>,
    /// Only PRs created on/after this date (YYYY-MM-DD)
    #[serde(default)]
    pub(crate) after: Option<String>,
    /// Only PRs created on/before this date (YYYY-MM-DD)
    #[serde(default)]
    pub(crate) before: Option<String>,
    #[serde(default = "default_pr_state")]
    pub(crate) state: String,
    #[serde(default = "default_pr_limit")]
    pub(crate) limit: u32,
}

pub(crate) fn default_pr_state() -> String {
    "all".to_string()
}

pub(crate) fn default_pr_limit() -> u32 {
    500
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct DbSyncSpec {
    /// Server profile to run mysqldump/pg_dump + mysql/psql through (both sides)
    pub(crate) server: String,
    pub(crate) from: DbSyncSide,
    pub(crate) to: DbSyncSide,
}

/// One side of a `sync_db:` task — either `env:` (a remote dotenv-style file, e.g. a
/// Laravel `.env`, to read DB_* credentials from) or the explicit fields. Mirrors
/// `commands::db::ConnOpts`, which `resolve_db_sync_creds` delegates to.
#[derive(Debug, Deserialize, Default, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct DbSyncSide {
    #[serde(default)]
    pub(crate) env: Option<String>,
    #[serde(default)]
    pub(crate) engine: Option<String>,
    #[serde(default)]
    pub(crate) host: Option<String>,
    #[serde(default)]
    pub(crate) port: Option<u16>,
    #[serde(default)]
    pub(crate) database: Option<String>,
    #[serde(default)]
    pub(crate) user: Option<String>,
    #[serde(default)]
    pub(crate) password: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SyncFilesSpec {
    /// Server profile (see: tooler server list)
    pub(crate) server: String,
    pub(crate) from: String,
    pub(crate) to: String,
    /// Pass `--delete` to rsync, removing destination files no longer present in `from`
    #[serde(default)]
    pub(crate) delete: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CheckPortSpec {
    pub(crate) host: String,
    pub(crate) port: u16,
    #[serde(default = "default_timeout")]
    pub(crate) timeout: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct EnvCheckSpec {
    pub(crate) reference: String,
    #[serde(default = "default_env_target")]
    pub(crate) target: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct HttpSpec {
    #[serde(default = "default_http_method")]
    pub(crate) method: String,
    pub(crate) url: String,
    #[serde(default)]
    pub(crate) headers: HashMap<String, String>,
    #[serde(default)]
    pub(crate) body: Option<String>,
    #[serde(default = "default_timeout")]
    pub(crate) timeout: u64,
    /// Don't fail the task on a non-2xx status — let when:/assert: on the registered
    /// `<reg>.status` decide instead. Default false, matching check_url:'s fail-fast.
    #[serde(default)]
    pub(crate) ignore_status: bool,
    /// Save the response body to this local file (relative to the playbook's own
    /// directory, confined via `join_confined`) instead of capturing it as a string —
    /// binary-safe, unlike the default `resp.text()` path. Combine with `register:` to
    /// capture the (still-relative) rendered `download:` path — not its bytes — for
    /// chaining straight into a later path-taking task, e.g.
    /// `mail: {attachments: ["{{reg}}"]}`, since every such task resolves its path the
    /// same way, relative to this same playbook directory.
    #[serde(default)]
    pub(crate) download: Option<String>,
    /// Follow pagination: keep fetching successive pages and concatenate their items
    /// into one `register:`ed JSON array. Mutually exclusive with `download:`. See
    /// `PaginateSpec`.
    #[serde(default)]
    pub(crate) paginate: Option<PaginateSpec>,
}

pub(crate) fn default_max_pages() -> usize {
    20
}

/// `http: {paginate: ...}` — after each page is fetched, `next` is rendered with the
/// page's body available as `{{page}}` (and `{{page.status}}`); its result is the next
/// page's URL. The loop stops when `next` renders blank, equals the current URL, or
/// `max_pages` is reached. `items` (a `| json:`-style path) points at the array within
/// each page's body — every page's array is concatenated into the registered result;
/// omit it to collect each whole page body as one element instead. `register:` then also
/// gets `<reg>.pages` (the page count) alongside `<reg>.status` (the last page's).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct PaginateSpec {
    pub(crate) next: String,
    #[serde(default)]
    pub(crate) items: Option<String>,
    #[serde(default = "default_max_pages")]
    pub(crate) max_pages: usize,
}

/// Poll one of `check_url`/`check_port`/`ssh` (exactly one — validated upfront in
/// `run_task_once`) every `interval` seconds until it succeeds or `timeout` elapses.
/// Distinct from `retries:`, which retries a whole task on *failure*; `wait_for:` is for
/// "keep checking until this becomes true" (e.g. wait for a service to come back up after
/// a restart), so it doesn't log every attempt the way `retries:` does.
#[derive(Debug, Deserialize, Default, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct WaitForSpec {
    #[serde(default)]
    pub(crate) check_url: Option<String>,
    #[serde(default)]
    pub(crate) check_port: Option<CheckPortSpec>,
    #[serde(default)]
    pub(crate) ssh: Option<SshSpec>,
    /// Poll until a local file (relative to the playbook directory, confined via
    /// `join_confined`) exists -- e.g. waiting for an upload to land.
    #[serde(default)]
    pub(crate) file_exists: Option<String>,
    /// Poll until a local file no longer exists -- e.g. waiting for a lock to clear.
    #[serde(default)]
    pub(crate) file_absent: Option<String>,
    #[serde(default = "default_wait_interval")]
    pub(crate) interval: u64,
    #[serde(default = "default_wait_timeout")]
    pub(crate) timeout: u64,
}

pub(crate) fn default_wait_interval() -> u64 {
    2
}
pub(crate) fn default_wait_timeout() -> u64 {
    60
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ScrapeSpec {
    pub(crate) url: String,
    #[serde(default)]
    pub(crate) headers: HashMap<String, String>,
    #[serde(default = "default_timeout")]
    pub(crate) timeout: u64,
    /// CSS selector for each "row"; omit to scrape the whole page as a single item.
    #[serde(default)]
    pub(crate) each: Option<String>,
    /// field name -> CSS selector, optionally `"<selector>@<attr>"` to grab an attribute
    /// (e.g. `href`, `src`) instead of trimmed text content.
    pub(crate) fields: HashMap<String, String>,
}

/// Generate a PDF/Excel/HTML report — the same engine `tooler report pdf/excel/html`
/// uses (`report::{pdf,excel,html}::build`), but fed inline data instead of file paths,
/// so a `register:`ed `http:`/`scrape:` result can go straight into a report with no
/// temp-file round-trip.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReportSpec {
    /// "html", "pdf", or "excel"
    pub(crate) format: String,
    #[serde(default = "default_report_title")]
    pub(crate) title: String,
    /// name -> a rendered value (typically `"{{a_registered_var}}"`). Parsed as JSON if
    /// possible; a value that isn't valid JSON is wrapped as a plain JSON string instead
    /// of failing the task, matching the DSL's general tolerance for opaque var content
    /// elsewhere (e.g. a missing `scrape:` field becomes `""`, not an error).
    pub(crate) sources: HashMap<String, String>,
    /// Output path, relative to the playbook's own directory (same rule as `run:`'s
    /// working directory / `env_check:`'s paths).
    pub(crate) out: String,
}

pub(crate) fn default_report_title() -> String {
    "Tooler Report".to_string()
}

/// `write_file:` — writes rendered `content` to `path` (relative to the playbook's own
/// directory), creating parent directories as needed. `path` is confined to that
/// directory by `join_confined` — an absolute path or a `..` that nets outside it is
/// rejected, rather than silently writing wherever a rendered `{{var}}` happened to point.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct WriteFileSpec {
    pub(crate) path: String,
    pub(crate) content: String,
    /// Append instead of overwrite.
    #[serde(default)]
    pub(crate) append: bool,
}

/// `template:` — render a Jinja-style template file (`{% for %}`/`{% if %}`/`{{ }}`,
/// minijinja) against the playbook's vars and write the result. Each var is exposed to
/// the template as parsed JSON when it parses (so `{% for r in rows %}` works on a
/// `register:`ed array), otherwise as a plain string; `{{ now }}` (RFC3339 UTC) is
/// always available. `src` and — for a local write — `dest` are confined to the
/// playbook's own directory (same as `write_file:`). Set `server:` to push the rendered
/// file to a remote path over SSH instead (same mechanism `fs_write:` uses); that form
/// requires `confirm: true`. `register:` (if set) captures the byte count written.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct TemplateSpec {
    pub(crate) src: String,
    pub(crate) dest: String,
    /// Push the rendered file to this server profile's `dest` path over SSH (as the SSH
    /// user, same as `fs_write:` — no `sudo`). Omit to write `dest` locally.
    #[serde(default)]
    pub(crate) server: Option<String>,
    #[serde(default)]
    pub(crate) confirm: bool,
}

impl RequiresConfirm for TemplateSpec {
    fn is_confirmed(&self) -> bool {
        self.confirm
    }
}

/// `read_csv:` — the read-side counterpart to `write_file:`. `path` is confined to the
/// playbook's own directory the same way (`join_confined`). `headers: true` (default)
/// uses the first row as field names, producing one JSON object per row; `headers: false`
/// produces plain arrays instead. Every cell comes back as a JSON string -- no type
/// guessing, same "let the consumer decide" philosophy `parse_mysql_tsv` already uses.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReadCsvSpec {
    pub(crate) path: String,
    #[serde(default = "default_true")]
    pub(crate) headers: bool,
    /// Single character. Defaults to ','.
    #[serde(default)]
    pub(crate) delimiter: Option<String>,
}

/// `write_csv:` — the write-side counterpart to `read_csv:`. `path` is confined to the
/// playbook's own directory the same way. `data` is rendered and must parse as a JSON
/// array: an array of objects writes a header row from the *first* object's keys (unless
/// `headers: false`) followed by one row per object in that key order — since this crate
/// builds `serde_json::Value::Object` without the `preserve_order` feature, that key
/// order is alphabetical, not YAML/JSON source order; an array of plain values/arrays is
/// written as raw rows (`headers:` has no effect — there are no field names to derive a
/// header from).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct WriteCsvSpec {
    pub(crate) path: String,
    /// Rendered, then parsed as a JSON array — typically `"{{a_registered_var}}"`.
    pub(crate) data: String,
    #[serde(default = "default_true")]
    pub(crate) headers: bool,
    /// Single character. Defaults to ','.
    #[serde(default)]
    pub(crate) delimiter: Option<String>,
}

/// Renders one JSON value as a CSV cell for `write_csv:`: a string is used as-is (not
/// re-quoted with JSON escaping), a number/bool uses its plain display form, and
/// null/missing becomes an empty cell — matching how `render()` already stringifies
/// values elsewhere in this DSL.
pub(crate) fn json_cell_to_string(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Flattens a JSON array into CSV bytes: an array of objects writes a header row from
/// the first object's keys (unless `headers` is false) then one row per object in that
/// key order; an array of plain values/arrays is written as raw rows. Shared by
/// `write_csv:` and `db_load: {format: json}`.
pub(crate) fn json_array_to_csv(
    elements: &[serde_json::Value],
    headers: bool,
    delimiter: u8,
) -> Result<Vec<u8>> {
    let mut writer = csv::WriterBuilder::new()
        .delimiter(delimiter)
        .from_writer(Vec::new());
    if let Some(serde_json::Value::Object(first)) = elements.first() {
        let cols: Vec<String> = first.keys().cloned().collect();
        if headers {
            writer
                .write_record(&cols)
                .context("writing CSV header row")?;
        }
        for el in elements {
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
        for el in elements {
            let record: Vec<String> = match el {
                serde_json::Value::Array(items) => items.iter().map(json_cell_to_string).collect(),
                other => vec![json_cell_to_string(other)],
            };
            writer.write_record(&record).context("writing CSV row")?;
        }
    }
    writer.into_inner().context("finalizing CSV output")
}

/// `db_query:` — mirrors `commands::db::DbSubcommand::Query`'s fields exactly, so the
/// mental model transfers 1:1 from the standalone `tooler db query` command.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct DbQuerySpec {
    /// Server profile to run the query through (see: tooler server list)
    pub(crate) server: String,
    /// SQL query (SELECT/SHOW/EXPLAIN/WITH/DESCRIBE only — enforced by `db::run_query`)
    pub(crate) sql: String,
    /// Remote path to a dotenv-style file (e.g. Laravel .env) to read DB_* credentials
    /// from, instead of the explicit fields below.
    #[serde(default)]
    pub(crate) env: Option<String>,
    #[serde(default)]
    pub(crate) engine: Option<String>,
    #[serde(default)]
    pub(crate) host: Option<String>,
    #[serde(default)]
    pub(crate) port: Option<u16>,
    #[serde(default)]
    pub(crate) database: Option<String>,
    #[serde(default)]
    pub(crate) user: Option<String>,
    #[serde(default)]
    pub(crate) password: Option<String>,
    #[serde(default = "default_db_max_rows")]
    pub(crate) max_rows: usize,
}

pub(crate) fn default_db_max_rows() -> usize {
    1000
}

/// `db_exec:` — same connection fields as `DbQuerySpec` minus `max_rows` (a single
/// statement has no rows to cap), plus `confirm`. Mirrors `commands::db::DbSubcommand::
/// Exec`'s fields, enforced read-side by `db::ensure_write_only` (INSERT/UPDATE/DELETE
/// only, no DDL). `confirm` must be `true` in the YAML itself -- the same "never runs
/// silently" posture `tooler db restore --confirm` uses on the CLI, just expressed as a
/// visible task field instead of a flag, so it shows up in a code review/diff.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct DbExecSpec {
    pub(crate) server: String,
    /// SQL statement (INSERT/UPDATE/DELETE only — enforced by `db::run_exec`)
    pub(crate) sql: String,
    #[serde(default)]
    pub(crate) env: Option<String>,
    #[serde(default)]
    pub(crate) engine: Option<String>,
    #[serde(default)]
    pub(crate) host: Option<String>,
    #[serde(default)]
    pub(crate) port: Option<u16>,
    #[serde(default)]
    pub(crate) database: Option<String>,
    #[serde(default)]
    pub(crate) user: Option<String>,
    #[serde(default)]
    pub(crate) password: Option<String>,
    #[serde(default)]
    pub(crate) confirm: bool,
}

impl RequiresConfirm for DbExecSpec {
    fn is_confirmed(&self) -> bool {
        self.confirm
    }
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub(crate) enum DbLoadFormat {
    #[default]
    Csv,
    Tsv,
    /// The file is a JSON array (typically a `db_query:`/`read_csv:` result written with
    /// `write_file:`) — converted to CSV in-process before streaming.
    Json,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub(crate) enum DbLoadMode {
    /// Append rows to whatever is already in the table (the default).
    #[default]
    Append,
    /// `TRUNCATE` the table first, then load.
    Truncate,
    /// MySQL only: a row whose PK/unique key already exists overwrites the existing row
    /// (`LOAD DATA REPLACE`). Not supported for Postgres.
    Upsert,
}

/// `db_load:` — bulk-load a local file into a remote table over one SSH connection
/// (`psql \copy` / `mysql LOAD DATA LOCAL INFILE`), covering the ETL "L" that `db_exec:`
/// (one statement) and `sync_db:` (whole database) don't. `file` resolves relative to
/// the playbook's own directory (confined, same as `write_csv:`) — typically a
/// `write_csv:`/`write_file:` output from an earlier task. Requires `confirm: true`,
/// same gate `db_exec:` uses. `register:` (if set) captures the client's summary line
/// (`COPY 42` on Postgres). Connection fields mirror `db_exec:`/`db_query:` exactly.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct DbLoadSpec {
    pub(crate) server: String,
    pub(crate) table: String,
    pub(crate) file: String,
    #[serde(default)]
    pub(crate) format: DbLoadFormat,
    #[serde(default)]
    pub(crate) mode: DbLoadMode,
    /// Explicit target column list, in file-column order. Omit to load every column in
    /// table order.
    #[serde(default)]
    pub(crate) columns: Option<Vec<String>>,
    /// The file's first line is a header row (CSV/TSV only; always true for `json`).
    #[serde(default = "default_true")]
    pub(crate) headers: bool,
    #[serde(default)]
    pub(crate) env: Option<String>,
    #[serde(default)]
    pub(crate) engine: Option<String>,
    #[serde(default)]
    pub(crate) host: Option<String>,
    #[serde(default)]
    pub(crate) port: Option<u16>,
    #[serde(default)]
    pub(crate) database: Option<String>,
    #[serde(default)]
    pub(crate) user: Option<String>,
    #[serde(default)]
    pub(crate) password: Option<String>,
    #[serde(default)]
    pub(crate) confirm: bool,
}

impl RequiresConfirm for DbLoadSpec {
    fn is_confirmed(&self) -> bool {
        self.confirm
    }
}

/// `secret_set:` — writes `value` into the OS keychain under `profile`/`key`, the same
/// store `{{secret.<profile>.<key>}}` reads from.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SecretSetSpec {
    pub(crate) profile: String,
    pub(crate) key: String,
    pub(crate) value: String,
    #[serde(default)]
    pub(crate) confirm: bool,
}

impl RequiresConfirm for SecretSetSpec {
    fn is_confirmed(&self) -> bool {
        self.confirm
    }
}

pub(crate) fn default_deploy_health_timeout() -> u64 {
    5
}
pub(crate) fn default_deploy_health_retries() -> u32 {
    3
}
pub(crate) fn default_deploy_health_delay() -> u64 {
    2
}

/// `deploy:` — the native playbook equivalent of `tooler deploy run`: pull the latest
/// code, run a build step, restart a service, and health-check it, each step optional.
/// Deliberately requires `confirm: true`, same gate every other action that mutates or
/// restarts remote state already has. See `commands::deploy::apply_deploy_steps`, the
/// exact function both this task and the standalone CLI command run through.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeploySpec {
    pub(crate) server: String,
    /// Remote path (git repo) to deploy
    pub(crate) path: String,
    /// Pull the latest code (git pull) in `path` before restarting
    #[serde(default)]
    pub(crate) pull: bool,
    /// Command to run remotely in `path` after pulling (e.g. a build step)
    #[serde(default)]
    pub(crate) build: Option<String>,
    /// Command to restart the service (e.g. "systemctl restart myapp")
    #[serde(default)]
    pub(crate) restart: Option<String>,
    /// URL to check after restarting
    #[serde(default)]
    pub(crate) health_url: Option<String>,
    /// Timeout in seconds for each health check attempt
    #[serde(default = "default_deploy_health_timeout")]
    pub(crate) health_timeout: u64,
    /// Number of health check attempts before giving up
    #[serde(default = "default_deploy_health_retries")]
    pub(crate) health_retries: u32,
    /// Seconds to wait between health check attempts
    #[serde(default = "default_deploy_health_delay")]
    pub(crate) health_delay: u64,
    /// Run the restart command via sudo
    #[serde(default)]
    pub(crate) sudo: bool,
    /// Sudo password (only used with `sudo: true`; omit to rely on NOPASSWD)
    #[serde(default)]
    pub(crate) sudo_pass: Option<String>,
    #[serde(default)]
    pub(crate) confirm: bool,
}

impl RequiresConfirm for DeploySpec {
    fn is_confirmed(&self) -> bool {
        self.confirm
    }
}

/// `upload:` — push a local file to an absolute remote path over scp, the same
/// `commands::ssh::run_scp` `tooler ssh copy` uses. `local` resolves relative to this
/// playbook's own directory (an absolute path is honored as-is, unconfined — an
/// author-time path like a build artifact, same rationale as `include_vars:`). `remote`
/// is the absolute destination path on the server. Deliberately requires `confirm:
/// true`, same gate `fs_write:` uses — it writes to the remote filesystem.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct UploadSpec {
    pub(crate) server: String,
    pub(crate) local: String,
    pub(crate) remote: String,
    #[serde(default)]
    pub(crate) confirm: bool,
}

impl RequiresConfirm for UploadSpec {
    fn is_confirmed(&self) -> bool {
        self.confirm
    }
}

/// `cron:` — manage a remote server's crontab, the same `tooler cron` engine. Exactly
/// one of `add`/`remove`/`list` (validated in `exec_cron`). `add` appends a full
/// crontab line; `remove` drops every line containing the given fixed substring; `list`
/// captures the parsed entries. `add`/`remove` mutate the crontab and require `confirm:
/// true`; `list` is read-only and needs no confirm. `register:` (if set) captures:
/// `add` → `"true"`, `remove` → the count of removed lines, `list` → a JSON array of
/// entries — directly `loop: {from: "{{reg}}"}`-chainable, same as `db_query:`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CronSpec {
    pub(crate) server: String,
    #[serde(default)]
    pub(crate) add: Option<String>,
    #[serde(default)]
    pub(crate) remove: Option<String>,
    #[serde(default)]
    pub(crate) list: bool,
    #[serde(default)]
    pub(crate) confirm: bool,
}

impl RequiresConfirm for CronSpec {
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
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct MailSpec {
    /// Mail profile to send through (see: tooler config set mail.<name>.host, and
    /// following fields).
    #[serde(default)]
    pub(crate) server: Option<String>,
    pub(crate) to: String,
    #[serde(default)]
    pub(crate) cc: Option<String>,
    #[serde(default)]
    pub(crate) bcc: Option<String>,
    pub(crate) subject: String,
    pub(crate) body: String,
    /// Send the body as `text/html` instead of `text/plain`.
    #[serde(default)]
    pub(crate) html: bool,
    #[serde(default)]
    pub(crate) from: Option<String>,
    #[serde(default)]
    pub(crate) host: Option<String>,
    #[serde(default)]
    pub(crate) port: Option<u16>,
    #[serde(default)]
    pub(crate) user: Option<String>,
    #[serde(default)]
    pub(crate) password: Option<String>,
    /// "starttls" | "tls" | "none" — overrides both the profile's `tls` and the
    /// port-based inference in `resolve_mail_creds`.
    #[serde(default)]
    pub(crate) tls: Option<String>,
    /// Local file paths to attach, relative to the playbook's own directory (confined
    /// via `join_confined`) — typically a `report:` output or an `http: {download:
    /// ...}` result.
    #[serde(default)]
    pub(crate) attachments: Vec<String>,
}

/// `mail_check:` — reads a `server:` mail profile's inbox over IMAP. Profile-only (no
/// inline host/user/password the way `mail:`/`db_query:` allow): narrower, newer, and a
/// profile is the common case since IMAP shares the same mailbox login `mail:` already
/// uses. See `fetch_mail`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct MailCheckSpec {
    /// Mail profile to read from (see: tooler config set mail.<name>.imap_port, etc).
    pub(crate) server: String,
    #[serde(default = "default_mail_folder")]
    pub(crate) folder: String,
    /// Only fetch messages without the \Seen flag. Default true — the common "what's new"
    /// case.
    #[serde(default = "default_true")]
    pub(crate) unseen_only: bool,
    #[serde(default = "default_mail_check_limit")]
    pub(crate) limit: u32,
    /// Fetch each message's plain-text body too, not just headers. Off by default to keep
    /// `register:`'s captured JSON small.
    #[serde(default)]
    pub(crate) include_body: bool,
    /// Mark fetched messages \Seen afterward, so a later run's `unseen_only` doesn't
    /// reprocess them -- the idempotency primitive for "check inbox -> act -> don't act
    /// twice". Off by default: mutating mailbox state is opt-in, same posture `db_query:`'s
    /// read-only default and `db exec`'s `confirm:` gate already establish.
    #[serde(default)]
    pub(crate) mark_seen: bool,
}

pub(crate) fn default_mail_folder() -> String {
    "INBOX".to_string()
}
pub(crate) fn default_mail_check_limit() -> u32 {
    10
}
pub(crate) fn default_true() -> bool {
    true
}

pub(crate) fn default_timeout() -> u64 {
    5
}
pub(crate) fn default_env_target() -> String {
    ".env".to_string()
}
pub(crate) fn default_http_method() -> String {
    "GET".to_string()
}

// ── Entrypoint ────────────────────────────────────────────────────────────────

/// A playbook argument is a literal path (existing, unchanged behavior) if it contains a
/// `/` or already ends in `.yml`/`.yaml`; otherwise it's a bare name, resolved against
/// `<project_root>/playbooks/<name>.yml` (then `.yaml`).
pub(crate) fn is_literal_path(s: &str) -> bool {
    s.contains('/') || s.ends_with(".yml") || s.ends_with(".yaml")
}

/// Joins `rel` onto `base`, rejecting anything that would land outside `base`: an
/// absolute `rel` (which `Path::join` would otherwise honor verbatim, discarding `base`
/// entirely), or a `..` that nets below `base` once walked lexically. Doesn't touch the
/// filesystem (no `canonicalize`) since the caller — `write_file:` — may be about to
/// create the file, so it need not exist yet. `a/../b` is allowed (it never actually
/// leaves `base`, just references it awkwardly); `../b` or `a/../../b` are not.
/// A path that has passed through `join_confined` — the only way to construct one. A
/// local-file action's own executing code can require `&ConfinedPath` instead of
/// `&Path`/`PathBuf`, making it impossible to hand it a path that skipped confinement,
/// not just conventionally unlikely — the same "prove it, don't just check it" pattern
/// `Confirmed<T>` applies to the `confirm:` gate.
pub(crate) struct ConfinedPath(PathBuf);

impl std::ops::Deref for ConfinedPath {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for ConfinedPath {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl ConfinedPath {
    /// Only needed where an owned `PathBuf` has to cross into a function that isn't
    /// (and shouldn't be) coupled to this type — e.g. `send_mail`, also called from
    /// `tooler mail send`'s own CLI path with ordinary `PathBuf`s.
    pub(crate) fn into_path_buf(self) -> PathBuf {
        self.0
    }

    #[cfg(test)]
    pub(crate) fn as_path(&self) -> &Path {
        &self.0
    }
}

pub(crate) fn join_confined(base: &Path, rel: &str) -> Result<ConfinedPath> {
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
    Ok(ConfinedPath(resolved))
}

/// Appends a trailing `/` to `path` if missing — rsync only copies a source directory's
/// *contents* when the source path ends in `/`; without it, the directory itself gets
/// nested one level deeper inside the destination. A well-known footgun `sync_files:`
/// guards against automatically.
pub(crate) fn ensure_trailing_slash(path: &str) -> String {
    if path.ends_with('/') {
        path.to_string()
    } else {
        format!("{path}/")
    }
}
