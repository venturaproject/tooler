//! MCP tool handlers split out of the original mcp.rs god-file.
use super::*;

#[tool_router(router = tool_router_core, vis = "pub(crate)")]
impl ToolerMcp {
    #[tool(
        description = "Show system information: working directory and environment variables",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn tooler_info(
        &self,
        Parameters(args): Parameters<InfoArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec!["info".to_string()];
        push_flag(&mut argv, "--env", args.env);
        push_flag(&mut argv, "--dir", args.dir);
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Run environment/health checks: git, OS keychain, SSH key files, \
                        self-exe resolution, config summary",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn tooler_doctor(&self) -> Result<CallToolResult, McpError> {
        self.exec_self(vec!["doctor".to_string()], &None).await
    }

    #[tool(
        description = "Echo text with optional color/uppercase/repeat formatting",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn tooler_echo(
        &self,
        Parameters(args): Parameters<EchoArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec!["echo".to_string()];
        argv.extend(args.text.clone());
        push_flag(&mut argv, "--upper", args.upper);
        argv.push("--color".to_string());
        argv.push(args.color.clone());
        argv.push("--repeat".to_string());
        argv.push(args.repeat.to_string());
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Pretty-print and query a JSON file by dot-notation key",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn tooler_json(
        &self,
        Parameters(args): Parameters<JsonQueryArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec!["json".to_string(), args.file.clone()];
        push_opt(&mut argv, "--key", &args.key);
        push_flag(&mut argv, "--compact", args.compact);
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Show variables from a .env file (values masked by default)",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn tooler_env_show(
        &self,
        Parameters(args): Parameters<EnvShowArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec!["env".to_string(), "show".to_string()];
        if let Some(f) = &args.file {
            argv.push(f.clone());
        }
        push_flag(&mut argv, "--reveal", args.reveal);
        self.exec_self(argv, &args.cwd).await
    }

    #[tool(
        description = "List variable names in a .env file",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn tooler_env_list(
        &self,
        Parameters(args): Parameters<EnvListArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec!["env".to_string(), "list".to_string()];
        if let Some(f) = &args.file {
            argv.push(f.clone());
        }
        self.exec_self(argv, &args.cwd).await
    }

    #[tool(
        description = "Get a single variable's value from a .env file",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn tooler_env_get(
        &self,
        Parameters(args): Parameters<EnvGetArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec!["env".to_string(), "get".to_string(), args.key.clone()];
        if let Some(f) = &args.file {
            argv.push(f.clone());
        }
        self.exec_self(argv, &args.cwd).await
    }

    #[tool(
        description = "Show keys present in one .env file but missing in the other",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn tooler_env_diff(
        &self,
        Parameters(args): Parameters<EnvDiffArgs>,
    ) -> Result<CallToolResult, McpError> {
        let argv = vec![
            "env".to_string(),
            "diff".to_string(),
            args.file_a.clone(),
            args.file_b.clone(),
        ];
        self.exec_self(argv, &args.cwd).await
    }

    #[tool(
        description = "Verify a .env file has all keys from a reference file",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn tooler_env_check(
        &self,
        Parameters(args): Parameters<EnvCheckArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec![
            "env".to_string(),
            "check".to_string(),
            args.reference.clone(),
        ];
        if let Some(t) = &args.target {
            argv.push(t.clone());
        }
        self.exec_self(argv, &args.cwd).await
    }

    #[tool(
        description = "Perform an HTTP GET request, with optional profile-based auth. Never \
                        accepts a bearer token as a tool argument -- set TOOLER_HTTP_TOKEN in \
                        the MCP server's own environment for ad hoc auth, or use --profile for \
                        a token stored in the OS keychain (only sent to that profile's host).",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = true)
    )]
    async fn tooler_http_get(
        &self,
        Parameters(args): Parameters<HttpGetArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec!["http".to_string(), "get".to_string(), args.url.clone()];
        push_repeated(&mut argv, "--header", &args.headers);
        push_opt_num(&mut argv, "--timeout", args.timeout);
        push_opt(&mut argv, "--profile", &args.profile);
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Perform an HTTP POST request with a JSON body. Never accepts a bearer \
                        token as a tool argument -- set TOOLER_HTTP_TOKEN in the MCP server's \
                        own environment for ad hoc auth, or use --profile for a token stored in \
                        the OS keychain (only sent to that profile's host).",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn tooler_http_post(
        &self,
        Parameters(args): Parameters<HttpPostArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec!["http".to_string(), "post".to_string(), args.url.clone()];
        push_opt(&mut argv, "--body", &args.body);
        push_repeated(&mut argv, "--header", &args.headers);
        push_opt_num(&mut argv, "--timeout", args.timeout);
        push_opt(&mut argv, "--profile", &args.profile);
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Search job listings via Adzuna (defaults: 'desarrollador' roles in \
                        Madrid, Spain -- Spanish keywords match Spain listings much better \
                        than English ones). Never accepts Adzuna credentials as a tool argument -- set \
                        TOOLER_ADZUNA_APP_ID/TOOLER_ADZUNA_APP_KEY in the MCP server's own \
                        environment, or run `tooler jobs configure` on the CLI to store them \
                        in the OS keychain for the active profile (CLI-only, by design).",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = true)
    )]
    async fn tooler_jobs_search(
        &self,
        Parameters(args): Parameters<JobsSearchArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec!["jobs".to_string(), "search".to_string()];
        push_opt(&mut argv, "--what", &args.what);
        push_opt(&mut argv, "--where", &args.r#where);
        push_opt(&mut argv, "--country", &args.country);
        push_opt(&mut argv, "--category", &args.category);
        push_opt(&mut argv, "--exclude", &args.exclude);
        push_opt_num(&mut argv, "--salary-min", args.salary_min);
        push_opt_num(&mut argv, "--max-days-old", args.max_days_old);
        push_opt(&mut argv, "--sort-by", &args.sort_by);
        push_flag(&mut argv, "--title-only", args.title_only.unwrap_or(false));
        push_opt_num(&mut argv, "--page", args.page);
        push_opt_num(&mut argv, "--results", args.results);
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "List valid Adzuna sector/category tags for a country (e.g. it-jobs, \
                        engineering-jobs) -- use a returned tag as `category` in \
                        tooler_jobs_search to filter by sector. Never accepts Adzuna \
                        credentials as a tool argument -- see tooler_jobs_search.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = true)
    )]
    async fn tooler_jobs_categories(
        &self,
        Parameters(args): Parameters<JobsCategoriesArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec!["jobs".to_string(), "categories".to_string()];
        push_opt(&mut argv, "--country", &args.country);
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Check whether a URL returns a 2xx response",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = true)
    )]
    async fn tooler_check_url(
        &self,
        Parameters(args): Parameters<CheckUrlArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec!["check".to_string(), "url".to_string(), args.url.clone()];
        push_opt_num(&mut argv, "--timeout", args.timeout);
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Check whether a TCP port is open on a host",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = true)
    )]
    async fn tooler_check_port(
        &self,
        Parameters(args): Parameters<CheckPortArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec![
            "check".to_string(),
            "port".to_string(),
            args.host.clone(),
            args.port.to_string(),
        ];
        push_opt_num(&mut argv, "--timeout", args.timeout);
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Show tooler's full configuration",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn tooler_config_show(&self) -> Result<CallToolResult, McpError> {
        self.exec_self(vec!["config".to_string(), "show".to_string()], &None)
            .await
    }

    #[tool(
        description = "Get a tooler config value by key, e.g. default.output",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn tooler_config_get(
        &self,
        Parameters(args): Parameters<ConfigGetArgs>,
    ) -> Result<CallToolResult, McpError> {
        if crate::commands::config::is_secret_backed_key(&args.key) {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "Refusing to read a keychain-backed value (profile token/client_secret/\
                 refresh_token, or a mail profile's password) over MCP -- it would end up in \
                 plaintext in the conversation. Run `tooler config get \"..\"` directly in a \
                 terminal instead.",
            )]));
        }
        let argv = vec!["config".to_string(), "get".to_string(), args.key.clone()];
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Set a tooler config value by key, e.g. default.output json. Refuses any \
                        keychain-backed key (profile.<name>.token/client_secret/refresh_token, \
                        mail.<name>.password) -- set those directly in a terminal instead, so \
                        the secret never enters the conversation.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn tooler_config_set(
        &self,
        Parameters(args): Parameters<ConfigSetArgs>,
    ) -> Result<CallToolResult, McpError> {
        if crate::commands::config::is_secret_backed_key(&args.key) {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "Refusing to set a keychain-backed value (profile token/client_secret/\
                 refresh_token, or a mail profile's password) over MCP -- it would sit in \
                 plaintext in the conversation/tool-call history. Run `tooler config set \
                 <key> ..` directly in a terminal instead; it's stored encrypted in the OS \
                 keychain.",
            )]));
        }
        let argv = vec![
            "config".to_string(),
            "set".to_string(),
            args.key.clone(),
            args.value.clone(),
        ];
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "List configured tooler profiles",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn tooler_config_profiles(&self) -> Result<CallToolResult, McpError> {
        self.exec_self(vec!["config".to_string(), "profiles".to_string()], &None)
            .await
    }

    #[tool(
        description = "Print tooler's config file path",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn tooler_config_path(&self) -> Result<CallToolResult, McpError> {
        self.exec_self(vec!["config".to_string(), "path".to_string()], &None)
            .await
    }

    #[tool(
        description = "Unset a tooler config value by key, e.g. profile.staging.token \
                        (safe to use over MCP -- it only removes the stored value)",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn tooler_config_unset(
        &self,
        Parameters(args): Parameters<ConfigUnsetArgs>,
    ) -> Result<CallToolResult, McpError> {
        let argv = vec!["config".to_string(), "unset".to_string(), args.key.clone()];
        self.exec_self(argv, &None).await
    }
}
