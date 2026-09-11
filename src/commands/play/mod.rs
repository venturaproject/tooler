//! `tooler play` — the YAML playbook DSL, its execution engine, `--lint`/`--explain`
//! static analysis, the chainable-filter template renderer, the incremental REPL, and
//! checkpoint/state/lock persistence. Split into submodules along the seams the file's
//! own `// ── Section ──` banners already marked out; see each submodule's own doc
//! comment for what it owns. Every submodule's items are `pub(crate)` and re-exported
//! here (`pub(crate) use dsl::*;` etc.) so `crate::commands::play::X` keeps resolving
//! exactly like it did when this was one file — no other module in the crate needed to
//! change for this split.
use crate::{context::Context, output::OutputFormat, project, report};
use anyhow::{Context as _, Result, bail};
use clap::Args;
use colored::Colorize;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

mod actions;
mod dispatch;
mod dsl;
mod exec;
mod filters;
mod lint;
mod mail;
mod repl;
mod state;
#[cfg(test)]
mod tests;

pub(crate) use actions::*;
pub(crate) use dispatch::*;
pub(crate) use dsl::*;
pub(crate) use exec::*;
pub(crate) use filters::*;
pub(crate) use lint::*;
pub(crate) use mail::*;
pub(crate) use repl::*;
pub(crate) use state::*;

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

    /// Skip deleting the --resume checkpoint after a fully-successful run. Meaningful
    /// combined with --resume (to keep the ability to resume again later) or on a fresh
    /// run (to pre-seed a checkpoint a future run can --resume from) -- e.g. an
    /// MCP-driven session that appends and runs one task at a time, each call keeping
    /// the checkpoint alive for the next. Ignored in --dry, same as checkpointing itself.
    #[arg(long = "keep-checkpoint")]
    pub keep_checkpoint: bool,

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

    /// Static analysis, zero side effects (same parsing as --list-tasks): warns about a
    /// registered http:/scrape:/db_query:/mail_check: result reaching run:/ssh:/fleet:
    /// without a | quote filter (possible shell injection), and a {{var}} reference that
    /// nothing earlier in the playbook defines (a likely typo). Heuristic, not a formal
    /// verifier -- see the README for its documented false-positive cases. Always exits
    /// 0; findings are advisory.
    #[arg(long)]
    pub lint: bool,

    /// Dump the whole playbook DSL (every task action's fields, and their types) as a
    /// formal JSON Schema document to stdout, and exit. Needs no FILE and no project --
    /// unlike --list-tasks/--lint, this describes the *language*, not one playbook. For
    /// an agent about to write or validate a playbook, or any other schema-aware
    /// tooling.
    #[arg(long)]
    pub schema: bool,

    /// Resolve every task's {{vars}}/{{state.*}} and print the concrete action each one
    /// *would* take -- the exact command line, SQL, URL, target server, file path -- as
    /// JSON, without running anything or opening a connection. Between --dry (shape) and
    /// --lint (problems): "show me exactly what will happen". A `register:`ed value from
    /// an earlier task isn't known statically, so it stays as the literal `{{name}}`.
    /// Always exits 0.
    #[arg(long)]
    pub explain: bool,
}
pub(crate) fn resolve_playbook_file(file: &str, project_root: &Path) -> Result<PathBuf> {
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
pub(crate) fn notes_path(file_path: &Path) -> PathBuf {
    file_path.with_extension("md")
}

pub(crate) fn read_notes(file_path: &Path) -> Option<String> {
    std::fs::read_to_string(notes_path(file_path)).ok()
}

pub(crate) fn print_notes_only(file_path: &Path, ctx: &Context) -> Result<()> {
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
pub(crate) struct TaskListEntry {
    pub(crate) name: String,
    pub(crate) action: &'static str,
    pub(crate) tags: Vec<String>,
    /// An `include:` task's target file — shown, but not recursed into (a separate file,
    /// possibly not resolvable without the full project context `--list-tasks` deliberately
    /// skips).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) include: Option<String>,
    /// `Some(true)`/`Some(false)` for a destructive action (whether its YAML already
    /// has `confirm: true`); `None` for every other task. See `confirm_gate`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) confirmed: Option<bool>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) block: Vec<TaskListEntry>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) rescue: Vec<TaskListEntry>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) always: Vec<TaskListEntry>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) parallel: Vec<TaskListEntry>,
}

/// Builds the `--list-tasks` tree from a task list, recursing into `block:`/`rescue:`/
/// `always:` (which are real tasks of this same playbook) but not `include:` (a separate
/// file — see `TaskListEntry::include`). Generic over the iterator so both a filtered
/// `&[&Task]` (the top-level call) and an owned `&[Task]` (`block:`'s own `Vec<Task>`)
/// work without cloning a `Task`.
pub(crate) fn build_task_list<'a>(tasks: impl IntoIterator<Item = &'a Task>) -> Vec<TaskListEntry> {
    tasks
        .into_iter()
        .map(|t| TaskListEntry {
            name: t.name.clone(),
            action: task_action_label(t),
            tags: t.tags.clone(),
            include: t.include.as_ref().map(|s| s.file().to_string()),
            confirmed: confirm_gate(t),
            block: t.block.as_deref().map(build_task_list).unwrap_or_default(),
            rescue: t.rescue.as_deref().map(build_task_list).unwrap_or_default(),
            always: t.always.as_deref().map(build_task_list).unwrap_or_default(),
            parallel: t
                .parallel
                .as_deref()
                .map(build_task_list)
                .unwrap_or_default(),
        })
        .collect()
}

/// `--list-tasks`: prints the playbook's task tree (name, action, tags, and nested
/// `block:`/`rescue:`/`always:`) with zero side effects — no `vars_files:`/secrets
/// resolution, no connections, no execution. `tasks` is already `--tags`/`--skip-tags`
/// filtered (see `task_matches_tags`) so the listing matches what a real run would
/// attempt.
pub(crate) fn print_task_list(playbook_name: &str, tasks: &[&Task], ctx: &Context) -> Result<()> {
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
            // Only the risky case (destructive, but confirm: true isn't set) gets a
            // visible marker -- silence means safe, same "don't clutter the common
            // case" convention --lint's findings already use. A destructive task that
            // already has confirm: true, or a non-destructive task, prints nothing extra.
            if e.confirmed == Some(false) {
                line.push_str(&format!(" {}", "[needs confirm:]".red().bold()));
            }
            println!("{line}");
            for (label, sub) in [
                ("block:", &e.block),
                ("rescue:", &e.rescue),
                ("always:", &e.always),
                ("parallel:", &e.parallel),
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
pub(crate) fn collect_tags<'a>(
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
        if let Some(p) = &t.parallel {
            collect_tags(p, out);
        }
    }
}

pub(crate) fn print_tag_list(tasks: &[&Task], ctx: &Context) -> Result<()> {
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
pub(crate) struct RunEnv<'a> {
    pub(crate) playbook_dir: PathBuf,
    /// This playbook's `name:` (the outer one's, for `--repl` without a file: `"repl"`).
    /// Copied into an `include:`'s `sub_env` as *that* sub-playbook's own name — unlike
    /// `start_at`/`state_path`/`data_path`, an `--audit-log` entry for a task inside an
    /// `include:` should say which playbook it actually belongs to. See
    /// `write_audit_entry`.
    pub(crate) playbook_name: String,
    /// From `--audit-log`/`TOOLER_PLAY_AUDIT_LOG` — appends one JSON line per task
    /// attempt to this file if set (see `write_audit_entry`). Propagated into `include:`'s
    /// `sub_env` (like `dry`/`quiet`/`auto_yes`), so nested tasks are captured in the same
    /// trail.
    pub(crate) audit_log: Option<PathBuf>,
    pub(crate) project_root: PathBuf,
    pub(crate) dry: bool,
    /// From `--diff` — print a unified diff of what fs_write:/write_file: are about to
    /// change, right before each one applies its write. Propagated into `include:`'s
    /// `sub_env` (like `dry`/`quiet`/`auto_yes`).
    pub(crate) diff: bool,
    pub(crate) quiet: bool,
    /// From `--yes` — auto-confirms every `confirm:` task instead of prompting or (when
    /// `quiet`) failing fast.
    pub(crate) auto_yes: bool,
    /// From `--start-at-task` — set only on the top-level run's own `RunEnv`, never
    /// copied into an `include:`'s `sub_env`, so the skip only ever applies to the
    /// outermost playbook's own task list (see `execute_playbook`). `--resume` also goes
    /// through this same field — `run()` resolves it to a concrete task name upfront.
    pub(crate) start_at: Option<String>,
    /// Where to write/read this playbook's `--resume` checkpoint (`<file>.state.json`).
    /// `Some(...)` only on the top-level run's own `RunEnv`, `None` for `include:`'s
    /// `sub_env` — checkpointing, like `start_at`, is a top-level-only concept.
    pub(crate) state_path: Option<PathBuf>,
    /// Where `state_set:` persists `{{state.*}}` values (`<file>.data.json`). `Some(...)`
    /// only on the top-level run's own `RunEnv`, same top-level-only scoping `state_path`
    /// has and for the same reason — an `include:`'s `sub_env` shares the outer
    /// playbook's `vars` map already, so its `state.*` vars flow through for free with
    /// no extra plumbing; only the on-disk *persistence* is a top-level concept.
    pub(crate) data_path: Option<PathBuf>,
    /// From `--keep-checkpoint` — skip deleting `state_path` after a fully-successful
    /// run. Only ever matters when `state_path.is_some()`, so same top-level-only scope
    /// as `state_path`/`start_at`/`data_path`.
    pub(crate) keep_checkpoint: bool,
    /// The `single_instance:` lock file (`<file>.lock`) to clean up right before the two
    /// JSON-mode `std::process::exit(1)` paths, which bypass `LockGuard`'s `Drop`.
    /// `Some(...)` only when the top-level playbook set `single_instance: true` and this
    /// isn't a `--dry` run; `None` otherwise (and always for an `include:`'s `sub_env`).
    pub(crate) lock_path: Option<PathBuf>,
    pub(crate) ctx: &'a Context,
}

/// Merges a playbook's own `vars_files:` (in order — a later file overrides an earlier
/// one) with its inline `vars:` (which wins over all of them) into one map — the
/// playbook's own baseline vars, before any `--var`/include-time override is layered on
/// top. `dir` is the directory `vars_files:` paths resolve relative to (the playbook's
/// own directory, same as `run:`/`env_check:` paths). Used both for the top-level
/// playbook in `run()` and for an `include:`d sub-playbook.
pub(crate) fn load_playbook_vars(
    playbook: &Playbook,
    dir: &Path,
) -> Result<HashMap<String, String>> {
    let mut merged: HashMap<String, String> = HashMap::new();
    // params: defaults sit at the very bottom — vars_files:, vars:, and any --var still
    // override them.
    for (name, p) in &playbook.params {
        if let Some(d) = p.default_as_string() {
            merged.insert(name.clone(), d);
        }
    }
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
pub(crate) fn load_vars_file(path: &Path) -> Result<HashMap<String, String>> {
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
pub(crate) fn apply_vars_file_overrides(
    vars: &mut HashMap<String, String>,
    paths: &[PathBuf],
) -> Result<()> {
    for path in paths {
        vars.extend(load_vars_file(path)?);
    }
    Ok(())
}

/// Applies `--var key=value` CLI overrides (repeatable) on top of `vars`, in order —
/// shared by a normal run, a `--resume`d run, and `--repl`.
pub(crate) fn apply_var_overrides(
    vars: &mut HashMap<String, String>,
    raw: &[String],
) -> Result<()> {
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
    // Describes the DSL itself, not a playbook -- runs before project::load() (unlike
    // every other early-exit flag below) so it works with no project and no FILE, the
    // same way --help would.
    if args.schema {
        let schema = schemars::schema_for!(Playbook);
        println!("{}", serde_json::to_string_pretty(&schema)?);
        return Ok(());
    }

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
            keep_checkpoint: args.keep_checkpoint,
            lock_path: None,
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

    if args.lint {
        let content = std::fs::read_to_string(&file_path)
            .with_context(|| format!("Cannot read playbook: {file}"))?;
        let playbook: Playbook =
            serde_yaml::from_str(&content).with_context(|| format!("Invalid YAML in {file}"))?;
        let playbook_dir = file_path.parent().unwrap_or(Path::new(".")).to_path_buf();
        let findings = lint_playbook(&playbook, &playbook_dir, &project_root, ctx);
        return print_lint_findings(&playbook.name, &findings, ctx);
    }

    if args.explain {
        let content = std::fs::read_to_string(&file_path)
            .with_context(|| format!("Cannot read playbook: {file}"))?;
        let playbook: Playbook =
            serde_yaml::from_str(&content).with_context(|| format!("Invalid YAML in {file}"))?;
        let playbook_dir = file_path.parent().unwrap_or(Path::new(".")).to_path_buf();
        let mut vars = load_playbook_vars(&playbook, &playbook_dir)?;
        apply_vars_file_overrides(&mut vars, &args.vars_file)?;
        apply_var_overrides(&mut vars, &args.vars)?;
        for (k, v) in load_persisted_state(&data_path_for(&file_path))? {
            vars.insert(format!("state.{k}"), v);
        }
        return print_explain(&playbook, &playbook_dir, &project_root, &mut vars, ctx);
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

    // single_instance: take the lock before any task runs. Real runs only — a --dry
    // inspection or a run of a playbook that didn't opt in never touches the lock. The
    // guard releases it on every return/panic from here on; env.lock_path covers the two
    // JSON-mode process::exit(1) paths that skip Drop.
    let (lock_guard, lock_path) = if playbook.single_instance && !args.dry {
        let guard = acquire_lock(&file_path, &playbook.name, playbook.lock_timeout)?;
        let p = guard.0.clone();
        (Some(guard), Some(p))
    } else {
        (None, None)
    };

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
        keep_checkpoint: args.keep_checkpoint,
        lock_path,
        ctx,
    };

    let result = execute_playbook(
        &playbook,
        &tag_filter,
        &skip_tag_filter,
        &notes,
        &mut vars,
        &mut include_stack,
        true,
        &env,
    );
    drop(lock_guard);
    result
}
// ── Sample playbook ───────────────────────────────────────────────────────────

pub(crate) fn write_sample(path: &Path, ctx: &Context) -> Result<()> {
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
pub(crate) struct PlaybookHeader {
    #[serde(default)]
    pub(crate) description: Option<String>,
}

pub(crate) fn list_playbooks(dir: &Path, ctx: &Context) -> Result<()> {
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
