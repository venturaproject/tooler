/// Guards against the CLI and the MCP tool surface drifting apart: every top-level
/// `tooler` command should have at least one `tooler_<command>[_*]` MCP tool, unless
/// explicitly exempted below.
use super::*;
use clap::CommandFactory;

#[test]
fn every_cli_command_has_a_matching_mcp_tool() {
    // Commands with no MCP tool on purpose: `mcp` is the server itself, and
    // `completions` (shell completion scripts) has no meaningful use from an LLM caller.
    let exempt = ["mcp", "completions"];

    let cli = crate::cli::Cli::command();
    let tool_names: Vec<String> = ToolerMcp::new()
        .tool_router
        .list_all()
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();

    for sub in cli.get_subcommands() {
        let name = sub.get_name();
        if exempt.contains(&name) {
            continue;
        }
        let prefix = format!("tooler_{name}");
        let has_match = tool_names
            .iter()
            .any(|t| *t == prefix || t.starts_with(&format!("{prefix}_")));
        assert!(
            has_match,
            "CLI command `{name}` has no matching MCP tool (expected `{prefix}` or `{prefix}_*`) \
                 -- add one in src/commands/mcp.rs, or add `{name}` to the `exempt` list above"
        );
    }
}

/// Guards against `tooler play`'s CLI flags and its `tooler_play` MCP tool's
/// arguments drifting apart the way they already did once (7 flags --
/// --skip-tags/--vars-file/--audit-log/--diff/--list-tasks/--list-tags/--lint --
/// landed on the CLI across several rounds with nobody updating `PlayMcpArgs` to
/// match, making every one of them unreachable from an agent driving `tooler` over
/// MCP instead of the raw CLI).
#[test]
fn every_play_cli_flag_has_a_matching_playmcpargs_field() {
    // Flags with no meaningful single-call MCP equivalent (--repl is an interactive
    // session), or that are global/handled elsewhere (--output/--profile).
    let exempt = ["repl", "output", "profile"];

    let cli = crate::cli::Cli::command();
    let play = cli.find_subcommand("play").expect("play subcommand exists");
    let mcp_fields = [
        "file",
        "dry",
        "var",
        "tags",
        "skip_tags",
        "init",
        "notes",
        "yes",
        "start_at_task",
        "resume",
        "keep_checkpoint",
        "vars_file",
        "audit_log",
        "diff",
        "list_tasks",
        "list_tags",
        "lint",
        "schema",
        "explain",
        "cwd",
    ];

    for arg in play.get_arguments() {
        let Some(long) = arg.get_long() else {
            continue;
        };
        if exempt.contains(&long) {
            continue;
        }
        let field = long.replace('-', "_");
        assert!(
            mcp_fields.contains(&field.as_str()),
            "tooler play --{long} has no matching field on PlayMcpArgs (tooler_play \
                 MCP tool) -- add one in src/commands/mcp.rs, or add \"{long}\" to the \
                 exempt list above"
        );
    }
}

#[test]
fn redact_argv_scrubs_a_secret_backed_config_set_value() {
    let argv = vec![
        "config".to_string(),
        "set".to_string(),
        "mail.notify.password".to_string(),
        "hunter2".to_string(),
    ];
    let redacted = redact_argv(&argv);
    assert_eq!(redacted[3], "***");
}

#[test]
fn redact_argv_leaves_a_plain_config_set_alone() {
    let argv = vec![
        "config".to_string(),
        "set".to_string(),
        "default.output".to_string(),
        "json".to_string(),
    ];
    assert_eq!(redact_argv(&argv), argv);
}

#[test]
fn redact_argv_scrubs_a_credential_shaped_var_value() {
    let argv = vec![
        "play".to_string(),
        "deploy.yml".to_string(),
        "--var".to_string(),
        "db_password=hunter2".to_string(),
        "-e".to_string(),
        "api_key=abc123".to_string(),
    ];
    let redacted = redact_argv(&argv);
    assert_eq!(redacted[3], "db_password=***");
    assert_eq!(redacted[5], "api_key=***");
}

#[test]
fn redact_argv_leaves_an_ordinary_var_alone() {
    let argv = vec![
        "play".to_string(),
        "deploy.yml".to_string(),
        "--var".to_string(),
        "env=prod".to_string(),
    ];
    assert_eq!(redact_argv(&argv), argv);
}
