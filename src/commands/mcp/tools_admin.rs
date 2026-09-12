//! MCP tool handlers split out of the original mcp.rs god-file.
use super::*;

#[tool_router(router = tool_router_admin, vis = "pub(crate)")]
impl ToolerMcp {
    #[tool(
        description = "List configured server groups (named sets of server profiles used by \
                        tooler_fleet_exec/check and playbook ssh:/fleet: tasks)",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn tooler_group_list(&self) -> Result<CallToolResult, McpError> {
        self.exec_self(vec!["group".to_string(), "list".to_string()], &None)
            .await
    }

    #[tool(
        description = "Add or update a server group. All members must already exist as server \
                        profiles (see tooler_server_add)",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn tooler_group_add(
        &self,
        Parameters(args): Parameters<GroupAddArgs>,
    ) -> Result<CallToolResult, McpError> {
        let argv = vec![
            "group".to_string(),
            "add".to_string(),
            args.name.clone(),
            "--members".to_string(),
            args.members.join(","),
        ];
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Show the members of a server group",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn tooler_group_show(
        &self,
        Parameters(args): Parameters<GroupNameArgs>,
    ) -> Result<CallToolResult, McpError> {
        let argv = vec!["group".to_string(), "show".to_string(), args.name.clone()];
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Remove a server group",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn tooler_group_remove(
        &self,
        Parameters(args): Parameters<GroupNameArgs>,
    ) -> Result<CallToolResult, McpError> {
        let argv = vec!["group".to_string(), "remove".to_string(), args.name.clone()];
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "List configured server profiles (host, user, SSH key)",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn tooler_server_list(&self) -> Result<CallToolResult, McpError> {
        self.exec_self(vec!["server".to_string(), "list".to_string()], &None)
            .await
    }

    #[tool(
        description = "Add or update a server profile",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn tooler_server_add(
        &self,
        Parameters(args): Parameters<ServerAddArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec![
            "server".to_string(),
            "add".to_string(),
            args.name.clone(),
            "--host".to_string(),
            args.host.clone(),
        ];
        push_opt(&mut argv, "--user", &args.user);
        push_opt_num(&mut argv, "--port", args.port);
        push_opt(&mut argv, "--key", &args.key);
        push_opt(&mut argv, "--ssl-dir", &args.ssl_dir);
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Show details of a server profile",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn tooler_server_show(
        &self,
        Parameters(args): Parameters<ServerNameArgs>,
    ) -> Result<CallToolResult, McpError> {
        let argv = vec!["server".to_string(), "show".to_string(), args.name.clone()];
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Remove a server profile",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn tooler_server_remove(
        &self,
        Parameters(args): Parameters<ServerNameArgs>,
    ) -> Result<CallToolResult, McpError> {
        let argv = vec![
            "server".to_string(),
            "remove".to_string(),
            args.name.clone(),
        ];
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Test SSH connectivity to a configured server",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = true)
    )]
    async fn tooler_ssh_check(
        &self,
        Parameters(args): Parameters<SshCheckArgs>,
    ) -> Result<CallToolResult, McpError> {
        let argv = vec!["ssh".to_string(), "check".to_string(), args.server.clone()];
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Execute a command on a remote server over SSH",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn tooler_ssh_exec(
        &self,
        Parameters(args): Parameters<SshExecArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec![
            "ssh".to_string(),
            "exec".to_string(),
            args.server.clone(),
            args.command.clone(),
        ];
        push_flag(&mut argv, "--sudo", args.sudo);
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Upload a local file to a remote server via scp",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn tooler_ssh_copy(
        &self,
        Parameters(args): Parameters<SshCopyArgs>,
    ) -> Result<CallToolResult, McpError> {
        let argv = vec![
            "ssh".to_string(),
            "copy".to_string(),
            args.local.clone(),
            args.remote.clone(),
        ];
        self.exec_self(argv, &None).await
    }

    #[tool(
        description = "Deploy SSL certificates to a server and reload nginx. PFX/sudo passwords \
                        are never passed as tool arguments -- set TOOLER_PFX_PASS / \
                        TOOLER_SUDO_PASS in the MCP server's own environment (e.g. in .mcp.json's \
                        \"env\" block) and they'll be picked up automatically.",
        annotations(
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn tooler_ssh_ssl(
        &self,
        Parameters(args): Parameters<SshSslArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec![
            "ssh".to_string(),
            "ssl".to_string(),
            args.server.clone(),
            "--pfx".to_string(),
            args.pfx.clone(),
            "--key".to_string(),
            args.key.clone(),
        ];
        push_opt(&mut argv, "--remote-dir", &args.remote_dir);
        push_opt(&mut argv, "--cert-name", &args.cert_name);
        push_opt(&mut argv, "--key-name", &args.key_name);
        self.exec_self(argv, &None).await
    }
}
