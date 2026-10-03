//! MCP tool handlers split out of the original mcp.rs god-file.
use super::*;

#[tool_router(router = tool_router_infra, vis = "pub(crate)")]
impl ToolerMcp {
    #[tool(
        description = "Show a systemd unit's status on a remote server over SSH \
                        (systemctl status). `active` in the result reflects the exit code \
                        (0 = active); a non-zero exit (e.g. a stopped or unknown unit) is \
                        returned as informative output, not a tool error.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = true)
    )]
    async fn tooler_systemd_status(
        &self,
        Parameters(args): Parameters<SystemdUnitArgs>,
    ) -> Result<CallToolResult, McpError> {
        let argv = vec![
            "systemd".to_string(),
            "status".to_string(),
            args.server.clone(),
            args.unit.clone(),
        ];
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Restart a systemd unit on a remote server over SSH. A sudo password, if \
                        needed, must never be passed as a tool argument -- set TOOLER_SUDO_PASS \
                        in the MCP server's own environment instead (or rely on \
                        passwordless/NOPASSWD sudo).",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn tooler_systemd_restart(
        &self,
        Parameters(args): Parameters<SystemdRestartArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec![
            "systemd".to_string(),
            "restart".to_string(),
            args.server.clone(),
            args.unit.clone(),
        ];
        push_flag(&mut argv, "--sudo", args.sudo);
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Show recent journal entries for a systemd unit on a remote server \
                        (journalctl -u)",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = true)
    )]
    async fn tooler_systemd_logs(
        &self,
        Parameters(args): Parameters<SystemdLogsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec![
            "systemd".to_string(),
            "logs".to_string(),
            args.server.clone(),
            args.unit.clone(),
        ];
        push_opt_num(&mut argv, "--lines", args.lines);
        push_flag(&mut argv, "--sudo", args.sudo);
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "List a remote server's crontab entries (crontab -l)",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = true)
    )]
    async fn tooler_cron_list(
        &self,
        Parameters(args): Parameters<CronServerArgs>,
    ) -> Result<CallToolResult, McpError> {
        let argv = vec!["cron".to_string(), "list".to_string(), args.server.clone()];
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Append a line to a remote server's crontab",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn tooler_cron_add(
        &self,
        Parameters(args): Parameters<CronAddArgs>,
    ) -> Result<CallToolResult, McpError> {
        let argv = vec![
            "cron".to_string(),
            "add".to_string(),
            args.server.clone(),
            args.line.clone(),
        ];
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Remove crontab lines containing a fixed substring, on a remote server",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn tooler_cron_remove(
        &self,
        Parameters(args): Parameters<CronRemoveArgs>,
    ) -> Result<CallToolResult, McpError> {
        let argv = vec![
            "cron".to_string(),
            "remove".to_string(),
            args.server.clone(),
            args.pattern.clone(),
        ];
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "List this machine's own crontab entries (no SSH -- the machine \
                        the tooler MCP server itself runs on)",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn tooler_cron_local_list(&self) -> Result<CallToolResult, McpError> {
        let argv = vec!["cron".to_string(), "local".to_string(), "list".to_string()];
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Append a line to this machine's own crontab (no SSH) -- e.g. to \
                        schedule a recurring `tooler play` run locally",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn tooler_cron_local_add(
        &self,
        Parameters(args): Parameters<CronLocalAddArgs>,
    ) -> Result<CallToolResult, McpError> {
        let argv = vec![
            "cron".to_string(),
            "local".to_string(),
            "add".to_string(),
            args.line.clone(),
        ];
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Remove crontab lines containing a fixed substring, from this \
                        machine's own crontab (no SSH)",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn tooler_cron_local_remove(
        &self,
        Parameters(args): Parameters<CronLocalRemoveArgs>,
    ) -> Result<CallToolResult, McpError> {
        let argv = vec![
            "cron".to_string(),
            "local".to_string(),
            "remove".to_string(),
            args.pattern.clone(),
        ];
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Show the last N lines of a remote file over SSH (tail -n)",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = true)
    )]
    async fn tooler_logs_tail(
        &self,
        Parameters(args): Parameters<LogsTailArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec![
            "logs".to_string(),
            "tail".to_string(),
            args.server.clone(),
            args.path.clone(),
        ];
        push_opt_num(&mut argv, "--lines", args.lines);
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Search a remote file over SSH for a fixed substring (grep -F)",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = true)
    )]
    async fn tooler_logs_grep(
        &self,
        Parameters(args): Parameters<LogsGrepArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec![
            "logs".to_string(),
            "grep".to_string(),
            args.server.clone(),
            args.path.clone(),
            args.pattern.clone(),
        ];
        push_opt_num(&mut argv, "--max-lines", args.max_lines);
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "List running processes on a remote server over SSH (ps aux), optionally \
                        filtered by a substring of the command line or an exact PID",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = true)
    )]
    async fn tooler_ps_list(
        &self,
        Parameters(args): Parameters<PsListArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec!["ps".to_string(), "list".to_string(), args.server.clone()];
        push_opt(&mut argv, "--filter", &args.filter);
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Send a signal to a process on a remote server over SSH (default: TERM). \
                        Without `confirm`, this only previews what would happen and sends \
                        nothing — pass `confirm: true` to actually apply it. A sudo password, \
                        if needed, must never be passed as a tool argument -- set \
                        TOOLER_SUDO_PASS in the MCP server's own environment instead.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn tooler_ps_kill(
        &self,
        Parameters(args): Parameters<PsKillArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec![
            "ps".to_string(),
            "kill".to_string(),
            args.server.clone(),
            args.pid.to_string(),
        ];
        push_opt(&mut argv, "--signal", &args.signal);
        push_flag(&mut argv, "--sudo", args.sudo);
        push_flag(&mut argv, "--confirm", args.confirm);
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Print a remote file's contents over SSH",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = true)
    )]
    async fn tooler_fs_cat(
        &self,
        Parameters(args): Parameters<FsCatArgs>,
    ) -> Result<CallToolResult, McpError> {
        let argv = vec![
            "fs".to_string(),
            "cat".to_string(),
            args.server.clone(),
            args.path.clone(),
        ];
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Overwrite a remote file over SSH with local content, from either \
                        `from_file` (a local path) or `content` (literal text) -- exactly one \
                        must be set. Without `confirm`, this only previews what would happen \
                        (byte count) and makes no change — pass `confirm: true` to actually \
                        apply it.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn tooler_fs_write(
        &self,
        Parameters(args): Parameters<FsWriteArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec![
            "fs".to_string(),
            "write".to_string(),
            args.server.clone(),
            args.path.clone(),
        ];
        push_opt(&mut argv, "--from-file", &args.from_file);
        push_opt(&mut argv, "--content", &args.content);
        push_flag(&mut argv, "--confirm", args.confirm);
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Diff a remote file against a local file over SSH (unified diff)",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = true)
    )]
    async fn tooler_fs_diff(
        &self,
        Parameters(args): Parameters<FsDiffArgs>,
    ) -> Result<CallToolResult, McpError> {
        let argv = vec![
            "fs".to_string(),
            "diff".to_string(),
            args.server.clone(),
            args.path.clone(),
            "--local".to_string(),
            args.local.clone(),
        ];
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Orchestrate a remote deploy over SSH: optional git pull, optional build \
                        command, optional restart command, then an optional HTTP health check \
                        -- run in that order, failing fast on the first error. Without `confirm`, \
                        this only previews the steps that would run and makes no change — pass \
                        `confirm: true` to actually apply it. A sudo password, if needed, must \
                        never be passed as a tool argument -- set TOOLER_SUDO_PASS in the MCP \
                        server's own environment instead.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn tooler_deploy_run(
        &self,
        Parameters(args): Parameters<DeployRunArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec![
            "deploy".to_string(),
            args.server.clone(),
            "--path".to_string(),
            args.path.clone(),
        ];
        push_flag(&mut argv, "--pull", args.pull);
        push_opt(&mut argv, "--build", &args.build);
        push_opt(&mut argv, "--restart", &args.restart);
        push_opt(&mut argv, "--health-url", &args.health_url);
        push_opt_num(&mut argv, "--health-timeout", args.health_timeout);
        push_opt_num(&mut argv, "--health-retries", args.health_retries);
        push_opt_num(&mut argv, "--health-delay", args.health_delay);
        push_flag(&mut argv, "--rollback-on-failure", args.rollback_on_failure);
        push_flag(&mut argv, "--sudo", args.sudo);
        push_flag(&mut argv, "--confirm", args.confirm);
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Run a command on multiple servers over SSH at once (pass `servers` as a \
                        comma-separated list of profile names, or `all: true` for every \
                        configured profile). Continues past a failing server and reports \
                        per-server results rather than aborting the whole batch.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn tooler_fleet_exec(
        &self,
        Parameters(args): Parameters<FleetExecArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec!["fleet".to_string(), "exec".to_string()];
        push_opt(&mut argv, "--servers", &args.servers);
        push_flag(&mut argv, "--all", args.all);
        push_opt(&mut argv, "--group", &args.group);
        argv.push(args.command.clone());
        push_flag(&mut argv, "--sudo", args.sudo);
        push_flag(&mut argv, "--parallel", args.parallel);
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Check SSH connectivity to multiple servers at once (pass `servers` as a \
                        comma-separated list of profile names, or `all: true` for every \
                        configured profile)",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = true)
    )]
    async fn tooler_fleet_check(
        &self,
        Parameters(args): Parameters<FleetCheckArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec!["fleet".to_string(), "check".to_string()];
        push_opt(&mut argv, "--servers", &args.servers);
        push_flag(&mut argv, "--all", args.all);
        push_opt(&mut argv, "--group", &args.group);
        push_flag(&mut argv, "--parallel", args.parallel);
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Get a resource snapshot (uptime/load average, memory, disk usage) for a \
                        remote server over SSH",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = true)
    )]
    async fn tooler_stat(
        &self,
        Parameters(args): Parameters<StatArgs>,
    ) -> Result<CallToolResult, McpError> {
        let argv = vec!["stat".to_string(), args.server.clone()];
        self.exec_self(argv, &None).await
    }
}
