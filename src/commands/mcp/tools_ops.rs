//! MCP tool handlers split out of the original mcp.rs god-file.
use super::*;

#[tool_router(router = tool_router_ops, vis = "pub(crate)")]
impl ToolerMcp {
    #[tool(
        description = "Compact git repo summary: branch, tag, status, recent commits",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn tooler_git_summary(
        &self,
        Parameters(args): Parameters<GitCwdArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.exec_self(vec!["git".to_string(), "summary".to_string()], &args.cwd)
            .await
    }

    #[tool(
        description = "Delete branches already merged into the current branch, or (with \
                        after/before) any local branch with a trailing DDMMYY date suffix \
                        in the given range regardless of merge status",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn tooler_git_clean(
        &self,
        Parameters(args): Parameters<GitCleanArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec!["git".to_string(), "clean".to_string()];
        push_flag(&mut argv, "--remote", args.remote);
        push_flag(&mut argv, "--confirm", args.confirm);
        push_opt(&mut argv, "--after", &args.after);
        push_opt(&mut argv, "--before", &args.before);
        self.exec_self(argv, &args.cwd).await
    }

    #[tool(
        description = "Generate a changelog from commits since the last tag",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn tooler_git_changelog(
        &self,
        Parameters(args): Parameters<GitChangelogArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec!["git".to_string(), "changelog".to_string()];
        push_opt(&mut argv, "--from", &args.from);
        self.exec_self(argv, &args.cwd).await
    }

    #[tool(
        description = "List available scaffold templates",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn tooler_scaffold_list(&self) -> Result<CallToolResult, McpError> {
        self.exec_self(vec!["scaffold".to_string(), "list".to_string()], &None)
            .await
    }

    #[tool(
        description = "Create a new project from a scaffold template",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn tooler_scaffold_new(
        &self,
        Parameters(args): Parameters<ScaffoldNewArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec![
            "scaffold".to_string(),
            "new".to_string(),
            args.template.clone(),
            args.name.clone(),
        ];
        push_opt(&mut argv, "--dir", &args.dir);
        self.exec_self(argv, &args.cwd).await
    }

    #[tool(
        description = "Generate a multi-page PDF report (cover page, per-source sections, \
                        tables, and embedded bar charts) from one or more JSON files, typically \
                        the --output json result of another tooler command",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn tooler_report_pdf(
        &self,
        Parameters(args): Parameters<ReportArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec!["report".to_string(), "pdf".to_string()];
        push_repeated(&mut argv, "--in", &args.input);
        argv.push("--out".to_string());
        argv.push(args.out.clone());
        if let Some(title) = &args.title {
            argv.push("--title".to_string());
            argv.push(title.clone());
        }
        self.exec_self(argv, &args.cwd).await
    }

    #[tool(
        description = "Generate a multi-sheet Excel (.xlsx) report (one sheet per source, with \
                        formatted tables and native charts) from one or more JSON files, \
                        typically the --output json result of another tooler command",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn tooler_report_excel(
        &self,
        Parameters(args): Parameters<ReportArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec!["report".to_string(), "excel".to_string()];
        push_repeated(&mut argv, "--in", &args.input);
        argv.push("--out".to_string());
        argv.push(args.out.clone());
        if let Some(title) = &args.title {
            argv.push("--title".to_string());
            argv.push(title.clone());
        }
        self.exec_self(argv, &args.cwd).await
    }

    #[tool(
        description = "Generate a self-contained HTML report (summary fields, tables, and \
                        inline SVG bar charts, no external assets) from one or more JSON \
                        files, typically the --output json result of another tooler command",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn tooler_report_html(
        &self,
        Parameters(args): Parameters<ReportArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec!["report".to_string(), "html".to_string()];
        push_repeated(&mut argv, "--in", &args.input);
        argv.push("--out".to_string());
        argv.push(args.out.clone());
        if let Some(title) = &args.title {
            argv.push("--title".to_string());
            argv.push(title.clone());
        }
        self.exec_self(argv, &args.cwd).await
    }

    #[tool(
        description = "Run a read-only SQL query (SELECT/SHOW/EXPLAIN/WITH/DESCRIBE) against a \
                        remote database by running psql/mysql directly on a server profile over \
                        SSH, returning rows as JSON — feed the result straight into \
                        tooler_report_pdf/excel/html. \
                        Prefer `env` (a remote dotenv-style file, e.g. Laravel .env) to supply \
                        DB_* credentials rather than passing them explicitly; a DB password can \
                        never be passed as a tool argument — set TOOLER_DB_PASSWORD in the \
                        environment the tooler MCP server itself runs in instead.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn tooler_db_query(
        &self,
        Parameters(args): Parameters<DbQueryArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec![
            "db".to_string(),
            "query".to_string(),
            args.server.clone(),
            args.sql.clone(),
        ];
        push_opt(&mut argv, "--env", &args.env);
        push_opt(&mut argv, "--engine", &args.engine);
        push_opt(&mut argv, "--host", &args.host);
        push_opt_num(&mut argv, "--port", args.port);
        push_opt(&mut argv, "--database", &args.database);
        push_opt(&mut argv, "--user", &args.user);
        push_opt_num(&mut argv, "--max-rows", args.max_rows);
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Run a single INSERT/UPDATE/DELETE statement against a remote \
                        database by running psql/mysql directly on a server profile over \
                        SSH -- deliberately narrower than tooler_db_query: no SELECT, no \
                        DDL (no DROP/TRUNCATE/ALTER/CREATE), exactly what marking a row \
                        processed or logging an event needs. Without confirm, this only \
                        previews what would run (the resolved SQL and target database) and \
                        makes no change -- pass confirm: true to actually apply it. Prefer \
                        `env` for credentials; a DB password can never be passed as a tool \
                        argument -- set TOOLER_DB_PASSWORD in the MCP server's own \
                        environment instead.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn tooler_db_exec(
        &self,
        Parameters(args): Parameters<DbExecArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec![
            "db".to_string(),
            "exec".to_string(),
            args.server.clone(),
            args.sql.clone(),
        ];
        push_opt(&mut argv, "--env", &args.env);
        push_opt(&mut argv, "--engine", &args.engine);
        push_opt(&mut argv, "--host", &args.host);
        push_opt_num(&mut argv, "--port", args.port);
        push_opt(&mut argv, "--database", &args.database);
        push_opt(&mut argv, "--user", &args.user);
        push_flag(&mut argv, "--confirm", args.confirm);
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Dump a remote database (pg_dump/mysqldump) over SSH to a local file, \
                        gzip-compressed by default. Prefer `env` (a remote dotenv-style file) \
                        to supply DB_* credentials rather than passing them explicitly; a DB \
                        password can never be passed as a tool argument — set \
                        TOOLER_DB_PASSWORD in the environment the tooler MCP server itself \
                        runs in instead.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn tooler_db_backup(
        &self,
        Parameters(args): Parameters<DbBackupArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec![
            "db".to_string(),
            "backup".to_string(),
            args.server.clone(),
            "--out".to_string(),
            args.out.clone(),
        ];
        push_opt(&mut argv, "--env", &args.env);
        push_opt(&mut argv, "--engine", &args.engine);
        push_opt(&mut argv, "--host", &args.host);
        push_opt_num(&mut argv, "--port", args.port);
        push_opt(&mut argv, "--database", &args.database);
        push_opt(&mut argv, "--user", &args.user);
        push_flag(&mut argv, "--no-gzip", args.no_gzip);
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Restore a local dump file into a remote database (psql/mysql) over SSH. \
                        Without `confirm`, this only previews what would run (byte count, \
                        target database) and makes no change — pass `confirm: true` to actually \
                        apply it. Prefer `env` for credentials; a DB password can never be \
                        passed as a tool argument — set TOOLER_DB_PASSWORD in the MCP server's \
                        own environment instead.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn tooler_db_restore(
        &self,
        Parameters(args): Parameters<DbRestoreArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec![
            "db".to_string(),
            "restore".to_string(),
            args.server.clone(),
            "--in".to_string(),
            args.input.clone(),
        ];
        push_opt(&mut argv, "--env", &args.env);
        push_opt(&mut argv, "--engine", &args.engine);
        push_opt(&mut argv, "--host", &args.host);
        push_opt_num(&mut argv, "--port", args.port);
        push_opt(&mut argv, "--database", &args.database);
        push_opt(&mut argv, "--user", &args.user);
        push_flag(&mut argv, "--confirm", args.confirm);
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Send an email over SMTP through a configured mail profile. A mail \
                        password can never be passed as a tool argument -- set it once with \
                        tooler_config_set (key mail.<name>.password, stored in the OS \
                        keychain) or TOOLER_MAIL_PASSWORD in the MCP server's own \
                        environment, then reference the profile by name here.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn tooler_mail_send(
        &self,
        Parameters(args): Parameters<MailSendArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec![
            "mail".to_string(),
            "send".to_string(),
            "--to".to_string(),
            args.to.clone(),
            "--subject".to_string(),
            args.subject.clone(),
            "--body".to_string(),
            args.body.clone(),
            "--server".to_string(),
            args.server.clone(),
        ];
        push_opt(&mut argv, "--cc", &args.cc);
        push_opt(&mut argv, "--bcc", &args.bcc);
        push_opt(&mut argv, "--from", &args.from);
        push_flag(&mut argv, "--html", args.html);
        for path in &args.attachments {
            argv.push("--attach".to_string());
            argv.push(path.clone());
        }
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Read a mail profile's inbox over IMAP (unseen messages by default). \
                        Only accepts a profile (server) -- never raw host/user/password, \
                        same rule tooler_mail_send follows.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = true)
    )]
    async fn tooler_mail_check(
        &self,
        Parameters(args): Parameters<MailCheckArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec![
            "mail".to_string(),
            "check".to_string(),
            "--server".to_string(),
            args.server.clone(),
            "--folder".to_string(),
            args.folder.clone(),
        ];
        push_flag(&mut argv, "--all", args.all);
        push_flag(&mut argv, "--include-body", args.include_body);
        push_opt_num(&mut argv, "--limit", args.limit);
        push_flag(&mut argv, "--mark-seen", args.mark_seen);
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Encrypt a file in place with a passphrase (AES-256-GCM) -- fails if \
                        it's already vault-encrypted",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn tooler_vault_encrypt(
        &self,
        Parameters(args): Parameters<VaultFileArgs>,
    ) -> Result<CallToolResult, McpError> {
        let argv = vault_argv("encrypt", &args);
        self.exec_self(argv, &args.cwd).await
    }

    #[tool(
        description = "Decrypt a vault-encrypted file in place with a passphrase -- fails if \
                        it isn't vault-encrypted",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn tooler_vault_decrypt(
        &self,
        Parameters(args): Parameters<VaultFileArgs>,
    ) -> Result<CallToolResult, McpError> {
        let argv = vault_argv("decrypt", &args);
        self.exec_self(argv, &args.cwd).await
    }

    #[tool(
        description = "Print a vault-encrypted file's decrypted contents without modifying \
                        it on disk",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn tooler_vault_view(
        &self,
        Parameters(args): Parameters<VaultFileArgs>,
    ) -> Result<CallToolResult, McpError> {
        let argv = vault_argv("view", &args);
        self.exec_self(argv, &args.cwd).await
    }

    #[tool(
        description = "Rotate a vault-encrypted file's passphrase in place -- decrypts with \
                        the old one and re-encrypts with a new one, the plaintext never \
                        touching disk in between. Fails if the file isn't already \
                        vault-encrypted",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn tooler_vault_rekey(
        &self,
        Parameters(args): Parameters<VaultRekeyArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec!["vault".to_string(), "rekey".to_string(), args.file.clone()];
        push_opt(&mut argv, "--old-password-env", &args.old_password_env);
        argv.push("--new-password-env".to_string());
        argv.push(args.new_password_env.clone());
        self.exec_self(argv, &args.cwd).await
    }

    #[tool(
        description = "List pull requests (title, labels, author, dates) via the `gh` CLI, \
                        optionally filtered to a created-date range. Requires `gh` installed \
                        and authenticated in the environment the tooler MCP server runs in. \
                        Feed the JSON result straight into tooler_report_pdf/excel/html.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = true)
    )]
    async fn tooler_gh_prs(
        &self,
        Parameters(args): Parameters<GhPrsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec!["gh".to_string(), "prs".to_string()];
        push_opt(&mut argv, "--repo", &args.repo);
        push_opt(&mut argv, "--after", &args.after);
        push_opt(&mut argv, "--before", &args.before);
        push_opt(&mut argv, "--state", &args.state);
        push_opt_num(&mut argv, "--limit", args.limit);
        self.exec_self(argv, &args.cwd).await
    }
}
