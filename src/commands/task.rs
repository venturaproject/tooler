//! A small, local Make/Task-style workflow runner. It intentionally does not share the
//! operational semantics of `play`: task outputs are filesystem artifacts and are the
//! source of truth for incremental execution.
use crate::{atomic_write, commands::shell, context::Context, output::OutputFormat};
use anyhow::{Context as _, Result, bail};
use clap::{Args, ValueEnum};
use glob::glob;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Args)]
pub struct TaskArgs {
    /// Targets to run (defaults to `default`)
    pub targets: Vec<String>,
    /// Taskfile path (defaults to Taskfile.yml or Taskfile.yaml)
    #[arg(short, long)]
    pub file: Option<PathBuf>,
    /// List declared targets
    #[arg(long)]
    pub list: bool,
    /// Show the resolved DAG and freshness without executing
    #[arg(long, visible_alias = "dry")]
    pub plan: bool,
    /// Run requested targets even when their outputs are current
    #[arg(long)]
    pub force: bool,
    /// Also force every dependency of requested targets
    #[arg(long)]
    pub force_deps: bool,
    /// Compare content fingerprints instead of relying on modification times
    #[arg(long)]
    pub hash: bool,
    /// Print the dependency graph instead of running
    #[arg(long)]
    pub graph: bool,
    /// Print a DOT graph (implies --graph)
    #[arg(long)]
    pub dot: bool,
    /// Show recorded workflow runs
    #[arg(long)]
    pub history: bool,
    /// Maximum history records to show
    #[arg(long, default_value_t = 20)]
    pub limit: usize,
    /// Manage declared local cron schedules
    #[arg(long, value_enum)]
    pub schedule: Option<ScheduleAction>,
    /// Schedule name for --schedule install/remove
    #[arg(long)]
    pub schedule_name: Option<String>,
}

#[derive(Clone, ValueEnum)]
pub enum ScheduleAction {
    List,
    Install,
    Remove,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Taskfile {
    version: u8,
    #[serde(default)]
    name: Option<String>,
    #[serde(default = "default_single_instance")]
    single_instance: bool,
    #[serde(default = "default_lock_timeout")]
    lock_timeout: u64,
    targets: BTreeMap<String, Target>,
    #[serde(default)]
    schedules: BTreeMap<String, Schedule>,
}
fn default_lock_timeout() -> u64 {
    7200
}
fn default_single_instance() -> bool {
    true
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Target {
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    deps: Vec<String>,
    #[serde(default)]
    inputs: Vec<String>,
    #[serde(default)]
    outputs: Vec<String>,
    #[serde(default)]
    phony: bool,
    #[serde(default)]
    cwd: Option<PathBuf>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    command: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Schedule {
    cron: String,
    target: String,
}

#[derive(Serialize)]
struct TargetResult {
    name: String,
    status: String,
    reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_ms: Option<u128>,
}
#[derive(Serialize)]
struct RunResult {
    workflow: String,
    name: Option<String>,
    requested: Vec<String>,
    targets: Vec<TargetResult>,
    success: bool,
}
#[derive(Serialize, Deserialize)]
struct HistoryRecord {
    run_id: String,
    started_at: String,
    finished_at: String,
    requested: Vec<String>,
    targets: Vec<HistoryTarget>,
    success: bool,
    duration_ms: u128,
}
#[derive(Serialize, Deserialize)]
struct HistoryTarget {
    name: String,
    status: String,
    reason: String,
    duration_ms: Option<u128>,
}

pub fn run(args: TaskArgs, ctx: &Context) -> Result<()> {
    let path = discover(args.file)?;
    let canonical = path
        .canonicalize()
        .with_context(|| format!("resolving {}", path.display()))?;
    let root = canonical
        .parent()
        .expect("taskfile has parent")
        .to_path_buf();
    let content = fs::read_to_string(&canonical)
        .with_context(|| format!("reading {}", canonical.display()))?;
    let file: Taskfile = serde_yaml::from_str(&content)
        .with_context(|| format!("parsing {}", canonical.display()))?;
    validate(&file)?;

    if args.list {
        return list(&file, ctx);
    }
    if let Some(action) = args.schedule {
        return schedule(
            action,
            args.schedule_name.as_deref(),
            &file,
            &canonical,
            ctx,
        );
    }
    if args.history {
        return history(&root, args.limit, ctx);
    }
    let requested = if args.targets.is_empty() {
        if !file.targets.contains_key("default") {
            return list(&file, ctx);
        }
        vec!["default".to_string()]
    } else {
        args.targets.clone()
    };
    let plan = build_plan(&file, &requested)?;
    if args.graph || args.dot {
        return print_graph(&file, &plan, args.dot, ctx);
    }
    execute(
        &file,
        Execution {
            taskfile: &canonical,
            root: &root,
            requested,
            plan,
            dry: args.plan,
            force: args.force,
            force_deps: args.force_deps,
            hash: args.hash,
        },
        ctx,
    )
}

fn discover(given: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = given {
        return Ok(path);
    }
    ["Taskfile.yml", "Taskfile.yaml"]
        .into_iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
        .ok_or_else(|| anyhow::anyhow!("no Taskfile.yml found (use --file PATH)"))
}

fn validate(file: &Taskfile) -> Result<()> {
    if file.version != 1 {
        bail!("Taskfile version must be 1");
    }
    if file.targets.is_empty() {
        bail!("Taskfile must declare at least one target");
    }
    let mut owners = HashMap::new();
    for (name, target) in &file.targets {
        if target.command.trim().is_empty() {
            bail!("target '{name}' has an empty command");
        }
        if !target.phony && target.outputs.is_empty() {
            bail!("target '{name}' needs outputs or phony: true");
        }
        for dep in &target.deps {
            if !file.targets.contains_key(dep) {
                bail!("target '{name}' depends on unknown target '{dep}'");
            }
            if dep == name {
                bail!("target '{name}' cannot depend on itself");
            }
        }
        for output in &target.outputs {
            if let Some(previous) = owners.insert(output, name) {
                bail!("output '{output}' is declared by both '{previous}' and '{name}'");
            }
        }
    }
    for (name, schedule) in &file.schedules {
        if !file.targets.contains_key(&schedule.target) {
            bail!(
                "schedule '{name}' targets unknown target '{}'",
                schedule.target
            );
        }
        validate_cron(&schedule.cron)?;
    }
    Ok(())
}

fn validate_cron(value: &str) -> Result<()> {
    let fields: Vec<_> = value.split_whitespace().collect();
    if fields.len() != 5 {
        bail!("cron schedule '{value}' must have five fields");
    }
    for (field, min, max, names) in [
        (fields[0], 0, 59, &[][..]),
        (fields[1], 0, 23, &[][..]),
        (fields[2], 1, 31, &[][..]),
        (
            fields[3],
            1,
            12,
            &[
                "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
            ][..],
        ),
        (
            fields[4],
            0,
            7,
            &["sun", "mon", "tue", "wed", "thu", "fri", "sat"][..],
        ),
    ] {
        for part in field.split(',') {
            let (base, step) = part
                .split_once('/')
                .map_or((part, None), |(base, step)| (base, Some(step)));
            if step.is_some_and(|step| step.parse::<u32>().ok().filter(|n| *n > 0).is_none()) {
                bail!("invalid cron field '{field}' in '{value}'");
            }
            let parse_value = |raw: &str| -> Option<u32> {
                raw.parse().ok().or_else(|| {
                    names
                        .iter()
                        .position(|name| raw.eq_ignore_ascii_case(name))
                        .map(|index| index as u32 + min)
                })
            };
            if base == "*" {
                continue;
            }
            let valid = if let Some((start, end)) = base.split_once('-') {
                matches!((parse_value(start), parse_value(end)), (Some(start), Some(end)) if start >= min && end <= max && start <= end)
            } else {
                matches!(parse_value(base), Some(number) if number >= min && number <= max)
            };
            if !valid {
                bail!("invalid cron field '{field}' in '{value}'");
            }
        }
    }
    Ok(())
}

fn build_plan(file: &Taskfile, requested: &[String]) -> Result<Vec<String>> {
    let mut plan = Vec::new();
    let mut visiting = BTreeSet::new();
    let mut done = BTreeSet::new();
    fn visit(
        name: &str,
        file: &Taskfile,
        plan: &mut Vec<String>,
        visiting: &mut BTreeSet<String>,
        done: &mut BTreeSet<String>,
    ) -> Result<()> {
        if done.contains(name) {
            return Ok(());
        }
        if !visiting.insert(name.to_string()) {
            bail!("dependency cycle includes target '{name}'");
        }
        let target = file
            .targets
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("unknown target '{name}'"))?;
        for dep in &target.deps {
            visit(dep, file, plan, visiting, done)?;
        }
        visiting.remove(name);
        done.insert(name.to_string());
        plan.push(name.to_string());
        Ok(())
    }
    for target in requested {
        visit(target, file, &mut plan, &mut visiting, &mut done)?;
    }
    Ok(plan)
}

fn resolve_patterns(root: &Path, patterns: &[String]) -> Result<Vec<PathBuf>> {
    let mut paths = BTreeSet::new();
    for pattern in patterns {
        let full = root.join(pattern).to_string_lossy().into_owned();
        for path in glob(&full).with_context(|| format!("invalid path pattern '{pattern}'"))? {
            let path = path?;
            if path.is_file() {
                paths.insert(path);
            }
        }
        if !pattern.contains(['*', '?', '[']) && !root.join(pattern).is_file() {
            bail!(
                "declared input '{}' does not exist",
                root.join(pattern).display()
            );
        }
    }
    Ok(paths.into_iter().collect())
}

#[derive(Default, Serialize, Deserialize)]
struct FingerprintCache {
    targets: HashMap<String, TargetFingerprints>,
}

#[derive(Clone, Serialize, Deserialize)]
struct TargetFingerprints {
    inputs: String,
    outputs: String,
}

fn hash_files(paths: &[PathBuf]) -> Result<String> {
    let mut hasher = Sha256::new();
    for path in paths {
        hasher.update(path.to_string_lossy().as_bytes());
        hasher.update([0]);
        hasher.update(fs::read(path).with_context(|| format!("hashing {}", path.display()))?);
        hasher.update([0]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn input_fingerprint(root: &Path, target: &Target, dep_outputs: &[PathBuf]) -> Result<String> {
    let mut hasher = Sha256::new();
    hasher.update(target.command.as_bytes());
    hasher.update(serde_json::to_vec(&target.env)?);
    hasher.update(serde_json::to_vec(&target.cwd)?);
    hasher.update(serde_json::to_vec(&target.deps)?);
    hasher.update(serde_json::to_vec(&target.inputs)?);
    hasher.update(serde_json::to_vec(&target.outputs)?);
    let mut paths = resolve_patterns(root, &target.inputs)?;
    paths.extend_from_slice(dep_outputs);
    paths.sort();
    paths.dedup();
    for path in paths {
        hasher.update(path.to_string_lossy().as_bytes());
        hasher.update([0]);
        hasher.update(fs::read(&path).with_context(|| format!("hashing {}", path.display()))?);
        hasher.update([0]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn freshness(
    root: &Path,
    name: &str,
    target: &Target,
    dep_outputs: &[PathBuf],
    use_hash: bool,
    cache: &FingerprintCache,
) -> Result<(bool, String)> {
    if target.phony {
        return Ok((false, "phony target".to_string()));
    }
    let outputs: Vec<_> = target.outputs.iter().map(|p| root.join(p)).collect();
    if let Some(missing) = outputs.iter().find(|p| !p.is_file()) {
        return Ok((false, format!("output {} is missing", missing.display())));
    }
    if use_hash {
        let Some(saved) = cache.targets.get(name) else {
            return Ok((false, "no saved content fingerprint".to_string()));
        };
        let current_inputs = input_fingerprint(root, target, dep_outputs)?;
        let current_outputs = hash_files(&outputs)?;
        if saved.inputs != current_inputs {
            return Ok((false, "input content changed".to_string()));
        }
        if saved.outputs != current_outputs {
            return Ok((false, "output content changed".to_string()));
        }
        return Ok((true, "content fingerprints match".to_string()));
    }
    let oldest = outputs
        .iter()
        .map(|p| Ok(fs::metadata(p)?.modified()?))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .min()
        .expect("validated outputs");
    let mut inputs = resolve_patterns(root, &target.inputs)?;
    inputs.extend_from_slice(dep_outputs);
    if let Some(newer) = inputs.iter().find(|p| {
        fs::metadata(p)
            .and_then(|m| m.modified())
            .map(|time| time > oldest)
            .unwrap_or(true)
    }) {
        return Ok((false, format!("{} is newer than outputs", newer.display())));
    }
    Ok((true, "outputs are current".to_string()))
}

struct Execution<'a> {
    taskfile: &'a Path,
    root: &'a Path,
    requested: Vec<String>,
    plan: Vec<String>,
    dry: bool,
    force: bool,
    force_deps: bool,
    hash: bool,
}

fn execute(file: &Taskfile, execution: Execution<'_>, ctx: &Context) -> Result<()> {
    let Execution {
        taskfile,
        root,
        requested,
        plan,
        dry,
        force,
        force_deps,
        hash,
    } = execution;
    let _lock = if file.single_instance {
        Some(acquire_lock(root, file.lock_timeout)?)
    } else {
        None
    };
    let started = chrono::Utc::now();
    let start_clock = std::time::Instant::now();
    let requested_set: BTreeSet<_> = requested.iter().cloned().collect();
    let cache_path = root.join(".tooler/task-fingerprints.json");
    let mut fingerprint_cache = if hash && cache_path.is_file() {
        serde_json::from_slice(&fs::read(&cache_path)?)
            .with_context(|| format!("reading task fingerprints {}", cache_path.display()))?
    } else {
        FingerprintCache::default()
    };
    let mut results = Vec::new();
    let mut output_map: HashMap<String, Vec<PathBuf>> = HashMap::new();
    let mut success = true;
    for name in &plan {
        let target = &file.targets[name];
        let deps: Vec<PathBuf> = target
            .deps
            .iter()
            .flat_map(|d| output_map.get(d).cloned().unwrap_or_default())
            .collect();
        let force_this = (force && requested_set.contains(name)) || force_deps;
        let (current, reason) = freshness(root, name, target, &deps, hash, &fingerprint_cache)?;
        if current && !force_this {
            results.push(TargetResult {
                name: name.clone(),
                status: "up_to_date".into(),
                reason,
                duration_ms: None,
            });
            output_map.insert(
                name.clone(),
                target.outputs.iter().map(|p| root.join(p)).collect(),
            );
            continue;
        }
        if dry {
            results.push(TargetResult {
                name: name.clone(),
                status: "would_run".into(),
                reason,
                duration_ms: None,
            });
            continue;
        }
        if ctx.output != OutputFormat::Json {
            println!("$ {}", target.command);
        }
        let now = std::time::Instant::now();
        let cwd = target
            .cwd
            .as_ref()
            .map(|p| root.join(p))
            .unwrap_or_else(|| root.to_path_buf());
        let mut command: Command = shell::command(&target.command);
        command.current_dir(&cwd).envs(&target.env);
        let status = if ctx.output == OutputFormat::Json {
            command
                .output()
                .with_context(|| format!("running target '{name}'"))?
                .status
        } else {
            command
                .status()
                .with_context(|| format!("running target '{name}'"))?
        };
        let elapsed = now.elapsed().as_millis();
        if !status.success() {
            results.push(TargetResult {
                name: name.clone(),
                status: "failed".into(),
                reason: format!("exit {}", status.code().unwrap_or(1)),
                duration_ms: Some(elapsed),
            });
            success = false;
            break;
        }
        let missing = target
            .outputs
            .iter()
            .map(|p| root.join(p))
            .find(|p| !p.is_file());
        if let Some(missing) = missing {
            results.push(TargetResult {
                name: name.clone(),
                status: "failed".into(),
                reason: format!("expected output {} was not created", missing.display()),
                duration_ms: Some(elapsed),
            });
            success = false;
            break;
        }
        output_map.insert(
            name.clone(),
            target.outputs.iter().map(|p| root.join(p)).collect(),
        );
        if hash {
            let outputs: Vec<_> = target.outputs.iter().map(|p| root.join(p)).collect();
            fingerprint_cache.targets.insert(
                name.clone(),
                TargetFingerprints {
                    inputs: input_fingerprint(root, target, &deps)?,
                    outputs: hash_files(&outputs)?,
                },
            );
            fs::create_dir_all(cache_path.parent().expect("cache path parent"))?;
            atomic_write::write(&cache_path, &serde_json::to_vec_pretty(&fingerprint_cache)?)?;
        }
        results.push(TargetResult {
            name: name.clone(),
            status: "ok".into(),
            reason,
            duration_ms: Some(elapsed),
        });
    }
    let result = RunResult {
        workflow: taskfile.display().to_string(),
        name: file.name.clone(),
        requested: requested.clone(),
        targets: results,
        success,
    };
    if !dry {
        append_history(root, &result, started, start_clock.elapsed())?;
    }
    print_result(&result, ctx);
    if !success {
        bail!("task workflow failed");
    }
    Ok(())
}

fn lock_path(root: &Path) -> PathBuf {
    root.join(".tooler/task")
}
struct TaskLock {
    _lock: crate::atomic_write::FileLock,
}
fn acquire_lock(root: &Path, _timeout: u64) -> Result<TaskLock> {
    let path = lock_path(root);
    fs::create_dir_all(path.parent().expect("parent"))?;
    match atomic_write::try_lock(&path)? {
        Some(lock) => Ok(TaskLock { _lock: lock }),
        None => bail!(
            "task workflow is already running ({})",
            atomic_write::lock_path(&path).display()
        ),
    }
}

fn append_history(
    root: &Path,
    result: &RunResult,
    started: chrono::DateTime<chrono::Utc>,
    elapsed: std::time::Duration,
) -> Result<()> {
    let dir = root.join(".tooler");
    fs::create_dir_all(&dir)?;
    let path = dir.join("task-history.jsonl");
    let _lock = atomic_write::lock(&path)?;
    let mut records = fs::read_to_string(&path).unwrap_or_default();
    let record = HistoryRecord {
        run_id: format!(
            "{}-{}",
            started.format("%Y%m%dT%H%M%SZ"),
            std::process::id()
        ),
        started_at: started.to_rfc3339(),
        finished_at: chrono::Utc::now().to_rfc3339(),
        requested: result.requested.clone(),
        targets: result
            .targets
            .iter()
            .map(|t| HistoryTarget {
                name: t.name.clone(),
                status: t.status.clone(),
                reason: t.reason.clone(),
                duration_ms: t.duration_ms,
            })
            .collect(),
        success: result.success,
        duration_ms: elapsed.as_millis(),
    };
    records.push_str(&serde_json::to_string(&record)?);
    records.push('\n');
    atomic_write::write(&path, records.as_bytes())
}

fn print_result(result: &RunResult, ctx: &Context) {
    if ctx.output == OutputFormat::Json {
        println!("{}", serde_json::to_string(result).unwrap());
        return;
    }
    for target in &result.targets {
        println!("{:16} {} ({})", target.name, target.status, target.reason);
    }
}
fn list(file: &Taskfile, ctx: &Context) -> Result<()> {
    if ctx.output == OutputFormat::Json {
        println!(
            "{}",
            serde_json::json!({"targets": file.targets.keys().collect::<Vec<_>>() })
        );
        return Ok(());
    }
    for (name, target) in &file.targets {
        println!(
            "{:20} {}",
            name,
            target.description.as_deref().unwrap_or("")
        );
    }
    Ok(())
}
fn print_graph(file: &Taskfile, plan: &[String], dot: bool, ctx: &Context) -> Result<()> {
    if dot {
        println!("digraph taskfile {{");
        for name in plan {
            for dep in &file.targets[name].deps {
                println!("  \"{dep}\" -> \"{name}\";");
            }
        }
        println!("}}");
    } else if ctx.output == OutputFormat::Json {
        println!(
            "{}",
            serde_json::json!({"targets": plan, "edges": plan.iter().flat_map(|n| file.targets[n].deps.iter().map(move |d| serde_json::json!([d,n]))).collect::<Vec<_>>() })
        );
    } else {
        for name in plan {
            let deps = &file.targets[name].deps;
            println!(
                "{}{}",
                name,
                if deps.is_empty() {
                    String::new()
                } else {
                    format!(" <- {}", deps.join(", "))
                }
            );
        }
    }
    Ok(())
}
fn history(root: &Path, limit: usize, ctx: &Context) -> Result<()> {
    let path = root.join(".tooler/task-history.jsonl");
    let mut records: Vec<HistoryRecord> = fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    records.reverse();
    records.truncate(limit);
    if ctx.output == OutputFormat::Json {
        println!("{}", serde_json::json!({"runs": records}));
    } else {
        for record in records {
            println!(
                "{} {} {}",
                record.started_at,
                if record.success { "ok" } else { "failed" },
                record.requested.join(",")
            );
        }
    }
    Ok(())
}
fn schedule(
    action: ScheduleAction,
    name: Option<&str>,
    file: &Taskfile,
    taskfile: &Path,
    ctx: &Context,
) -> Result<()> {
    match action {
        ScheduleAction::List => {
            if ctx.output == OutputFormat::Json {
                println!(
                    "{}",
                    serde_json::json!({"schedules": file.schedules.keys().collect::<Vec<_>>() })
                );
            } else {
                for (name, schedule) in &file.schedules {
                    println!("{name:20} {} -> {}", schedule.cron, schedule.target);
                }
            }
            Ok(())
        }
        ScheduleAction::Install | ScheduleAction::Remove => {
            let name = name.ok_or_else(|| anyhow::anyhow!("--schedule-name is required"))?;
            let schedule = file
                .schedules
                .get(name)
                .ok_or_else(|| anyhow::anyhow!("unknown schedule '{name}'"))?;
            let marker = format!("tooler-task:{name}");
            match action {
                ScheduleAction::Install => {
                    let exe = std::env::current_exe()?.canonicalize()?;
                    let root = taskfile.parent().expect("taskfile has parent");
                    let line = format!(
                        "{} cd {} && {} task --file {} {} # {}",
                        schedule.cron,
                        crate::db::shell_quote(&root.display().to_string()),
                        crate::db::shell_quote(&exe.display().to_string()),
                        crate::db::shell_quote(&taskfile.display().to_string()),
                        crate::db::shell_quote(&schedule.target),
                        marker,
                    );
                    crate::commands::cron::replace_local_line(&marker, &line)?;
                    if ctx.output == OutputFormat::Json {
                        println!("{}", serde_json::json!({"installed": name, "line": line}));
                    } else {
                        println!("Installed schedule '{name}'.");
                    }
                }
                ScheduleAction::Remove => {
                    let removed = crate::commands::cron::remove_local_lines(&marker)?;
                    if ctx.output == OutputFormat::Json {
                        println!("{}", serde_json::json!({"removed": removed}));
                    } else {
                        println!("Removed {} schedule line(s).", removed.len());
                    }
                }
                ScheduleAction::List => unreachable!(),
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn task(command: &str, deps: &[&str]) -> Target {
        Target {
            description: None,
            deps: deps.iter().map(|v| v.to_string()).collect(),
            inputs: vec![],
            outputs: vec!["out".into()],
            phony: false,
            cwd: None,
            env: BTreeMap::new(),
            command: command.into(),
        }
    }
    #[test]
    fn plan_orders_dependencies() {
        let mut targets = BTreeMap::new();
        targets.insert("build".into(), task("true", &["generate"]));
        targets.insert("generate".into(), task("true", &[]));
        let file = Taskfile {
            version: 1,
            name: None,
            single_instance: false,
            lock_timeout: 1,
            targets,
            schedules: BTreeMap::new(),
        };
        assert_eq!(
            build_plan(&file, &["build".into()]).unwrap(),
            ["generate", "build"]
        );
    }
    #[test]
    fn plan_rejects_cycles() {
        let mut targets = BTreeMap::new();
        targets.insert("a".into(), task("true", &["b"]));
        targets.insert("b".into(), task("true", &["a"]));
        let file = Taskfile {
            version: 1,
            name: None,
            single_instance: false,
            lock_timeout: 1,
            targets,
            schedules: BTreeMap::new(),
        };
        assert!(build_plan(&file, &["a".into()]).is_err());
    }

    #[test]
    fn cron_validation_rejects_invalid_ranges_and_accepts_standard_syntax() {
        assert!(validate_cron("*/15 9-17 * jan,mar mon-fri").is_ok());
        assert!(validate_cron("60 * * * *").is_err());
        assert!(validate_cron("* * * * nope").is_err());
        assert!(validate_cron("*/0 * * * *").is_err());
    }

    #[test]
    fn history_append_preserves_existing_records() {
        let directory = tempfile::tempdir().unwrap();
        let result = RunResult {
            workflow: "Taskfile.yml".to_string(),
            name: None,
            requested: vec!["default".to_string()],
            targets: vec![],
            success: true,
        };
        let started = chrono::Utc::now();
        append_history(
            directory.path(),
            &result,
            started,
            std::time::Duration::ZERO,
        )
        .unwrap();
        append_history(
            directory.path(),
            &result,
            started,
            std::time::Duration::ZERO,
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(directory.path().join(".tooler/task-history.jsonl"))
                .unwrap()
                .lines()
                .count(),
            2
        );
    }
}
