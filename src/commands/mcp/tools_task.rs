use super::*;

#[tool_router(router = tool_router_task, vis = "pub(crate)")]
impl ToolerMcp {
    #[tool(
        description = "Run or inspect a persistent local Taskfile workflow. Taskfiles provide Make-style dependency graphs and file-output incremental execution. Use plan, graph, list, or history for read-only inspection. Real execution runs local shell commands from an on-disk Taskfile; it never accepts inline YAML.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn tooler_task(
        &self,
        Parameters(args): Parameters<TaskMcpArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut argv = vec!["task".to_string()];
        argv.extend(args.targets.clone());
        push_opt(&mut argv, "--file", &args.file);
        push_flag(&mut argv, "--list", args.list);
        push_flag(&mut argv, "--plan", args.plan);
        push_flag(&mut argv, "--force", args.force);
        push_flag(&mut argv, "--force-deps", args.force_deps);
        push_flag(&mut argv, "--graph", args.graph);
        push_flag(&mut argv, "--dot", args.dot);
        push_flag(&mut argv, "--history", args.history);
        push_opt_num(&mut argv, "--limit", args.limit);
        push_opt(&mut argv, "--schedule", &args.schedule);
        push_opt(&mut argv, "--schedule-name", &args.schedule_name);
        self.exec_self(argv, &args.cwd).await
    }
}
