//! `tooler play --repl`: type one task action at a time against a `vars` map that
//! persists for the session, with `rustyline`-backed history/completion and `.save`ing
//! the session back out as a real playbook file.
use super::*;
use anyhow::Result;
use colored::Colorize;
use rustyline::{Editor, error::ReadlineError, history::DefaultHistory};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

// ── REPL ──────────────────────────────────────────────────────────────────────

/// Inserts `name: "<name>"` into `value` (must already be a `Mapping`) if it doesn't
/// already have a `name` key — the synthetic name every `--repl` line gets so it can
/// deserialize into `Task` (whose `name` field is required) without the user typing one.
pub(crate) fn merge_repl_name(value: &mut serde_yaml::Value, name: &str) {
    if let serde_yaml::Value::Mapping(map) = value {
        let key = serde_yaml::Value::String("name".to_string());
        if !map.contains_key(&key) {
            map.insert(key, serde_yaml::Value::String(name.to_string()));
        }
    }
}

pub(crate) fn print_repl_help() {
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

pub(crate) fn print_repl_vars(vars: &HashMap<String, String>) {
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
pub(crate) fn save_repl_session(
    session: &[serde_yaml::Value],
    playbook_dir: &Path,
    arg: &str,
) -> Result<()> {
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
pub(crate) struct ReplHelper;

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
/// `<tooler_dir>/repl_history`, the same directory `config::config_path()`/`TOOLER_HOME`
/// resolve. Always `Some` in practice (`config::tooler_dir()` always resolves to
/// something, falling back to `./.tooler`) — kept as `Option` for callers that already
/// treat "can't persist history" as a non-fatal, current-session-only degradation.
pub(crate) fn repl_history_path() -> Option<PathBuf> {
    Some(crate::config::tooler_dir().join("repl_history"))
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
pub(crate) fn run_repl(mut vars: HashMap<String, String>, env: RunEnv) -> Result<()> {
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
