//! The on-disk sidecar files a playbook run touches outside its own YAML: the
//! `--resume` checkpoint (`<file>.state.json`), `state_set:`'s durable
//! `<file>.data.json`, and `single_instance:`'s exclusive `<file>.lock`.
use super::*;
use anyhow::{Result, bail};
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

/// Keeps the OS-backed lock alive for the run. The OS releases it on process exit, including
/// the JSON-mode `std::process::exit(1)` paths and abrupt process termination.
pub(crate) struct LockGuard {
    _lock: crate::atomic_write::FileLock,
}

/// Acquires the `single_instance:` lock for `file_path`. Returns a `LockGuard` that
/// releases it on drop. Fails if a fresh lock (younger than `lock_timeout` seconds) is
/// already held. The OS releases the advisory lock when a dead process exits, avoiding
/// stale-lock takeover and deletion races entirely.
pub(crate) fn acquire_lock(
    file_path: &Path,
    playbook_name: &str,
    _lock_timeout: u64,
) -> Result<LockGuard> {
    let lock_file = lock_path_for(file_path);
    match crate::atomic_write::try_lock(file_path)? {
        Some(lock) => Ok(LockGuard { _lock: lock }),
        None => {
            bail!(
                "'{playbook_name}' is already running — {} is held",
                lock_file.display()
            );
        }
    }
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
pub(crate) fn write_persisted_state(env: &RunEnv, updates: &HashMap<String, String>) -> Result<()> {
    let Some(path) = &env.data_path else {
        return Ok(());
    };
    (|| -> Result<()> {
        let _lock = crate::atomic_write::lock(path)?;
        let mut state = load_persisted_state(path)?;
        state.extend(updates.iter().filter_map(|(k, v)| {
            k.strip_prefix("state.")
                .map(|key| (key.to_string(), v.clone()))
        }));
        let json = serde_json::to_string_pretty(&state)?;
        crate::atomic_write::write(path, json.as_bytes())
            .with_context(|| format!("writing persisted state {}", path.display()))
    })()
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
) -> Result<()> {
    let Some(path) = &env.state_path else {
        return Ok(());
    };
    let checkpoint = PlayCheckpoint {
        playbook: playbook_name.to_string(),
        last_completed_task: last_completed_task.to_string(),
        vars: vars.clone(),
        updated_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
    };
    serde_json::to_string_pretty(&checkpoint)
        .map_err(anyhow::Error::from)
        .and_then(|json| {
            crate::atomic_write::write(path, json.as_bytes())
                .with_context(|| format!("writing checkpoint {}", path.display()))
        })
}
