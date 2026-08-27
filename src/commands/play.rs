use crate::{context::Context, output::OutputFormat, project, report};
use anyhow::{Context as _, Result, bail};
use clap::Args;
use colored::Colorize;
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

    /// Override a variable: --var key=value (repeatable)
    #[arg(long = "var", short = 'e')]
    pub vars: Vec<String>,

    /// Run only tasks matching these tags (comma-separated)
    #[arg(long)]
    pub tags: Option<String>,

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
    /// has no effect inside `include:`/`block:`.
    #[arg(long = "start-at-task")]
    pub start_at_task: Option<String>,
}

// ── YAML schema ───────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
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
    /// `loop:`, only the last iteration's value persists.
    #[serde(default)]
    register: Option<String>,
    /// Retry this task up to N times (total attempts = retries + 1) before giving up.
    /// Applies per `loop:` iteration if combined with `loop:`. Ignored in `--dry`.
    #[serde(default)]
    retries: Option<u32>,
    /// Seconds to wait between retry attempts (default 1 if `retries:` is set).
    #[serde(default)]
    delay: Option<u64>,
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
struct SshSpec {
    /// Server profile name (see: tooler server list)
    server: String,
    command: String,
    #[serde(default)]
    sudo: bool,
}

#[derive(Debug, Deserialize)]
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
}

#[derive(Debug, Deserialize)]
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
struct CheckPortSpec {
    host: String,
    port: u16,
    #[serde(default = "default_timeout")]
    timeout: u64,
}

#[derive(Debug, Deserialize)]
struct EnvCheckSpec {
    reference: String,
    #[serde(default = "default_env_target")]
    target: String,
}

#[derive(Debug, Deserialize)]
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
}

/// Poll one of `check_url`/`check_port`/`ssh` (exactly one — validated upfront in
/// `run_task_once`) every `interval` seconds until it succeeds or `timeout` elapses.
/// Distinct from `retries:`, which retries a whole task on *failure*; `wait_for:` is for
/// "keep checking until this becomes true" (e.g. wait for a service to come back up after
/// a restart), so it doesn't log every attempt the way `retries:` does.
#[derive(Debug, Deserialize, Default)]
struct WaitForSpec {
    #[serde(default)]
    check_url: Option<String>,
    #[serde(default)]
    check_port: Option<CheckPortSpec>,
    #[serde(default)]
    ssh: Option<SshSpec>,
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

/// Mostly-static, per-run execution context threaded through the dispatch chain —
/// bundled into one struct because the parameter list (playbook_dir, project_root, dry,
/// quiet, ctx, plus mutable vars/include_stack passed alongside) got too long to stay
/// readable as positional args once `register:`/`include:` needed threading through too.
struct RunEnv<'a> {
    playbook_dir: PathBuf,
    project_root: PathBuf,
    dry: bool,
    quiet: bool,
    /// From `--yes` — auto-confirms every `confirm:` task instead of prompting or (when
    /// `quiet`) failing fast.
    auto_yes: bool,
    /// From `--start-at-task` — set only on the top-level run's own `RunEnv`, never
    /// copied into an `include:`'s `sub_env`, so the skip only ever applies to the
    /// outermost playbook's own task list (see `execute_playbook`).
    start_at: Option<String>,
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
        let path = dir.join(vf);
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("Cannot read vars_files entry: {}", path.display()))?;
        let file_vars: HashMap<String, String> = serde_yaml::from_str(&content)
            .with_context(|| format!("Invalid YAML in vars_files entry: {}", path.display()))?;
        merged.extend(file_vars);
    }
    merged.extend(playbook.vars.clone());
    Ok(merged)
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

    let Some(file) = args.file.as_deref() else {
        return list_playbooks(&playbooks_dir, ctx);
    };

    let file_path = resolve_playbook_file(file, &project_root)?;

    if args.notes {
        return print_notes_only(&file_path, ctx);
    }
    let notes = read_notes(&file_path);

    let playbook_dir = file_path.parent().unwrap_or(Path::new(".")).to_path_buf();

    let content = std::fs::read_to_string(&file_path)
        .with_context(|| format!("Cannot read playbook: {file}"))?;

    let mut playbook: Playbook =
        serde_yaml::from_str(&content).with_context(|| format!("Invalid YAML in {file}"))?;

    // vars_files: (in order) merged under inline vars:, before --var overrides both.
    playbook.vars = load_playbook_vars(&playbook, &playbook_dir)?;

    // Merge CLI --var overrides into playbook vars
    for var in &args.vars {
        if let Some((k, v)) = var.split_once('=') {
            playbook
                .vars
                .insert(k.trim().to_string(), v.trim().to_string());
        } else {
            bail!("--var must be in key=value format, got: '{var}'");
        }
    }

    let tag_filter: Option<Vec<&str>> = args
        .tags
        .as_deref()
        .map(|t| t.split(',').map(str::trim).collect());

    let mut vars = playbook.vars.clone();
    let mut include_stack: Vec<PathBuf> = vec![file_path];
    let env = RunEnv {
        playbook_dir,
        project_root,
        dry: args.dry,
        quiet: ctx.output == OutputFormat::Json,
        auto_yes: args.yes,
        start_at: args.start_at_task.clone(),
        ctx,
    };

    execute_playbook(
        &playbook,
        &tag_filter,
        &notes,
        &mut vars,
        &mut include_stack,
        true,
        &env,
    )
}

// ── Runner ────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct TaskOutcome {
    name: String,
    status: &'static str,
    error: Option<String>,
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

#[allow(clippy::too_many_arguments)]
fn execute_playbook(
    playbook: &Playbook,
    tag_filter: &Option<Vec<&str>>,
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
        .filter(|t| match tag_filter {
            None => true,
            Some(tags) => t.tags.iter().any(|tag| tags.contains(&tag.as_str())),
        })
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
                        println!(
                            "  {} {} — {}",
                            "!".yellow().bold(),
                            "failed (ignored):".yellow(),
                            e
                        );
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
    }

    // Reached only if every regular task above succeeded (or was skipped/ignored) —
    // any unhandled failure already returned or exited above. Handlers use the same
    // failure-reporting shape as a regular task failure, addressed to the handler
    // instead of an indexed task.
    for name in &notified {
        let handler = playbook
            .handlers
            .iter()
            .find(|h| &h.name == name)
            .expect("validated by validate_handlers");

        if !json {
            println!("\n{} [{}]", "HANDLER".bold().magenta(), handler.name.bold());
        }

        match run_task(handler, vars, include_stack, env) {
            Ok(()) => {
                ok += 1;
                outcomes.push(TaskOutcome {
                    name: handler.name.clone(),
                    status: "ok",
                    error: None,
                });
            }
            Err(e) => {
                failed += 1;
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
                    "\n{} failed at handler \"{}\".",
                    "PLAY".bold().red(),
                    handler.name.bold()
                );
                print_recap(ok, failed, skipped);
                bail!("playbook failed");
            }
        }
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

/// A minimal condition language over `render()`-substituted strings: "<a> == <b>",
/// "<a> != <b>", or a bare truthy check. Not a full expression language — matches
/// tooler's existing plain `{{var}}` templating rather than adding a new one.
fn eval_when(expr: &str, vars: &HashMap<String, String>) -> bool {
    let rendered = render(expr, vars);
    let rendered = rendered.trim();
    if let Some((lhs, rhs)) = rendered.split_once("!=") {
        return lhs.trim() != rhs.trim();
    }
    if let Some((lhs, rhs)) = rendered.split_once("==") {
        return lhs.trim() == rhs.trim();
    }
    !rendered.is_empty() && rendered != "false" && rendered != "0"
}

/// Expands `loop:` (if present) into one retried-`run_task_once` call per item, with
/// `{{item}}` added to that iteration's vars. The first failing iteration (after its own
/// retries are exhausted) fails the whole task — remaining items are not attempted. If
/// `register:` is set, only the *last* iteration's captured value persists into the
/// outer `vars` (simplest well-defined rule for a loop+register combination).
fn run_task(
    task: &Task,
    vars: &mut HashMap<String, String>,
    include_stack: &mut Vec<PathBuf>,
    env: &RunEnv,
) -> Result<()> {
    let Some(spec) = &task.loop_spec else {
        return run_task_once_with_retries(task, vars, include_stack, env);
    };
    let items = resolve_loop_items(spec, vars);
    for item in &items {
        let mut loop_vars = vars.clone();
        match item {
            LoopItem::Scalar(s) => {
                loop_vars.insert("item".to_string(), s.clone());
                if !env.quiet {
                    println!("  {} item={}", "→".dimmed(), s.dimmed());
                }
            }
            LoopItem::Map(m) => {
                for (k, v) in m {
                    loop_vars.insert(format!("item.{k}"), v.clone());
                }
                if !env.quiet {
                    let joined = m
                        .iter()
                        .map(|(k, v)| format!("item.{k}={v}"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    println!("  {} {joined}", "→".dimmed());
                }
            }
        }
        run_task_once_with_retries(task, &mut loop_vars, include_stack, env)?;
        if let Some(reg) = &task.register
            && let Some(val) = loop_vars.get(reg)
        {
            vars.insert(reg.clone(), val.clone());
        }
    }
    Ok(())
}

/// Retries a single (non-loop-expanded) task invocation up to `task.retries` extra times,
/// waiting `task.delay` (default 1s) between attempts. A no-op wrapper when `retries:`
/// isn't set (attempts=1) or in `--dry` (nothing ever fails in dry mode, since every
/// action's real work is itself gated on `!env.dry`).
fn run_task_once_with_retries(
    task: &Task,
    vars: &mut HashMap<String, String>,
    include_stack: &mut Vec<PathBuf>,
    env: &RunEnv,
) -> Result<()> {
    let attempts = task.retries.unwrap_or(0) + 1;
    for attempt in 1..=attempts {
        match run_task_once(task, vars, include_stack, env) {
            Ok(()) => return Ok(()),
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
            Err(e) => return Err(e),
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

fn run_task_once(
    task: &Task,
    vars: &mut HashMap<String, String>,
    include_stack: &mut Vec<PathBuf>,
    env: &RunEnv,
) -> Result<()> {
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
            || task.confirm.is_some())
    {
        bail!(
            "register: is not supported for check_url/check_port/env_check/include/assert/block/debug/set_fact/wait_for/confirm tasks"
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
        ]
        .into_iter()
        .filter(|b| *b)
        .count();
        if set_count != 1 {
            bail!(
                "wait_for: needs exactly one of check_url/check_port/ssh, task '{}' has {set_count}",
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
        if !env.quiet {
            println!(
                "  {} {} {}",
                spec.method.to_uppercase().bold(),
                "→".bold(),
                url.dimmed()
            );
        }
        if !env.dry {
            let (body, status) = http_request(spec, &url, vars)?;
            if let Some(reg) = &task.register {
                vars.insert(format!("{reg}.status"), status.to_string());
                vars.insert(reg.clone(), body);
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
        let describe = if let Some(url) = &spec.check_url {
            format!("{} to respond", render(url, vars))
        } else if let Some(port_spec) = &spec.check_port {
            format!(
                "{}:{} to accept connections",
                render(&port_spec.host, vars),
                port_spec.port
            )
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
                project_root: env.project_root.clone(),
                dry: env.dry,
                quiet: env.quiet,
                auto_yes: env.auto_yes,
                start_at: None,
                ctx: env.ctx,
            };
            include_stack.push(include_path);
            let result = execute_playbook(
                &sub_playbook,
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
         report, env_check, ssh, fleet, include, assert, block, debug, confirm, set_fact, \
         sync_db, sync_files)",
        task.name
    );
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
    let env = side.env.as_deref().map(|s| render(s, vars));
    let engine = side.engine.as_deref().map(|s| render(s, vars));
    let host = side.host.as_deref().map(|s| render(s, vars));
    let database = side.database.as_deref().map(|s| render(s, vars));
    let user = side.user.as_deref().map(|s| render(s, vars));
    let password = side.password.as_deref().map(|s| render(s, vars));
    crate::commands::db::resolve_credentials(
        server,
        &crate::commands::db::ConnOpts {
            env: env.as_deref(),
            engine: engine.as_deref(),
            host: host.as_deref(),
            port: side.port,
            database: database.as_deref(),
            user: user.as_deref(),
            password: password.as_deref(),
        },
    )
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
fn http_request(
    spec: &HttpSpec,
    url: &str,
    vars: &HashMap<String, String>,
) -> Result<(String, u16)> {
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
    let resp = req
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
/// a replacement string differs (real value vs. masked). A trailing `| json:<path>` filter
/// (see `split_filter`/`apply_json_filter`) is applied uniformly regardless of `resolve`,
/// so `render_for_display` masks-then-would-filter too, but the mask token `***` never
/// parses as JSON, so a masked secret piped through `| json:...` just stays unresolved —
/// never leaks. Unresolvable tokens (unknown name, bad filter) are left exactly as
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
            Some(path) => apply_json_filter(&v, path),
            None => Some(v),
        });
        out.push_str(&resolved.unwrap_or_else(|| format!("{{{{{inner}}}}}")));
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    out
}

/// Splits a `{{...}}` token's trimmed inner text on an optional trailing `| json:<path>`
/// filter — e.g. `"resp | json:data.id"` -> `("resp", Some("data.id"))`. Only the `json:`
/// filter is recognized; anything else after a `|` is left as part of the token name (so a
/// stray `|` doesn't silently vanish) and will simply fail to resolve like any unknown
/// token.
fn split_filter(inner: &str) -> (&str, Option<&str>) {
    if let Some((token, filter)) = inner.split_once('|') {
        let filter = filter.trim();
        if let Some(path) = filter.strip_prefix("json:") {
            return (token.trim(), Some(path.trim()));
        }
    }
    (inner, None)
}

/// Applies a `json:<path>` filter to `value` (parsed as JSON), walking dot-separated
/// `path` segments, each optionally suffixed with one or more `[N]` array indices (e.g.
/// `data.items[0].title`, `[2]`). A string leaf renders raw (unquoted); any other JSON
/// value (number/bool/object/array/null) renders via its JSON text form. Returns `None`
/// on invalid JSON or a path that doesn't match — `render_with` then leaves the whole
/// `{{...}}` token literal, same as any other unresolvable token.
fn apply_json_filter(value: &str, path: &str) -> Option<String> {
    let root: serde_json::Value = serde_json::from_str(value).ok()?;
    let mut cur = &root;
    for segment in path.split('.') {
        if segment.is_empty() {
            continue;
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
            project_root: PathBuf::from("."),
            dry: true,
            quiet: true,
            auto_yes: false,
            start_at: None,
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
    fn json_filter_extracts_object_field_and_array_index() {
        let v = vars(&[("resp", r#"{"data":{"id":42,"items":["a","b","c"]}}"#)]);
        assert_eq!(render("{{resp | json:data.id}}", &v), "42");
        assert_eq!(render("{{resp | json:data.items[1]}}", &v), "b");
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
            &mut vars,
            &mut include_stack,
            true,
            &env,
        )
        .unwrap_err();
        assert!(err.to_string().contains("nonexistent"), "error was: {err}");
    }
}
