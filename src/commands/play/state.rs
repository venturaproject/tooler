//! The on-disk sidecar files a playbook run touches outside its own YAML: the
//! `--resume` checkpoint (`<file>.state.json`), `state_set:`'s durable
//! `<file>.data.json`, and `single_instance:`'s exclusive `<file>.lock`.
use super::*;
use anyhow::{Result, anyhow, bail};
use colored::Colorize;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// `--resume`'s on-disk checkpoint — a sibling of the playbook file (`<file>.state.json`,
/// see `state_path_for`), written after every top-level task's non-fatal outcome and
/// deleted on full success. `vars` is the *entire* vars map at that point, which can
/// include values resolved from `{{secret.*}}` (e.g. via `set_fact:`) — see
/// `write_checkpoint`'s 0600-permission handling.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct PlayCheckpoint {
    pub(crate) playbook: String,
    pub(crate) last_completed_task: String,
    pub(crate) vars: HashMap<String, String>,
    pub(crate) updated_at: String,
}

/// The `--resume` checkpoint path for a given playbook file: the file's own path with
/// `.state.json` appended (e.g. `playbooks/deploy.yml` -> `playbooks/deploy.yml.state.json`).
pub(crate) fn state_path_for(file_path: &Path) -> PathBuf {
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
pub(crate) fn data_path_for(file_path: &Path) -> PathBuf {
    let mut s = file_path.as_os_str().to_os_string();
    s.push(".data.json");
    PathBuf::from(s)
}

/// The `single_instance:` lock path for a playbook file: `<file>.lock`, a sibling of the
/// `.state.json`/`.data.json` files. Gitignore `*.lock` alongside those.
pub(crate) fn lock_path_for(file_path: &Path) -> PathBuf {
    let mut s = file_path.as_os_str().to_os_string();
    s.push(".lock");
    PathBuf::from(s)
}

/// What a `<file>.lock` holds — one JSON line, so a human (or a `--dry` inspection) can
/// see which process claims the playbook and since when.
#[derive(Serialize, Deserialize)]
pub(crate) struct LockInfo {
    pub(crate) pid: u32,
    pub(crate) started_at: String,
    pub(crate) playbook: String,
    pub(crate) host: String,
}

/// Removes the lock file it owns on drop — covers a clean finish, any `Err` return, and
/// a panic. The two JSON-mode `std::process::exit(1)` paths bypass `Drop`, so they clean
/// `env.lock_path` explicitly; a hard kill is covered by `lock_timeout` staleness.
pub(crate) struct LockGuard(pub(crate) PathBuf);

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub(crate) fn best_effort_hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Acquires the `single_instance:` lock for `file_path`. Returns a `LockGuard` that
/// releases it on drop. Fails if a fresh lock (younger than `lock_timeout` seconds) is
/// already held; takes over — with a warning — a lock older than that (the previous run
/// was killed without cleaning up) or one that's corrupt/unparseable.
pub(crate) fn acquire_lock(
    file_path: &Path,
    playbook_name: &str,
    lock_timeout: u64,
) -> Result<LockGuard> {
    use std::io::Write;
    let path = lock_path_for(file_path);

    for attempt in 0..2 {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut f) => {
                let info = LockInfo {
                    pid: std::process::id(),
                    started_at: chrono::Utc::now().to_rfc3339(),
                    playbook: playbook_name.to_string(),
                    host: best_effort_hostname(),
                };
                let _ = writeln!(f, "{}", serde_json::to_string(&info).unwrap_or_default());
                return Ok(LockGuard(path));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let held = std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|s| serde_json::from_str::<LockInfo>(s.trim()).ok());
                let stale = match &held {
                    Some(info) => chrono::DateTime::parse_from_rfc3339(&info.started_at)
                        .map(|t| {
                            (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_seconds()
                                > lock_timeout as i64
                        })
                        .unwrap_or(true),
                    None => true,
                };
                if !stale {
                    let info = held.expect("non-stale implies parsed");
                    bail!(
                        "'{playbook_name}' is already running (pid {}, on {}, since {}) — \
                         {} is held. If that run is dead, delete the lock file.",
                        info.pid,
                        info.host,
                        info.started_at,
                        path.display()
                    );
                }
                eprintln!(
                    "  {} {} looks stale{} — taking over",
                    "!".yellow().bold(),
                    path.display(),
                    held.map(|i| format!(" (pid {}, since {})", i.pid, i.started_at))
                        .unwrap_or_else(|| " (unparseable)".to_string())
                );
                let _ = std::fs::remove_file(&path);
                if attempt == 1 {
                    bail!(
                        "could not acquire {} after taking over a stale lock",
                        path.display()
                    );
                }
            }
            Err(e) => {
                return Err(anyhow!("creating lock file {}: {e}", path.display()));
            }
        }
    }
    unreachable!("loop returns or bails within 2 attempts")
}

/// Loads `<file>.data.json` if it exists (a missing file is an empty map, not an error --
/// the common case on a playbook's first run) into a flat, unprefixed `HashMap`. Callers
/// insert each entry into `vars` under a `state.<key>` prefix so `{{state.<key>}}`
/// resolves through the ordinary `vars.get(token)` branch of `resolve_token` -- no
/// changes needed to `render`/`resolve_token` themselves.
pub(crate) fn load_persisted_state(path: &Path) -> Result<HashMap<String, String>> {
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
pub(crate) fn write_persisted_state(env: &RunEnv, vars: &HashMap<String, String>) {
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

pub(crate) fn load_checkpoint(path: &Path) -> Result<PlayCheckpoint> {
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
pub(crate) fn write_checkpoint(
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
