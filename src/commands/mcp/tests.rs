/// Guards against the CLI and the MCP tool surface drifting apart: every top-level
/// `tooler` command should have at least one `tooler_<command>[_*]` MCP tool, unless
/// explicitly exempted below.
use super::*;

#[test]
fn mcp_policy_limits_commands_and_explicit_server_arguments() {
    let mut mcp = ToolerMcp::new();
    mcp.policy = McpPolicy {
        allowed_commands: vec!["deploy".to_string()],
        allowed_servers: vec!["staging".to_string()],
    };
    assert!(
        mcp.policy_error(&["deploy".to_string(), "staging".to_string()])
            .is_none()
    );
    assert!(
        mcp.policy_error(&["deploy".to_string(), "production".to_string()])
            .unwrap()
            .contains("production")
    );
    assert!(
        mcp.policy_error(&["git".to_string(), "summary".to_string()])
            .unwrap()
            .contains("git")
    );
}

#[test]
fn mcp_does_not_expose_plaintext_local_secrets() {
    let mcp = ToolerMcp::new();
    for argv in [
        vec!["env".into(), "get".into(), "TOKEN".into()],
        vec![
            "env".into(),
            "show".into(),
            ".env".into(),
            "--reveal".into(),
        ],
        vec!["vault".into(), "view".into(), "secrets.yml".into()],
    ] {
        assert!(
            mcp.policy_error(&argv)
                .unwrap()
                .contains("plaintext local secrets")
        );
    }
}

#[test]
fn mail_profiles_are_not_checked_as_ssh_servers() {
    let mut mcp = ToolerMcp::new();
    mcp.policy = McpPolicy {
        allowed_commands: vec!["mail".into()],
        allowed_servers: vec!["staging".into()],
    };
    assert!(
        mcp.policy_error(&[
            "mail".into(),
            "send".into(),
            "--server".into(),
            "smtp".into()
        ])
        .is_none()
    );
}

#[test]
fn mcp_server_allowlist_rejects_unresolvable_or_embedded_targets() {
    let mut mcp = ToolerMcp::new();
    mcp.policy = McpPolicy {
        allowed_commands: vec!["ssh".into(), "fleet".into(), "play".into(), "db".into()],
        allowed_servers: vec!["staging".into()],
    };
    assert!(
        mcp.policy_error(&[
            "ssh".into(),
            "copy".into(),
            "local".into(),
            "staging:/tmp/x".into()
        ])
        .is_none()
    );
    assert!(
        mcp.policy_error(&[
            "ssh".into(),
            "copy".into(),
            "local".into(),
            "production:/tmp/x".into()
        ])
        .unwrap()
        .contains("production")
    );
    assert!(
        mcp.policy_error(&[
            "fleet".into(),
            "exec".into(),
            "--group".into(),
            "web".into()
        ])
        .unwrap()
        .contains("--all/--group")
    );
    assert!(
        mcp.policy_error(&[
            "fleet".into(),
            "exec".into(),
            "--servers".into(),
            "staging,production".into()
        ])
        .unwrap()
        .contains("production")
    );
    assert!(
        mcp.policy_error(&["play".into(), "deploy.yml".into()])
            .unwrap()
            .contains("playbooks")
    );
    assert!(
        mcp.policy_error(&[
            "db".into(),
            "query".into(),
            "production".into(),
            "select 1".into()
        ])
        .unwrap()
        .contains("production")
    );
}

#[test]
fn mcp_server_allowlist_denies_server_profile_mutation() {
    let mut mcp = ToolerMcp::new();
    mcp.policy = McpPolicy {
        allowed_commands: vec!["server".into(), "config".into()],
        allowed_servers: vec!["staging".into()],
    };
    assert!(
        mcp.policy_error(&["server".into(), "add".into(), "staging".into()])
            .unwrap()
            .contains("server-profile mutation")
    );
    assert!(
        mcp.policy_error(&[
            "config".into(),
            "set".into(),
            "server.staging.host".into(),
            "attacker.example".into(),
        ])
        .unwrap()
        .contains("server-profile mutation")
    );
}

#[test]
fn mcp_policy_load_failure_denies_every_command() {
    let mut mcp = ToolerMcp::new();
    mcp.policy_load_error = Some("invalid TOML".into());
    assert!(
        mcp.policy_error(&["deploy".into(), "staging".into()])
            .unwrap()
            .contains("could not be loaded")
    );
}
use clap::CommandFactory;

#[test]
fn every_cli_command_has_a_matching_mcp_tool() {
    // Internal/local-only commands with no MCP tool: `factory` is intentionally run only
    // by the reviewed playbook; `mcp` is the server itself; `completions` and `monitor`
    // are local scheduling concerns; `oauth` requires an interactive browser callback.
    let exempt = ["mcp", "completions", "monitor", "oauth", "factory"];

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
    assert_eq!(redact_argv(&argv)[3], "env=***");
}

#[test]
fn redact_argv_scrubs_sensitive_request_and_execution_content() {
    let argv = vec![
        "http".into(),
        "post".into(),
        "https://example.test".into(),
        "--header".into(),
        "Authorization: Bearer secret".into(),
        "--body".into(),
        "{\"password\":\"secret\"}".into(),
    ];
    assert_eq!(redact_argv(&argv)[4], "***");
    assert_eq!(redact_argv(&argv)[6], "***");
    let db = vec![
        "db".into(),
        "query".into(),
        "staging".into(),
        "select * from users".into(),
    ];
    assert_eq!(redact_argv(&db)[3], "***");
    let file = vec![
        "fs".into(),
        "write".into(),
        "staging".into(),
        "/x".into(),
        "--content".into(),
        "private".into(),
    ];
    assert_eq!(redact_argv(&file)[5], "***");
    let fleet = vec![
        "fleet".into(),
        "exec".into(),
        "--servers".into(),
        "staging".into(),
        "curl -H 'Authorization: secret'".into(),
    ];
    assert_eq!(redact_argv(&fleet)[3], "staging");
    assert_eq!(redact_argv(&fleet)[4], "***");
}

#[test]
fn playbook_arguments_must_match_declared_schema() {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {"environment": {"type": "string", "enum": ["staging"]}, "confirm": {"type": "boolean"}},
        "required": ["environment"]
    });
    let schema = schema.as_object().unwrap();
    assert!(
        validate_playbook_arguments(
            schema,
            serde_json::json!({"environment": "staging"})
                .as_object()
                .unwrap()
        )
        .is_ok()
    );
    assert!(
        validate_playbook_arguments(
            schema,
            serde_json::json!({"environment": "production"})
                .as_object()
                .unwrap()
        )
        .is_err()
    );
    assert!(
        validate_playbook_arguments(
            schema,
            serde_json::json!({"environment": "staging", "extra": true})
                .as_object()
                .unwrap()
        )
        .is_err()
    );
}

#[test]
fn temporary_playbook_files_are_private_and_session_ids_reject_symlinks() {
    let temp = TempPlaybookFile::write("name: test\ntasks: []\n").unwrap();
    assert!(temp.path().is_file());
    #[cfg(unix)]
    assert_eq!(
        std::os::unix::fs::MetadataExt::mode(&std::fs::metadata(temp.path()).unwrap()) & 0o077,
        0
    );
    let session = create_session_file("name: test\ntasks: []\n").unwrap();
    assert!(validate_session_path(session.to_str().unwrap()).is_ok());
    let link = std::env::temp_dir().join(format!("{SESSION_FILE_PREFIX}test-link.yml"));
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&session, &link).unwrap();
        assert!(validate_session_path(link.to_str().unwrap()).is_err());
        std::fs::remove_file(&link).unwrap();
    }
    std::fs::remove_file(session).unwrap();
}
