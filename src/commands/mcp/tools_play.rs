//! MCP tool handlers split out of the original mcp.rs god-file.
use super::*;

#[tool_router(router = tool_router_play, vis = "pub(crate)")]
impl ToolerMcp {
    #[tool(
        description = "Run a named script defined in .tooler.toml",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn tooler_run(
        &self,
        Parameters(args): Parameters<RunMcpArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec!["run".to_string()];
        if let Some(script) = &args.script {
            argv.push(script.clone());
        }
        push_flag(&mut argv, "--dry", args.dry);
        if !args.extra.is_empty() {
            argv.push("--".to_string());
            argv.extend(args.extra.clone());
        }
        self.exec_self(argv, &args.cwd).await
    }

    #[tool(
        description = "Run a YAML playbook (tasks, vars, health checks) or generate a sample. \
                        A confirm: task in the playbook never blocks this tool waiting on \
                        stdin — it fails fast unless yes=true is passed. If a previous call \
                        failed partway through, pass resume=true (optionally with vars \
                        overrides) to continue right after the last completed task instead \
                        of starting over — start_at_task and resume are mutually exclusive. \
                        list_tasks/list_tags/lint inspect a playbook with zero side effects \
                        (no vars_files:/secrets resolution, no connections, nothing run) \
                        instead of executing it — useful before committing to a real run. \
                        schema=true dumps the whole playbook DSL itself as a formal JSON \
                        Schema document and exits — no file needed, for grounding an agent \
                        before it writes or validates a playbook. content=\"<yaml>\" runs \
                        against inline playbook text instead of file (mutually exclusive), \
                        skipping the write-to-disk step — only valid combined with dry/lint/ \
                        list_tasks/list_tags, since a real run still needs a real file",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn tooler_play(
        &self,
        Parameters(args): Parameters<PlayMcpArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec!["play".to_string()];
        let _temp_guard;
        if let Some(content) = &args.content {
            if args.file.is_some() {
                return Err(McpError::invalid_params(
                    "content and file are mutually exclusive",
                    None,
                ));
            }
            if !(args.dry || args.lint || args.list_tasks || args.list_tags || args.explain) {
                return Err(McpError::invalid_params(
                    "content: requires dry=true, lint=true, list_tasks=true, \
                     list_tags=true, or explain=true -- a real run needs a real file",
                    None,
                ));
            }
            let guard = TempPlaybookFile::write(content).map_err(|e| {
                McpError::internal_error(format!("failed to write temp playbook: {e}"), None)
            })?;
            argv.push(guard.0.to_string_lossy().to_string());
            _temp_guard = Some(guard);
        } else {
            if let Some(file) = &args.file {
                argv.push(file.clone());
            }
            _temp_guard = None;
        }
        push_flag(&mut argv, "--dry", args.dry);
        push_repeated(&mut argv, "--var", &args.vars);
        push_opt(&mut argv, "--tags", &args.tags);
        push_opt(&mut argv, "--skip-tags", &args.skip_tags);
        push_flag(&mut argv, "--init", args.init);
        push_flag(&mut argv, "--yes", args.yes);
        push_flag(&mut argv, "--notes", args.notes);
        push_opt(&mut argv, "--start-at-task", &args.start_at_task);
        push_flag(&mut argv, "--resume", args.resume);
        push_flag(&mut argv, "--keep-checkpoint", args.keep_checkpoint);
        push_repeated(&mut argv, "--vars-file", &args.vars_file);
        push_opt(&mut argv, "--audit-log", &args.audit_log);
        push_flag(&mut argv, "--diff", args.diff);
        push_flag(&mut argv, "--list-tasks", args.list_tasks);
        push_flag(&mut argv, "--list-tags", args.list_tags);
        push_flag(&mut argv, "--lint", args.lint);
        push_flag(&mut argv, "--schema", args.schema);
        push_flag(&mut argv, "--explain", args.explain);
        self.exec_self(argv, &args.cwd).await
    }

    #[tool(
        description = "Open an incremental playbook session: creates a private temp \
                        playbook file and returns a session_id (its path) to pass to \
                        tooler_play_repl_exec/tooler_play_repl_close. Lets an agent run \
                        one task at a time against a vars map that persists across \
                        separate tool calls -- the MCP-callable equivalent of \
                        tooler play --repl, which is otherwise interactive-only (built \
                        around a real terminal, not a request/response call). Not safe \
                        to call tooler_play_repl_exec concurrently on the same \
                        session_id -- one call at a time per session.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn tooler_play_repl_open(
        &self,
        Parameters(args): Parameters<PlayReplOpenArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut vars = std::collections::HashMap::new();
        crate::commands::play::apply_var_overrides(&mut vars, &args.vars)
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;

        let mut doc = serde_yaml::Mapping::new();
        doc.insert(
            serde_yaml::Value::String("name".to_string()),
            serde_yaml::Value::String(args.name.unwrap_or_else(|| "MCP session".to_string())),
        );
        if !vars.is_empty() {
            let mut vars_map = serde_yaml::Mapping::new();
            for (k, v) in vars {
                vars_map.insert(serde_yaml::Value::String(k), serde_yaml::Value::String(v));
            }
            doc.insert(
                serde_yaml::Value::String("vars".to_string()),
                serde_yaml::Value::Mapping(vars_map),
            );
        }
        doc.insert(
            serde_yaml::Value::String("tasks".to_string()),
            serde_yaml::Value::Sequence(Vec::new()),
        );

        let path = new_session_path();
        let content = serde_yaml::to_string(&serde_yaml::Value::Mapping(doc)).map_err(|e| {
            McpError::internal_error(format!("failed to build session playbook: {e}"), None)
        })?;
        std::fs::write(&path, content).map_err(|e| {
            McpError::internal_error(format!("failed to write session file: {e}"), None)
        })?;

        Ok(CallToolResult::success(vec![ContentBlock::text(
            serde_json::json!({"session_id": path.to_string_lossy()}).to_string(),
        )]))
    }

    #[tool(
        description = "Run one task inside an already-open incremental playbook session \
                        (see tooler_play_repl_open) -- appends task to the session's \
                        playbook and runs only that new task, with every var the \
                        session has registered/set so far already restored. Returns the \
                        same JSON shape tooler_play itself returns, scoped to just this \
                        call's task. If the previous task in this session failed, this \
                        call's task REPLACES it instead of appending after it -- a \
                        failed task otherwise blocks every later call, since it's \
                        retried on every attempt until something in its place succeeds. \
                        Not safe to call twice concurrently on the same session_id.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn tooler_play_repl_exec(
        &self,
        Parameters(args): Parameters<PlayReplExecArgs>,
    ) -> Result<CallToolResult, McpError> {
        let path = validate_session_path(&args.session_id)?;
        let existing = std::fs::read_to_string(&path).map_err(|e| {
            McpError::invalid_params(format!("session not found (already closed?): {e}"), None)
        })?;
        let mut doc: serde_yaml::Value = serde_yaml::from_str(&existing)
            .map_err(|e| McpError::internal_error(format!("session file is corrupt: {e}"), None))?;

        let mut task_value: serde_yaml::Value = serde_yaml::from_str(&args.task)
            .map_err(|e| McpError::invalid_params(format!("invalid task YAML: {e}"), None))?;
        if !task_value.is_mapping() {
            return Err(McpError::invalid_params(
                "task must be a mapping, e.g. \"run: echo hi\" or \
                 \"{http: {url: ...}, register: x}\"",
                None,
            ));
        }

        // A checkpoint's last_completed_task lags the file's actual last task whenever
        // the most recent attempt failed (or was never reached) -- that task is stuck
        // at the end of the file, and every future --resume would just retry it again
        // rather than run this new one. Replace it in place instead of appending after
        // it -- the same fix a human editing the file by hand before re-running
        // --resume would make.
        let checkpoint_path = crate::commands::play::state_path_for(&path);
        let checkpoint_exists = checkpoint_path.exists();
        let last_completed = crate::commands::play::load_checkpoint(&checkpoint_path)
            .ok()
            .map(|c| c.last_completed_task);

        let tasks = doc
            .get_mut("tasks")
            .and_then(|t| t.as_sequence_mut())
            .ok_or_else(|| McpError::internal_error("session file has no tasks: list", None))?;
        let last_task_name = tasks
            .last()
            .and_then(|t| t.get("name"))
            .and_then(|n| n.as_str())
            .map(str::to_string);
        let stuck = match (&last_completed, &last_task_name) {
            (Some(completed), Some(last)) => completed != last,
            (None, Some(_)) => true,
            _ => false,
        };

        let n = if stuck { tasks.len() } else { tasks.len() + 1 };
        crate::commands::play::merge_repl_name(&mut task_value, &format!("session-{n}"));
        if stuck {
            *tasks.last_mut().expect("stuck implies at least one task") = task_value;
        } else {
            tasks.push(task_value);
        }

        let new_content = serde_yaml::to_string(&doc).map_err(|e| {
            McpError::internal_error(format!("failed to serialize session playbook: {e}"), None)
        })?;
        std::fs::write(&path, new_content).map_err(|e| {
            McpError::internal_error(format!("failed to write session file: {e}"), None)
        })?;

        // --resume only once a checkpoint actually exists -- the session's first exec
        // call has none yet, and runs fresh (still --keep-checkpoint, so this first
        // task's checkpoint survives for the *next* call to --resume from).
        let mut argv = vec!["play".to_string(), path.to_string_lossy().to_string()];
        if checkpoint_exists {
            argv.push("--resume".to_string());
        }
        argv.push("--keep-checkpoint".to_string());
        self.exec_self(argv, &args.cwd).await
    }

    #[tool(
        description = "Close an incremental playbook session (see \
                        tooler_play_repl_open), deleting its temp playbook file and \
                        checkpoint. Pass save_as to copy the session's accumulated \
                        tasks to a real, permanent playbook file first -- the same as \
                        .save in the interactive REPL. Safe to call on an \
                        already-closed or unknown session_id (reported, not an error).",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn tooler_play_repl_close(
        &self,
        Parameters(args): Parameters<PlayReplCloseArgs>,
    ) -> Result<CallToolResult, McpError> {
        let path = validate_session_path(&args.session_id)?;
        if !path.exists() {
            return Ok(CallToolResult::success(vec![ContentBlock::text(
                "session already closed (or never existed)".to_string(),
            )]));
        }

        let mut saved_to = None;
        if let Some(save_as) = &args.save_as {
            let dest = match &args.cwd {
                Some(dir) => std::path::Path::new(dir).join(save_as),
                None => PathBuf::from(save_as),
            };
            std::fs::copy(&path, &dest).map_err(|e| {
                McpError::internal_error(format!("failed to save session playbook: {e}"), None)
            })?;
            saved_to = Some(dest.to_string_lossy().to_string());
        }

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(crate::commands::play::state_path_for(&path));

        Ok(CallToolResult::success(vec![ContentBlock::text(
            serde_json::json!({"closed": true, "saved_to": saved_to}).to_string(),
        )]))
    }

    #[tool(
        description = "List currently open incremental playbook sessions (see \
                        tooler_play_repl_open) -- every session file still on disk, \
                        with its session_id, playbook name, and how many tasks it has \
                        so far. For recovering a session_id lost from context, or \
                        auditing for abandoned sessions before the OS's own \
                        temp-directory policy eventually cleans them up (this tool \
                        only lists -- it never deletes).",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn tooler_play_repl_list(
        &self,
        Parameters(_args): Parameters<PlayReplListArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut sessions = Vec::new();
        if let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) {
            for entry in entries.flatten() {
                let path = entry.path();
                let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                // A session's own checkpoint file (`<session>.yml.state.json`, written
                // by --keep-checkpoint) sits in the same temp dir and also starts with
                // SESSION_FILE_PREFIX -- excluded by requiring the real ".yml" playbook
                // extension too, or it would show up as a second, phantom session.
                if !name.starts_with(SESSION_FILE_PREFIX) || !name.ends_with(".yml") {
                    continue;
                }
                let (playbook_name, task_count) = std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|s| serde_yaml::from_str::<serde_yaml::Value>(&s).ok())
                    .map(|doc| {
                        let name = doc
                            .get("name")
                            .and_then(|n| n.as_str())
                            .unwrap_or("")
                            .to_string();
                        let count = doc
                            .get("tasks")
                            .and_then(|t| t.as_sequence())
                            .map(Vec::len)
                            .unwrap_or(0);
                        (name, count)
                    })
                    .unwrap_or_default();
                sessions.push(serde_json::json!({
                    "session_id": path.to_string_lossy(),
                    "playbook_name": playbook_name,
                    "task_count": task_count,
                }));
            }
        }
        Ok(CallToolResult::success(vec![ContentBlock::text(
            serde_json::json!({"sessions": sessions}).to_string(),
        )]))
    }
}
