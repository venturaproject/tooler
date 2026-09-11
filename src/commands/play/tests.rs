//! Unit tests for the playbook engine's internals (integration-level behavior is
//! `tests/play_cmd.rs`, which only exercises the compiled binary). Reaches every
//! submodule's `pub(crate)` surface via `super::*`'s glob re-exports.
use super::*;

#[test]
fn is_literal_path_detects_slash_and_extensions() {
    assert!(is_literal_path("playbook.yml"));
    assert!(is_literal_path("playbook.yaml"));
    assert!(is_literal_path("./playbooks/deploy.yml"));
    assert!(is_literal_path("sub/deploy"));
}

#[test]
fn is_literal_path_false_for_bare_names() {
    assert!(!is_literal_path("deploy"));
    assert!(!is_literal_path("smoke-test"));
}

fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[test]
fn eval_when_equality() {
    let v = vars(&[("env", "prod")]);
    assert!(eval_when("{{env}} == prod", &v));
    assert!(!eval_when("{{env}} == staging", &v));
}

#[test]
fn eval_when_inequality() {
    let v = vars(&[("env", "prod")]);
    assert!(eval_when("{{env}} != staging", &v));
    assert!(!eval_when("{{env}} != prod", &v));
}

#[test]
fn eval_when_truthy_bare_value() {
    let v = vars(&[("enabled", "yes")]);
    assert!(eval_when("{{enabled}}", &v));
    let v = vars(&[("enabled", "false")]);
    assert!(!eval_when("{{enabled}}", &v));
    let v = vars(&[("enabled", "")]);
    assert!(!eval_when("{{enabled}}", &v));
}

#[test]
fn eval_when_greater_than() {
    let v = vars(&[("count", "3")]);
    assert!(!eval_when("{{count}} > 5", &v));
    let v = vars(&[("count", "9")]);
    assert!(eval_when("{{count}} > 5", &v));
}

#[test]
fn eval_when_less_than() {
    let v = vars(&[("count", "3")]);
    assert!(eval_when("{{count}} < 5", &v));
    let v = vars(&[("count", "9")]);
    assert!(!eval_when("{{count}} < 5", &v));
}

#[test]
fn eval_when_greater_or_equal() {
    let v = vars(&[("count", "5")]);
    assert!(eval_when("{{count}} >= 5", &v));
    let v = vars(&[("count", "4")]);
    assert!(!eval_when("{{count}} >= 5", &v));
}

#[test]
fn eval_when_less_or_equal() {
    let v = vars(&[("count", "5")]);
    assert!(eval_when("{{count}} <= 5", &v));
    let v = vars(&[("count", "6")]);
    assert!(!eval_when("{{count}} <= 5", &v));
}

#[test]
fn eval_when_comparison_is_false_when_either_side_is_not_numeric() {
    // The exact bug this fix closes: a non-numeric comparison must not silently
    // fall through to "truthy" (which would make it always true).
    let v = vars(&[("count", "not-a-number")]);
    assert!(!eval_when("{{count}} > 5", &v));
    assert!(!eval_when("{{count}} < 5", &v));
    assert!(!eval_when("{{count}} >= 5", &v));
    assert!(!eval_when("{{count}} <= 5", &v));
}

#[test]
fn notes_path_swaps_yml_extension_for_md() {
    assert_eq!(
        notes_path(Path::new("playbooks/deploy.yml")),
        PathBuf::from("playbooks/deploy.md")
    );
    assert_eq!(
        notes_path(Path::new("playbooks/deploy.yaml")),
        PathBuf::from("playbooks/deploy.md")
    );
}

#[test]
fn register_is_rejected_for_check_and_include_actions() {
    let ctx = Context::new(
        OutputFormat::Json,
        "default".to_string(),
        crate::config::Config::default(),
    );
    let env = dry_env(&ctx);
    let mut vars = HashMap::new();
    let mut include_stack = Vec::new();

    let task = Task {
        register: Some("x".to_string()),
        check_url: Some("http://example.com".to_string()),
        ..Default::default()
    };
    assert!(run_task_once(&task, &mut vars, &mut include_stack, &env).is_err());

    let task = Task {
        register: Some("x".to_string()),
        include: Some(IncludeSpec::Simple("sub.yml".to_string())),
        ..Default::default()
    };
    assert!(run_task_once(&task, &mut vars, &mut include_stack, &env).is_err());
}

#[test]
fn register_is_allowed_for_run_action() {
    let ctx = Context::new(
        OutputFormat::Json,
        "default".to_string(),
        crate::config::Config::default(),
    );
    // dry: true — no real subprocess runs, so this only exercises the upfront
    // register-validation guard, not actual command execution.
    let env = dry_env(&ctx);
    let mut vars = HashMap::new();
    let mut include_stack = Vec::new();
    let task = Task {
        register: Some("x".to_string()),
        run: Some(RunSpec::Simple("echo hi".to_string())),
        ..Default::default()
    };
    assert!(run_task_once(&task, &mut vars, &mut include_stack, &env).is_ok());
}

fn dry_env(ctx: &Context) -> RunEnv<'_> {
    RunEnv {
        playbook_dir: PathBuf::from("."),
        playbook_name: "test".to_string(),
        audit_log: None,
        project_root: PathBuf::from("."),
        dry: true,
        diff: false,
        quiet: true,
        auto_yes: false,
        start_at: None,
        state_path: None,
        data_path: None,
        keep_checkpoint: false,
        lock_path: None,
        ctx,
    }
}

#[test]
fn assert_true_succeeds_and_assert_false_fails() {
    let ctx = Context::new(
        OutputFormat::Json,
        "default".to_string(),
        crate::config::Config::default(),
    );
    let env = dry_env(&ctx);
    let mut vars = vars(&[("env", "prod")]);
    let mut include_stack = Vec::new();

    let task = Task {
        assert: Some(AssertSpec::Simple("{{env}} == prod".to_string())),
        ..Default::default()
    };
    assert!(run_task_once(&task, &mut vars, &mut include_stack, &env).is_ok());

    let task = Task {
        assert: Some(AssertSpec::Simple("{{env}} == staging".to_string())),
        ..Default::default()
    };
    assert!(run_task_once(&task, &mut vars, &mut include_stack, &env).is_err());
}

#[test]
fn render_still_substitutes_known_vars() {
    let v = vars(&[("name", "world")]);
    assert_eq!(render("hello {{name}}", &v), "hello world");
}

#[test]
fn render_for_display_masks_secret_tokens_but_not_others() {
    let v = vars(&[("name", "world"), ("env.PATH_LIKE", "unused")]);
    // A secret token is masked for display...
    assert_eq!(
        render_for_display("token={{secret.myprofile.api_key}}", &v),
        "token=***"
    );
    // ...but the real render() still substitutes it for actual execution.
    // (secret.myprofile.api_key isn't stored, so it stays literal here — this
    // just confirms render_for_display's masking is independent of resolve_token.)
    assert_eq!(
        render("token={{secret.myprofile.api_key}}", &v),
        "token={{secret.myprofile.api_key}}"
    );
    // Plain vars are unaffected by render_for_display.
    assert_eq!(render_for_display("hello {{name}}", &v), "hello world");
}

#[test]
fn shell_quote_wraps_a_plain_value() {
    assert_eq!(shell_quote("hello world"), "'hello world'");
}

#[test]
fn shell_quote_escapes_embedded_single_quotes() {
    assert_eq!(shell_quote("it's here"), r"'it'\''s here'");
}

#[test]
fn quote_filter_resolves_a_var() {
    let v = vars(&[("item", "; rm -rf /")]);
    assert_eq!(render("echo {{item | quote}}", &v), "echo '; rm -rf /'");
}

#[test]
fn quote_filter_on_an_unknown_token_stays_literal() {
    let v = vars(&[]);
    assert_eq!(render("{{missing | quote}}", &v), "{{missing | quote}}");
}

#[test]
fn render_for_display_quotes_a_masked_secret_without_leaking() {
    let v = vars(&[]);
    // The real value never resolves (not stored), but the point stands even when
    // it would: render_for_display masks to "***" before the filter runs, so the
    // filtered result is always the masked placeholder, quoted -- never the secret.
    assert_eq!(render_for_display("{{secret.p.k | quote}}", &v), "'***'");
}

#[test]
fn json_filter_extracts_object_field_and_array_index() {
    let v = vars(&[("resp", r#"{"data":{"id":42,"items":["a","b","c"]}}"#)]);
    assert_eq!(render("{{resp | json:data.id}}", &v), "42");
    assert_eq!(render("{{resp | json:data.items[1]}}", &v), "b");
}

#[test]
fn json_filter_length_of_array() {
    let v = vars(&[("resp", r#"["a","b","c"]"#)]);
    assert_eq!(render("{{resp | json:length}}", &v), "3");
}

#[test]
fn json_filter_length_of_object() {
    let v = vars(&[("resp", r#"{"a":1,"b":2}"#)]);
    assert_eq!(render("{{resp | json:length}}", &v), "2");
}

#[test]
fn json_filter_length_of_string() {
    let v = vars(&[("resp", r#""hello""#)]);
    assert_eq!(render("{{resp | json:length}}", &v), "5");
}

#[test]
fn json_filter_length_after_a_nested_path() {
    let v = vars(&[("resp", r#"{"data":{"items":["a","b"]}}"#)]);
    assert_eq!(render("{{resp | json:data.items.length}}", &v), "2");
}

#[test]
fn json_filter_length_on_a_scalar_stays_literal() {
    let v = vars(&[("resp", "42")]);
    assert_eq!(
        render("{{resp | json:length}}", &v),
        "{{resp | json:length}}"
    );
}

#[test]
fn json_filter_stays_literal_on_bad_json_or_missing_path() {
    let v = vars(&[("resp", "not json")]);
    assert_eq!(
        render("{{resp | json:data.id}}", &v),
        "{{resp | json:data.id}}"
    );
    let v = vars(&[("resp", r#"{"data":{}}"#)]);
    assert_eq!(
        render("{{resp | json:data.missing}}", &v),
        "{{resp | json:data.missing}}"
    );
}

#[test]
fn json_filter_on_a_masked_secret_never_resolves() {
    // render_for_display masks {{secret.*}} to "***", which isn't valid JSON — a
    // `| json:` filter piped onto it must stay unresolved, never leak partial data.
    let v = vars(&[]);
    assert_eq!(
        render_for_display("{{secret.p.k | json:token}}", &v),
        "{{secret.p.k | json:token}}"
    );
}

#[test]
fn http_spec_deserializes_with_defaults() {
    let spec: HttpSpec = serde_yaml::from_str("url: https://example.com\n").unwrap();
    assert_eq!(spec.method, "GET");
    assert_eq!(spec.timeout, 5);
    assert!(!spec.ignore_status);
    assert!(spec.headers.is_empty());
}

#[test]
fn http_spec_deserializes_full_fields() {
    let spec: HttpSpec = serde_yaml::from_str(
            "method: POST\nurl: https://example.com\nheaders:\n  X-Test: \"1\"\nbody: '{}'\ntimeout: 10\nignore_status: true\n",
        )
        .unwrap();
    assert_eq!(spec.method, "POST");
    assert_eq!(spec.headers.get("X-Test").map(String::as_str), Some("1"));
    assert_eq!(spec.body.as_deref(), Some("{}"));
    assert_eq!(spec.timeout, 10);
    assert!(spec.ignore_status);
}

#[test]
fn http_spec_deserializes_download_field() {
    let spec: HttpSpec =
        serde_yaml::from_str("url: https://example.com/f.pdf\ndownload: out/f.pdf\n").unwrap();
    assert_eq!(spec.download.as_deref(), Some("out/f.pdf"));
}

#[test]
fn write_file_spec_deserializes_with_default_append() {
    let spec: WriteFileSpec = serde_yaml::from_str("path: out.txt\ncontent: hello\n").unwrap();
    assert_eq!(spec.path, "out.txt");
    assert_eq!(spec.content, "hello");
    assert!(!spec.append);
}

#[test]
fn write_file_spec_deserializes_with_append() {
    let spec: WriteFileSpec =
        serde_yaml::from_str("path: out.txt\ncontent: hello\nappend: true\n").unwrap();
    assert!(spec.append);
}

#[test]
fn read_csv_spec_deserializes_with_defaults() {
    let spec: ReadCsvSpec = serde_yaml::from_str("path: data.csv\n").unwrap();
    assert_eq!(spec.path, "data.csv");
    assert!(spec.headers);
    assert!(spec.delimiter.is_none());
}

#[test]
fn read_csv_spec_deserializes_explicit_fields() {
    let spec: ReadCsvSpec =
        serde_yaml::from_str("path: data.tsv\nheaders: false\ndelimiter: \"\\t\"\n").unwrap();
    assert!(!spec.headers);
    assert_eq!(spec.delimiter.as_deref(), Some("\t"));
}

#[test]
fn write_csv_spec_deserializes_with_defaults() {
    let spec: WriteCsvSpec = serde_yaml::from_str("path: out.csv\ndata: \"{{rows}}\"\n").unwrap();
    assert_eq!(spec.path, "out.csv");
    assert!(spec.headers);
    assert!(spec.delimiter.is_none());
}

#[test]
fn write_csv_spec_deserializes_explicit_fields() {
    let spec: WriteCsvSpec = serde_yaml::from_str(
        "path: out.tsv\ndata: \"{{rows}}\"\nheaders: false\ndelimiter: \"\\t\"\n",
    )
    .unwrap();
    assert!(!spec.headers);
    assert_eq!(spec.delimiter.as_deref(), Some("\t"));
}

#[test]
fn json_cell_to_string_unwraps_strings_and_stringifies_others() {
    assert_eq!(
        json_cell_to_string(&serde_json::Value::String("hi".to_string())),
        "hi"
    );
    assert_eq!(
        json_cell_to_string(&serde_json::Value::Number(3.into())),
        "3"
    );
    assert_eq!(json_cell_to_string(&serde_json::Value::Null), "");
}

#[test]
fn guess_mime_covers_common_extensions_and_falls_back() {
    assert_eq!(guess_mime(Path::new("report.pdf")), "application/pdf");
    assert_eq!(guess_mime(Path::new("data.CSV")), "text/csv");
    assert_eq!(
        guess_mime(Path::new("mystery.xyz")),
        "application/octet-stream"
    );
    assert_eq!(
        guess_mime(Path::new("noextension")),
        "application/octet-stream"
    );
}

#[test]
fn task_action_label_identifies_run_and_debug_and_falls_back_to_unknown() {
    let run_task = Task {
        run: Some(RunSpec::Simple("echo hi".to_string())),
        ..Default::default()
    };
    assert_eq!(task_action_label(&run_task), "run");

    let debug_task = Task {
        debug: Some("hi".to_string()),
        ..Default::default()
    };
    assert_eq!(task_action_label(&debug_task), "debug");

    let no_action_task = Task::default();
    assert_eq!(task_action_label(&no_action_task), "unknown");
}

#[test]
fn fs_cat_spec_deserializes() {
    let spec: FsCatSpec = serde_yaml::from_str("server: web1\npath: /etc/app/.env\n").unwrap();
    assert_eq!(spec.server, "web1");
    assert_eq!(spec.path, "/etc/app/.env");
}

#[test]
fn fs_write_spec_deserializes_with_default_confirm() {
    let spec: FsWriteSpec =
        serde_yaml::from_str("server: web1\npath: /tmp/x\ncontent: hi\n").unwrap();
    assert!(!spec.confirm);
}

#[test]
fn confirmed_require_rejects_an_unconfirmed_spec_with_a_clear_message() {
    let spec: FsWriteSpec =
        serde_yaml::from_str("server: web1\npath: /tmp/x\ncontent: hi\n").unwrap();
    let err = Confirmed::require(&spec, "fs_write", "overwrite it").unwrap_err();
    assert!(
        err.to_string()
            .contains("fs_write: refused to run without confirm: true (task 'overwrite it')"),
        "error was: {err}"
    );
}

#[test]
fn confirmed_require_accepts_a_confirmed_spec() {
    let spec: FsWriteSpec =
        serde_yaml::from_str("server: web1\npath: /tmp/x\ncontent: hi\nconfirm: true\n").unwrap();
    let confirmed = Confirmed::require(&spec, "fs_write", "overwrite it").unwrap();
    // Deref gives access to the spec's own fields through the proof wrapper.
    assert_eq!(confirmed.path, "/tmp/x");
}

#[test]
fn fs_write_spec_rejects_an_unknown_field_instead_of_silently_dropping_it() {
    let err = serde_yaml::from_str::<FsWriteSpec>(
        "server: web1\npath: /tmp/x\ncontent: hi\nconfrim: true\n",
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("unknown field `confrim`"),
        "error was: {err}"
    );
}

#[test]
fn systemd_restart_spec_deserializes_with_defaults() {
    let spec: SystemdRestartSpec = serde_yaml::from_str("server: web1\nunit: nginx\n").unwrap();
    assert!(!spec.sudo);
    assert!(spec.sudo_pass.is_none());
    assert!(!spec.confirm);
}

#[test]
fn systemd_restart_spec_deserializes_explicit_confirm() {
    let spec: SystemdRestartSpec =
        serde_yaml::from_str("server: web1\nunit: nginx\nconfirm: true\n").unwrap();
    assert!(spec.confirm);
}

#[test]
fn systemd_status_spec_deserializes() {
    let spec: SystemdStatusSpec = serde_yaml::from_str("server: web1\nunit: nginx\n").unwrap();
    assert_eq!(spec.unit, "nginx");
}

#[test]
fn logs_tail_spec_deserializes_with_default_lines() {
    let spec: LogsTailSpec =
        serde_yaml::from_str("server: web1\npath: /var/log/app.log\n").unwrap();
    assert_eq!(spec.lines, 100);
}

#[test]
fn logs_grep_spec_deserializes_with_default_max_lines() {
    let spec: LogsGrepSpec =
        serde_yaml::from_str("server: web1\npath: /var/log/app.log\npattern: ERROR\n").unwrap();
    assert_eq!(spec.max_lines, 200);
}

#[test]
fn ps_list_spec_deserializes_with_default_filter() {
    let spec: PsListSpec = serde_yaml::from_str("server: web1\n").unwrap();
    assert!(spec.filter.is_none());
}

#[test]
fn ps_kill_spec_deserializes_with_defaults() {
    let spec: PsKillSpec = serde_yaml::from_str("server: web1\npid: 1234\n").unwrap();
    assert_eq!(spec.signal, "TERM");
    assert!(!spec.sudo);
    assert!(!spec.confirm);
}

#[test]
fn ps_kill_spec_rejects_an_unknown_field_instead_of_silently_dropping_it() {
    let err =
        serde_yaml::from_str::<PsKillSpec>("server: web1\npid: 1234\nconfrim: true\n").unwrap_err();
    assert!(
        err.to_string().contains("unknown field `confrim`"),
        "error was: {err}"
    );
}

#[test]
fn stat_spec_deserializes() {
    let spec: StatSpec = serde_yaml::from_str("server: web1\n").unwrap();
    assert_eq!(spec.server, "web1");
}

#[test]
fn git_summary_spec_deserializes_from_an_empty_map() {
    serde_yaml::from_str::<GitSummarySpec>("{}").unwrap();
}

#[test]
fn git_changelog_spec_deserializes_with_default_from() {
    let spec: GitChangelogSpec = serde_yaml::from_str("{}").unwrap();
    assert!(spec.from.is_none());
}

#[test]
fn gh_prs_spec_deserializes_with_defaults() {
    let spec: GhPrsSpec = serde_yaml::from_str("{}").unwrap();
    assert_eq!(spec.state, "all");
    assert_eq!(spec.limit, 500);
    assert!(spec.repo.is_none());
}

#[test]
fn db_query_spec_deserializes_with_default_max_rows() {
    let spec: DbQuerySpec =
        serde_yaml::from_str("server: db1\nsql: SELECT 1\nenv: /var/www/.env\n").unwrap();
    assert_eq!(spec.server, "db1");
    assert_eq!(spec.sql, "SELECT 1");
    assert_eq!(spec.env.as_deref(), Some("/var/www/.env"));
    assert_eq!(spec.max_rows, 1000);
}

#[test]
fn db_query_spec_deserializes_explicit_fields_and_max_rows() {
    let spec: DbQuerySpec = serde_yaml::from_str(
        "server: db1\nsql: SELECT 1\nengine: mysql\nhost: 127.0.0.1\nport: 3306\n\
             database: app\nuser: root\npassword: secret\nmax_rows: 50\n",
    )
    .unwrap();
    assert_eq!(spec.engine.as_deref(), Some("mysql"));
    assert_eq!(spec.port, Some(3306));
    assert_eq!(spec.max_rows, 50);
}

#[test]
fn mail_spec_deserializes_with_server_profile() {
    let spec: MailSpec =
        serde_yaml::from_str("server: notif\nto: a@example.com\nsubject: hi\nbody: hello\n")
            .unwrap();
    assert_eq!(spec.server.as_deref(), Some("notif"));
    assert_eq!(spec.to, "a@example.com");
    assert!(!spec.html);
    assert!(spec.host.is_none());
}

#[test]
fn mail_spec_deserializes_with_inline_host_fields() {
    let spec: MailSpec = serde_yaml::from_str(
        "to: a@example.com\nsubject: hi\nbody: hello\nhtml: true\n\
             host: smtp.example.com\nport: 465\nuser: u\npassword: p\ntls: tls\n",
    )
    .unwrap();
    assert!(spec.server.is_none());
    assert_eq!(spec.host.as_deref(), Some("smtp.example.com"));
    assert_eq!(spec.port, Some(465));
    assert!(spec.html);
    assert_eq!(spec.tls.as_deref(), Some("tls"));
}

#[test]
fn mail_spec_deserializes_attachments() {
    let spec: MailSpec = serde_yaml::from_str(
        "server: notif\nto: a@example.com\nsubject: hi\nbody: hello\n\
             attachments:\n  - report.pdf\n  - data.csv\n",
    )
    .unwrap();
    assert_eq!(spec.attachments, vec!["report.pdf", "data.csv"]);
}

#[test]
fn mail_spec_attachments_default_to_empty() {
    let spec: MailSpec =
        serde_yaml::from_str("server: notif\nto: a@example.com\nsubject: hi\nbody: hello\n")
            .unwrap();
    assert!(spec.attachments.is_empty());
}

#[test]
fn mail_spec_rejects_an_unknown_field_instead_of_silently_dropping_it() {
    let err = serde_yaml::from_str::<MailSpec>(
        "servre: notif\nto: a@example.com\nsubject: hi\nbody: hello\n",
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("unknown field `servre`"),
        "error was: {err}"
    );
}

#[test]
fn task_rejects_an_unknown_field_instead_of_silently_dropping_it() {
    // A typo'd delya:/registerr: alongside a valid run: used to succeed silently,
    // ignoring both -- see the deny_unknown_fields sweep across Task and every
    // *Spec struct.
    let err = serde_yaml::from_str::<Task>("name: t\nrun: echo hi\ndelya: 5\nregisterr: oops\n")
        .unwrap_err();
    assert!(
        err.to_string().contains("unknown field `delya`"),
        "error was: {err}"
    );
}

#[test]
fn mail_check_spec_deserializes_with_defaults() {
    let spec: MailCheckSpec = serde_yaml::from_str("server: notif\n").unwrap();
    assert_eq!(spec.server, "notif");
    assert_eq!(spec.folder, "INBOX");
    assert!(spec.unseen_only);
    assert_eq!(spec.limit, 10);
    assert!(!spec.include_body);
    assert!(!spec.mark_seen);
}

#[test]
fn mail_check_spec_deserializes_explicit_fields() {
    let spec: MailCheckSpec = serde_yaml::from_str(
        "server: notif\nfolder: Archive\nunseen_only: false\nlimit: 5\n\
             include_body: true\nmark_seen: true\n",
    )
    .unwrap();
    assert_eq!(spec.folder, "Archive");
    assert!(!spec.unseen_only);
    assert_eq!(spec.limit, 5);
    assert!(spec.include_body);
    assert!(spec.mark_seen);
}

#[test]
fn db_exec_spec_deserializes_with_default_confirm() {
    let spec: DbExecSpec =
        serde_yaml::from_str("server: db1\nsql: UPDATE t SET x=1\nenv: /var/www/.env\n").unwrap();
    assert_eq!(spec.server, "db1");
    assert_eq!(spec.sql, "UPDATE t SET x=1");
    assert!(!spec.confirm);
}

#[test]
fn db_exec_spec_deserializes_explicit_confirm() {
    let spec: DbExecSpec = serde_yaml::from_str(
        "server: db1\nsql: DELETE FROM t WHERE id=1\nengine: mysql\nhost: 127.0.0.1\n\
             user: root\npassword: secret\nconfirm: true\n",
    )
    .unwrap();
    assert!(spec.confirm);
    assert_eq!(spec.engine.as_deref(), Some("mysql"));
}

#[test]
fn resolve_mail_creds_prefers_explicit_over_profile() {
    let mut cfg = crate::config::Config::default();
    cfg.mail.insert(
        "notif".to_string(),
        crate::config::MailServer {
            host: "profile.example.com".to_string(),
            port: 465,
            user: "profileuser@example.com".to_string(),
            from: Some("profile-from@example.com".to_string()),
            tls: None,
            imap_host: None,
            imap_port: None,
        },
    );
    let ctx = Context::new(OutputFormat::Json, "default".to_string(), cfg);
    let creds = resolve_mail_creds(
        &ctx,
        Some("notif"),
        Some("explicit.example.com"),
        None,
        None,
        Some("secretpw"),
        None,
        None,
    )
    .unwrap();
    // Explicit `host` wins over the profile's.
    assert_eq!(creds.host, "explicit.example.com");
    // Unspecified fields fall back to the profile.
    assert_eq!(creds.user, "profileuser@example.com");
    assert_eq!(creds.from, "profile-from@example.com");
    assert_eq!(creds.port, 465);
    // Explicit password always wins (never touches the keychain).
    assert_eq!(creds.password, "secretpw");
    // tls inferred from the profile's port (465) since neither side set `tls`.
    assert_eq!(creds.tls, "tls");
}

#[test]
fn resolve_mail_creds_infers_starttls_from_587_and_tls_from_465() {
    let ctx = Context::new(
        OutputFormat::Json,
        "default".to_string(),
        crate::config::Config::default(),
    );
    let creds_587 = resolve_mail_creds(
        &ctx,
        None,
        Some("h"),
        Some(587),
        Some("u"),
        Some("p"),
        None,
        None,
    )
    .unwrap();
    assert_eq!(creds_587.tls, "starttls");

    let creds_465 = resolve_mail_creds(
        &ctx,
        None,
        Some("h"),
        Some(465),
        Some("u"),
        Some("p"),
        None,
        None,
    )
    .unwrap();
    assert_eq!(creds_465.tls, "tls");
}

#[test]
fn resolve_mail_creds_errors_clearly_with_no_creds() {
    let ctx = Context::new(
        OutputFormat::Json,
        "default".to_string(),
        crate::config::Config::default(),
    );
    let err = resolve_mail_creds(&ctx, None, None, None, None, None, None, None).unwrap_err();
    assert!(err.to_string().contains("host"));
}

#[test]
fn resolve_mail_creds_errors_on_unknown_profile() {
    let ctx = Context::new(
        OutputFormat::Json,
        "default".to_string(),
        crate::config::Config::default(),
    );
    let err =
        resolve_mail_creds(&ctx, Some("ghost"), None, None, None, None, None, None).unwrap_err();
    assert!(err.to_string().contains("No mail profile 'ghost'"));
}

#[test]
fn resolve_imap_creds_errors_on_unknown_profile() {
    let ctx = Context::new(
        OutputFormat::Json,
        "default".to_string(),
        crate::config::Config::default(),
    );
    let err = resolve_imap_creds(&ctx, "ghost").unwrap_err();
    assert!(err.to_string().contains("No mail profile 'ghost'"));
}

#[test]
fn resolve_imap_host_port_defaults_from_smtp_host_and_993() {
    let profile = crate::config::MailServer {
        host: "mail16.serv00.com".to_string(),
        port: 587,
        user: "notification@example.com".to_string(),
        from: None,
        tls: None,
        imap_host: None,
        imap_port: None,
    };
    let (host, port) = resolve_imap_host_port(&profile);
    assert_eq!(host.as_deref(), Some("mail16.serv00.com"));
    assert_eq!(port, 993);
}

#[test]
fn resolve_imap_host_port_prefers_explicit_imap_fields() {
    let profile = crate::config::MailServer {
        host: "smtp.example.com".to_string(),
        port: 587,
        user: "notification@example.com".to_string(),
        from: None,
        tls: None,
        imap_host: Some("imap.example.com".to_string()),
        imap_port: Some(143),
    };
    let (host, port) = resolve_imap_host_port(&profile);
    assert_eq!(host.as_deref(), Some("imap.example.com"));
    assert_eq!(port, 143);
}

#[test]
fn max_parallel_without_loop_is_rejected() {
    let ctx = Context::new(
        OutputFormat::Json,
        "default".to_string(),
        crate::config::Config::default(),
    );
    let env = dry_env(&ctx);
    let mut vars = HashMap::new();
    let mut include_stack = Vec::new();
    let task = Task {
        debug: Some("hi".to_string()),
        max_parallel: Some(4),
        ..Default::default()
    };
    let err = run_task(&task, &mut vars, &mut include_stack, &env).unwrap_err();
    assert!(
        err.to_string().contains("max_parallel:"),
        "error was: {err}"
    );
}

#[test]
fn merge_repl_name_adds_a_name_when_absent() {
    let mut value: serde_yaml::Value = serde_yaml::from_str("run: echo hi").unwrap();
    merge_repl_name(&mut value, "repl-1");
    assert_eq!(value["name"].as_str(), Some("repl-1"));
    assert_eq!(value["run"].as_str(), Some("echo hi"));
}

#[test]
fn merge_repl_name_leaves_an_explicit_name_alone() {
    let mut value: serde_yaml::Value =
        serde_yaml::from_str("name: my task\nrun: echo hi\n").unwrap();
    merge_repl_name(&mut value, "repl-1");
    assert_eq!(value["name"].as_str(), Some("my task"));
}

#[test]
fn set_fact_stores_rendered_values_into_vars() {
    let ctx = Context::new(
        OutputFormat::Json,
        "default".to_string(),
        crate::config::Config::default(),
    );
    let env = dry_env(&ctx);
    let mut vars = vars(&[("name", "world")]);
    let mut include_stack = Vec::new();
    let mut facts = HashMap::new();
    facts.insert("greeting".to_string(), "hello {{name}}".to_string());
    let task = Task {
        set_fact: Some(facts),
        ..Default::default()
    };
    assert!(run_task_once(&task, &mut vars, &mut include_stack, &env).is_ok());
    assert_eq!(
        vars.get("greeting").map(String::as_str),
        Some("hello world")
    );
}

#[test]
fn set_fact_rejects_register() {
    let ctx = Context::new(
        OutputFormat::Json,
        "default".to_string(),
        crate::config::Config::default(),
    );
    let env = dry_env(&ctx);
    let mut vars = HashMap::new();
    let mut include_stack = Vec::new();
    let task = Task {
        register: Some("x".to_string()),
        set_fact: Some(HashMap::new()),
        ..Default::default()
    };
    assert!(run_task_once(&task, &mut vars, &mut include_stack, &env).is_err());
}

#[test]
fn render_leaves_unknown_and_unresolvable_tokens_literal() {
    let v = vars(&[]);
    assert_eq!(render("{{nope}}", &v), "{{nope}}");
    // A profile/key that was never stored resolves to None the same way an unknown
    // plain var does — never crashes, just leaves the token as-is.
    assert_eq!(
        render("{{secret.nonexistent_profile.nonexistent_key}}", &v),
        "{{secret.nonexistent_profile.nonexistent_key}}"
    );
}

#[test]
fn validate_handlers_rejects_unknown_notify_target() {
    let playbook = Playbook {
        name: "t".to_string(),
        description: None,
        vars_files: Vec::new(),
        vars: HashMap::new(),
        handlers: vec![],
        on_failure: vec![],
        mcp_tool: None,
        params: HashMap::new(),
        single_instance: false,
        lock_timeout: 21600,
        tasks: vec![Task {
            name: "t1".to_string(),
            notify: vec!["nonexistent_handler".to_string()],
            ..Default::default()
        }],
    };
    assert!(validate_handlers(&playbook).is_err());
}

#[test]
fn validate_handlers_accepts_a_matching_notify_target() {
    let playbook = Playbook {
        name: "t".to_string(),
        description: None,
        vars_files: Vec::new(),
        vars: HashMap::new(),
        handlers: vec![Task {
            name: "restart".to_string(),
            ..Default::default()
        }],
        on_failure: vec![],
        mcp_tool: None,
        params: HashMap::new(),
        single_instance: false,
        lock_timeout: 21600,
        tasks: vec![Task {
            name: "t1".to_string(),
            notify: vec!["restart".to_string()],
            ..Default::default()
        }],
    };
    assert!(validate_handlers(&playbook).is_ok());
}

#[test]
fn parse_field_selector_splits_on_last_at() {
    assert_eq!(parse_field_selector("a.title"), ("a.title", None));
    assert_eq!(
        parse_field_selector("a.title@href"),
        ("a.title", Some("href"))
    );
    // No plain-CSS attribute selector like `[data-x]` starts or ends with '@', so an
    // empty side just falls back to "no attribute" rather than misparsing.
    assert_eq!(parse_field_selector("@href"), ("@href", None));
    assert_eq!(parse_field_selector("a.title@"), ("a.title@", None));
}

#[test]
fn resolve_loop_items_dynamic_parses_json_array_of_objects() {
    let v = vars(&[(
        "jobs",
        r#"[{"title":"Dev","company":"Acme"},{"title":"Lead","company":"Beta"}]"#,
    )]);
    let spec = LoopSpec::Dynamic {
        from: "{{jobs}}".to_string(),
        split: None,
        batch: None,
    };
    let items = resolve_loop_items(&spec, &v);
    assert_eq!(items.len(), 2);
    let LoopItem::Map(m) = &items[0] else {
        panic!("expected a map item");
    };
    assert_eq!(m.get("title"), Some(&"Dev".to_string()));
    assert_eq!(m.get("company"), Some(&"Acme".to_string()));
}

#[test]
fn resolve_loop_items_dynamic_falls_back_to_text_split() {
    let v = vars(&[("names", "alice\nbob\n\ncarol")]);
    let spec = LoopSpec::Dynamic {
        from: "{{names}}".to_string(),
        split: None,
        batch: None,
    };
    let items = resolve_loop_items(&spec, &v);
    assert_eq!(items.len(), 3);
    assert!(matches!(&items[0], LoopItem::Scalar(s) if s == "alice"));
    assert!(matches!(&items[2], LoopItem::Scalar(s) if s == "carol"));
}

#[test]
fn resolve_loop_items_dynamic_respects_custom_split() {
    let v = vars(&[("names", "alice,bob,carol")]);
    let spec = LoopSpec::Dynamic {
        from: "{{names}}".to_string(),
        split: Some(",".to_string()),
        batch: None,
    };
    let items = resolve_loop_items(&spec, &v);
    assert_eq!(items.len(), 3);
    assert!(matches!(&items[1], LoopItem::Scalar(s) if s == "bob"));
}

#[test]
fn scrape_spec_deserializes() {
    let spec: ScrapeSpec = serde_yaml::from_str(
        "url: https://example.com\neach: .row\nfields:\n  title: .title\n  link: a@href\n",
    )
    .unwrap();
    assert_eq!(spec.url, "https://example.com");
    assert_eq!(spec.each.as_deref(), Some(".row"));
    assert_eq!(spec.fields.get("link").map(String::as_str), Some("a@href"));
}

#[test]
fn loop_item_deserializes_scalar_list() {
    let items: Vec<LoopItem> = serde_yaml::from_str("[a, b, c]").unwrap();
    assert!(matches!(&items[0], LoopItem::Scalar(s) if s == "a"));
    assert!(matches!(&items[2], LoopItem::Scalar(s) if s == "c"));
}

#[test]
fn loop_item_deserializes_map_list() {
    let items: Vec<LoopItem> =
        serde_yaml::from_str("- name: a\n  port: \"1\"\n- name: b\n  port: \"2\"\n").unwrap();
    let LoopItem::Map(m) = &items[0] else {
        panic!("expected a map item");
    };
    assert_eq!(m.get("name"), Some(&"a".to_string()));
    assert_eq!(m.get("port"), Some(&"1".to_string()));
}

#[test]
fn confirm_dry_run_is_a_noop() {
    let ctx = Context::new(
        OutputFormat::Json,
        "default".to_string(),
        crate::config::Config::default(),
    );
    let env = dry_env(&ctx); // dry: true
    let mut vars = HashMap::new();
    let mut include_stack = Vec::new();
    let task = Task {
        confirm: Some("proceed?".to_string()),
        ..Default::default()
    };
    assert!(run_task_once(&task, &mut vars, &mut include_stack, &env).is_ok());
}

#[test]
fn confirm_bails_when_quiet_and_not_auto_yes() {
    let ctx = Context::new(
        OutputFormat::Json,
        "default".to_string(),
        crate::config::Config::default(),
    );
    // quiet: true (inherited from dry_env), dry: false — the non-interactive/agent
    // path with no --yes must fail fast instead of blocking on stdin.
    let env = RunEnv {
        dry: false,
        ..dry_env(&ctx)
    };
    let mut vars = HashMap::new();
    let mut include_stack = Vec::new();
    let task = Task {
        confirm: Some("proceed?".to_string()),
        ..Default::default()
    };
    let err = run_task_once(&task, &mut vars, &mut include_stack, &env).unwrap_err();
    assert!(err.to_string().contains("--yes"), "error was: {err}");
}

#[test]
fn confirm_succeeds_with_auto_yes_without_prompting() {
    let ctx = Context::new(
        OutputFormat::Json,
        "default".to_string(),
        crate::config::Config::default(),
    );
    let env = RunEnv {
        dry: false,
        auto_yes: true,
        ..dry_env(&ctx)
    };
    let mut vars = HashMap::new();
    let mut include_stack = Vec::new();
    let task = Task {
        confirm: Some("proceed?".to_string()),
        ..Default::default()
    };
    assert!(run_task_once(&task, &mut vars, &mut include_stack, &env).is_ok());
}

#[test]
fn confirm_rejects_register() {
    let ctx = Context::new(
        OutputFormat::Json,
        "default".to_string(),
        crate::config::Config::default(),
    );
    let env = dry_env(&ctx);
    let mut vars = HashMap::new();
    let mut include_stack = Vec::new();
    let task = Task {
        register: Some("x".to_string()),
        confirm: Some("proceed?".to_string()),
        ..Default::default()
    };
    assert!(run_task_once(&task, &mut vars, &mut include_stack, &env).is_err());
}

#[test]
fn report_spec_deserializes_with_default_title() {
    let spec: ReportSpec =
        serde_yaml::from_str("format: html\nsources:\n  data: \"{{resp}}\"\nout: report.html\n")
            .unwrap();
    assert_eq!(spec.format, "html");
    assert_eq!(spec.title, "Tooler Report");
    assert_eq!(
        spec.sources.get("data").map(String::as_str),
        Some("{{resp}}")
    );
    assert_eq!(spec.out, "report.html");
}

#[test]
fn report_rejects_an_unknown_format() {
    let ctx = Context::new(
        OutputFormat::Json,
        "default".to_string(),
        crate::config::Config::default(),
    );
    let env = dry_env(&ctx);
    let mut vars = HashMap::new();
    let mut include_stack = Vec::new();
    let task = Task {
        report: Some(ReportSpec {
            format: "csv".to_string(),
            title: default_report_title(),
            sources: HashMap::new(),
            out: "out.csv".to_string(),
        }),
        ..Default::default()
    };
    let err = run_task_once(&task, &mut vars, &mut include_stack, &env).unwrap_err();
    assert!(
        err.to_string().contains("html/pdf/excel"),
        "error was: {err}"
    );
}

#[test]
fn report_accepts_a_known_format_case_insensitively() {
    let ctx = Context::new(
        OutputFormat::Json,
        "default".to_string(),
        crate::config::Config::default(),
    );
    let env = dry_env(&ctx); // dry: true — validates format without writing a file
    let mut vars = HashMap::new();
    let mut include_stack = Vec::new();
    let task = Task {
        report: Some(ReportSpec {
            format: "HTML".to_string(),
            title: default_report_title(),
            sources: HashMap::new(),
            out: "out.html".to_string(),
        }),
        ..Default::default()
    };
    assert!(run_task_once(&task, &mut vars, &mut include_stack, &env).is_ok());
}

#[test]
fn wait_for_requires_exactly_one_check() {
    let ctx = Context::new(
        OutputFormat::Json,
        "default".to_string(),
        crate::config::Config::default(),
    );
    let env = dry_env(&ctx);
    let mut vars = HashMap::new();
    let mut include_stack = Vec::new();

    // Zero checks set.
    let task = Task {
        wait_for: Some(WaitForSpec::default()),
        ..Default::default()
    };
    assert!(run_task_once(&task, &mut vars, &mut include_stack, &env).is_err());

    // Exactly one — accepted (dry: true, so no real polling happens).
    let task = Task {
        wait_for: Some(WaitForSpec {
            check_url: Some("http://example.com".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert!(run_task_once(&task, &mut vars, &mut include_stack, &env).is_ok());

    // Two checks set at once.
    let task = Task {
        wait_for: Some(WaitForSpec {
            check_url: Some("http://example.com".to_string()),
            check_port: Some(CheckPortSpec {
                host: "example.com".to_string(),
                port: 80,
                timeout: 5,
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert!(run_task_once(&task, &mut vars, &mut include_stack, &env).is_err());

    // file_exists alone — accepted.
    let task = Task {
        wait_for: Some(WaitForSpec {
            file_exists: Some("flag.txt".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert!(run_task_once(&task, &mut vars, &mut include_stack, &env).is_ok());

    // file_exists and file_absent set together.
    let task = Task {
        wait_for: Some(WaitForSpec {
            file_exists: Some("flag.txt".to_string()),
            file_absent: Some("lock.txt".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert!(run_task_once(&task, &mut vars, &mut include_stack, &env).is_err());
}

#[test]
fn wait_for_rejects_register() {
    let ctx = Context::new(
        OutputFormat::Json,
        "default".to_string(),
        crate::config::Config::default(),
    );
    let env = dry_env(&ctx);
    let mut vars = HashMap::new();
    let mut include_stack = Vec::new();
    let task = Task {
        register: Some("x".to_string()),
        wait_for: Some(WaitForSpec {
            check_url: Some("http://example.com".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert!(run_task_once(&task, &mut vars, &mut include_stack, &env).is_err());
}

#[test]
fn timeout_without_run_is_rejected() {
    let ctx = Context::new(
        OutputFormat::Json,
        "default".to_string(),
        crate::config::Config::default(),
    );
    let env = dry_env(&ctx);
    let mut vars = HashMap::new();
    let mut include_stack = Vec::new();
    let task = Task {
        timeout: Some(5),
        check_url: Some("http://example.com".to_string()),
        ..Default::default()
    };
    assert!(run_task_once(&task, &mut vars, &mut include_stack, &env).is_err());
}

#[test]
fn timeout_with_run_is_accepted() {
    let ctx = Context::new(
        OutputFormat::Json,
        "default".to_string(),
        crate::config::Config::default(),
    );
    // dry: true — only the upfront validation runs, no real subprocess/timeout.
    let env = dry_env(&ctx);
    let mut vars = HashMap::new();
    let mut include_stack = Vec::new();
    let task = Task {
        timeout: Some(5),
        run: Some(RunSpec::Simple("echo hi".to_string())),
        ..Default::default()
    };
    assert!(run_task_once(&task, &mut vars, &mut include_stack, &env).is_ok());
}

#[test]
fn ensure_trailing_slash_appends_when_missing_and_is_idempotent() {
    assert_eq!(ensure_trailing_slash("/a/b"), "/a/b/");
    assert_eq!(ensure_trailing_slash("/a/b/"), "/a/b/");
}

#[test]
fn join_confined_allows_plain_relative_paths() {
    let base = Path::new("/pb/dir");
    assert_eq!(
        join_confined(base, "out.txt").unwrap().as_path(),
        Path::new("/pb/dir/out.txt")
    );
    assert_eq!(
        join_confined(base, "sub/out.txt").unwrap().as_path(),
        Path::new("/pb/dir/sub/out.txt")
    );
}

#[test]
fn join_confined_allows_a_dotdot_that_nets_back_inside_base() {
    let base = Path::new("/pb/dir");
    // Wanders outside and back, but never nets below `base`.
    assert_eq!(
        join_confined(base, "a/../b").unwrap().as_path(),
        Path::new("/pb/dir/b")
    );
}

#[test]
fn join_confined_rejects_absolute_paths() {
    let base = Path::new("/pb/dir");
    assert!(join_confined(base, "/etc/passwd").is_err());
}

#[test]
fn join_confined_rejects_dotdot_that_escapes_base() {
    let base = Path::new("/pb/dir");
    assert!(join_confined(base, "../outside.txt").is_err());
    assert!(join_confined(base, "a/../../outside.txt").is_err());
}

#[test]
fn data_path_for_uses_a_different_suffix_than_state_path_for() {
    let file = Path::new("/pb/dir/playbook.yml");
    let data_path = data_path_for(file);
    let state_path = state_path_for(file);
    assert_eq!(data_path, PathBuf::from("/pb/dir/playbook.yml.data.json"));
    assert_ne!(data_path, state_path);
}

#[test]
fn load_persisted_state_returns_an_empty_map_for_a_missing_file() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nope.data.json");
    let state = load_persisted_state(&missing).unwrap();
    assert!(state.is_empty());
}

#[test]
fn load_persisted_state_reads_a_real_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("playbook.yml.data.json");
    std::fs::write(&path, r#"{"last_uid": "42"}"#).unwrap();
    let state = load_persisted_state(&path).unwrap();
    assert_eq!(state.get("last_uid"), Some(&"42".to_string()));
}

#[test]
fn db_sync_spec_deserializes_env_and_explicit_sides() {
    let spec: DbSyncSpec = serde_yaml::from_str(
        "server: serv00\n\
             from:\n  env: backend_prod/.env\n\
             to:\n  engine: mysql\n  host: localhost\n  database: dev_db\n  user: root\n",
    )
    .unwrap();
    assert_eq!(spec.server, "serv00");
    assert_eq!(spec.from.env.as_deref(), Some("backend_prod/.env"));
    assert_eq!(spec.to.database.as_deref(), Some("dev_db"));
    assert_eq!(spec.to.engine.as_deref(), Some("mysql"));
}

#[test]
fn sync_files_spec_deserializes() {
    let spec: SyncFilesSpec = serde_yaml::from_str(
        "server: serv00\nfrom: /a/prod/storage\nto: /a/dev/storage\ndelete: true\n",
    )
    .unwrap();
    assert_eq!(spec.from, "/a/prod/storage");
    assert!(spec.delete);
}

#[test]
fn sync_db_and_sync_files_support_register() {
    let ctx = Context::new(
        OutputFormat::Json,
        "default".to_string(),
        crate::config::Config::default(),
    );
    // dry: true — only the upfront register-validation guard runs, no real SSH.
    let env = dry_env(&ctx);
    let mut vars = HashMap::new();
    let mut include_stack = Vec::new();

    let task = Task {
        register: Some("x".to_string()),
        sync_db: Some(DbSyncSpec {
            server: "serv00".to_string(),
            from: DbSyncSide::default(),
            to: DbSyncSide::default(),
        }),
        ..Default::default()
    };
    assert!(run_task_once(&task, &mut vars, &mut include_stack, &env).is_ok());

    let task = Task {
        register: Some("x".to_string()),
        sync_files: Some(SyncFilesSpec {
            server: "serv00".to_string(),
            from: "/a".to_string(),
            to: "/b".to_string(),
            delete: false,
        }),
        ..Default::default()
    };
    assert!(run_task_once(&task, &mut vars, &mut include_stack, &env).is_ok());
}

#[test]
fn sync_files_no_longer_rejects_timeout() {
    // timeout: is now supported on sync_files: (an rsync can hang just as easily as
    // run:/ssh:/fleet: can) -- --dry never attempts a real connection either way, so
    // this should succeed instead of hitting the old upfront "only supported on" bail.
    let ctx = Context::new(
        OutputFormat::Json,
        "default".to_string(),
        crate::config::Config::default(),
    );
    let env = dry_env(&ctx);
    let mut vars = HashMap::new();
    let mut include_stack = Vec::new();
    let task = Task {
        timeout: Some(5),
        sync_files: Some(SyncFilesSpec {
            server: "serv00".to_string(),
            from: "/a".to_string(),
            to: "/b".to_string(),
            delete: false,
        }),
        ..Default::default()
    };
    assert!(run_task_once(&task, &mut vars, &mut include_stack, &env).is_ok());
}

#[test]
fn include_spec_deserializes_both_shapes() {
    let simple: IncludeSpec = serde_yaml::from_str("sub.yml").unwrap();
    assert_eq!(simple.file(), "sub.yml");
    assert!(simple.vars().is_empty());

    let with_vars: IncludeSpec =
        serde_yaml::from_str("file: sub.yml\nvars:\n  service: api\n").unwrap();
    assert_eq!(with_vars.file(), "sub.yml");
    assert_eq!(
        with_vars.vars().get("service").map(String::as_str),
        Some("api")
    );
}

#[test]
fn load_playbook_vars_with_no_vars_files_passes_inline_vars_through() {
    let playbook = Playbook {
        name: "t".to_string(),
        description: None,
        vars_files: Vec::new(),
        vars: vars(&[("host", "localhost")]),
        handlers: vec![],
        on_failure: vec![],
        mcp_tool: None,
        params: HashMap::new(),
        single_instance: false,
        lock_timeout: 21600,
        tasks: vec![],
    };
    let merged = load_playbook_vars(&playbook, Path::new(".")).unwrap();
    assert_eq!(merged.get("host").map(String::as_str), Some("localhost"));
}

#[test]
fn start_at_task_rejects_an_unknown_task_name() {
    let ctx = Context::new(
        OutputFormat::Json,
        "default".to_string(),
        crate::config::Config::default(),
    );
    let env = RunEnv {
        start_at: Some("nonexistent".to_string()),
        ..dry_env(&ctx)
    };
    let playbook = Playbook {
        name: "t".to_string(),
        description: None,
        vars_files: Vec::new(),
        vars: HashMap::new(),
        handlers: vec![],
        on_failure: vec![],
        mcp_tool: None,
        params: HashMap::new(),
        single_instance: false,
        lock_timeout: 21600,
        tasks: vec![Task {
            name: "only task".to_string(),
            debug: Some("hi".to_string()),
            ..Default::default()
        }],
    };
    let mut vars = HashMap::new();
    let mut include_stack = Vec::new();
    let err = execute_playbook(
        &playbook,
        &None,
        &None,
        &None,
        &mut vars,
        &mut include_stack,
        true,
        &env,
    )
    .unwrap_err();
    assert!(err.to_string().contains("nonexistent"), "error was: {err}");
}

#[test]
fn parse_pipeline_splits_token_and_ops() {
    let (tok, ops) = parse_pipeline("rows | where:active==true | pluck:name | join:,");
    assert_eq!(tok, "rows");
    assert_eq!(
        ops,
        vec![
            FilterOp::Where("active", true, "true"),
            FilterOp::Pluck("name"),
            FilterOp::Join(","),
        ]
    );
}

#[test]
fn parse_pipeline_stops_at_an_unknown_filter_word() {
    let (tok, ops) = parse_pipeline("x | bogus | upper");
    assert_eq!(tok, "x");
    assert!(ops.is_empty());
}

/// A no-op resolver for the filter unit tests (only `hmac_sha256:` uses it).
fn no_resolve(_: &str) -> Option<String> {
    None
}
fn filt(op: FilterOp, v: &str) -> Option<String> {
    apply_filter_op(&op, v.to_string(), &no_resolve)
}

#[test]
fn apply_filter_op_pluck_and_where_and_join() {
    let rows = r#"[{"n":"a","ok":"y"},{"n":"b","ok":"n"},{"n":"c","ok":"y"}]"#;
    assert_eq!(
        filt(FilterOp::Pluck("n"), rows).unwrap(),
        r#"["a","b","c"]"#
    );
    let filtered = filt(FilterOp::Where("ok", true, "y"), rows).unwrap();
    let names = filt(FilterOp::Pluck("n"), &filtered).unwrap();
    assert_eq!(filt(FilterOp::Join("-"), &names).unwrap(), "a-c");
}

#[test]
fn apply_filter_op_on_a_non_array_yields_none() {
    assert!(filt(FilterOp::Pluck("x"), "not json").is_none());
    assert!(filt(FilterOp::First, "42").is_none());
}

#[test]
fn date_time_filters_round_trip() {
    // 2021-01-01T00:00:00Z is unix 1609459200.
    assert_eq!(
        filt(FilterOp::Unix, "2021-01-01T00:00:00Z").unwrap(),
        "1609459200"
    );
    assert_eq!(
        filt(FilterOp::Date("%Y-%m-%d"), "1609459200").unwrap(),
        "2021-01-01"
    );
    let shifted = filt(FilterOp::Shift("-1d"), "2021-01-02T00:00:00Z").unwrap();
    assert_eq!(
        filt(FilterOp::Date("%Y-%m-%d"), &shifted).unwrap(),
        "2021-01-01"
    );
}

#[test]
fn encoding_and_hash_filters() {
    assert_eq!(filt(FilterOp::Base64, "hello").unwrap(), "aGVsbG8=");
    assert_eq!(filt(FilterOp::Base64d, "aGVsbG8=").unwrap(), "hello");
    assert_eq!(
        filt(FilterOp::Sha256, "abc").unwrap(),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(
        filt(FilterOp::Md5, "abc").unwrap(),
        "900150983cd24fb0d6963f7d28e17f72"
    );
    assert_eq!(filt(FilterOp::UrlEncode, "a b&c").unwrap(), "a%20b%26c");
    // Known HMAC-SHA256 vector (key "key", msg "The quick brown fox jumps over the lazy dog").
    let sig = apply_filter_op(
        &FilterOp::HmacSha256("k"),
        "The quick brown fox jumps over the lazy dog".to_string(),
        &|name: &str| (name == "k").then(|| "key".to_string()),
    )
    .unwrap();
    assert_eq!(
        sig,
        "f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8"
    );
}

#[test]
fn numeric_and_aggregate_filters() {
    assert_eq!(filt(FilterOp::Add("5"), "10").unwrap(), "15");
    assert_eq!(
        filt(FilterOp::Div("3"), "10").unwrap(),
        "3.3333333333333335"
    );
    assert_eq!(filt(FilterOp::Round(Some("2")), "3.14159").unwrap(), "3.14");
    assert_eq!(filt(FilterOp::Sum, "[1,2,3,4]").unwrap(), "10");
    assert_eq!(filt(FilterOp::Avg, "[2,4]").unwrap(), "3");
    assert_eq!(filt(FilterOp::Count, r#"["a","b"]"#).unwrap(), "2");
    assert_eq!(filt(FilterOp::Sort, "[3,1,2]").unwrap(), "[1,2,3]");
    assert_eq!(filt(FilterOp::Unique, "[1,1,2,2,3]").unwrap(), "[1,2,3]");
    assert_eq!(filt(FilterOp::Reverse, "[1,2,3]").unwrap(), "[3,2,1]");
    assert_eq!(
        filt(FilterOp::Slice("1:3"), "[0,1,2,3,4]").unwrap(),
        "[1,2]"
    );
    assert_eq!(
        filt(FilterOp::Slice("-2:"), "[0,1,2,3,4]").unwrap(),
        "[3,4]"
    );
    assert_eq!(
        filt(FilterOp::SortBy("p"), r#"[{"p":3},{"p":1},{"p":2}]"#).unwrap(),
        r#"[{"p":1},{"p":2},{"p":3}]"#
    );
}

#[test]
fn render_default_only_fires_on_an_unresolved_token() {
    let v = vars(&[("known", "here")]);
    assert_eq!(render("{{known | default:x}}", &v), "here");
    assert_eq!(render("{{missing | default:x}}", &v), "x");
}

#[test]
fn playbook_tool_defs_reads_mcp_tool_and_params() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
            dir.path().join("deploy.yml"),
            "name: Deploy\nmcp_tool: {description: \"Deploy the app\"}\n\
             params:\n  env: {type: string, required: true, enum: [prod, staging]}\n  \
             dry: {type: boolean, default: false}\n\
             tasks:\n  - name: go\n    db_exec: {server: db1, sql: \"UPDATE x SET y=1\", confirm: true}\n",
        )
        .unwrap();
    std::fs::write(
        dir.path().join("plain.yml"),
        "name: Plain\ntasks:\n  - name: x\n    run: echo hi\n",
    )
    .unwrap();

    let defs = playbook_tool_defs(dir.path());
    assert_eq!(defs.len(), 1, "only the mcp_tool: one is exposed");
    let d = &defs[0];
    assert_eq!(d.name, "tooler_pb_deploy");
    assert_eq!(d.description, "Deploy the app");
    let schema = &d.input_schema;
    assert_eq!(schema["required"], serde_json::json!(["env"]));
    assert_eq!(
        schema["properties"]["env"]["enum"],
        serde_json::json!(["prod", "staging"])
    );
    // has a confirm-gated db_exec: -> the schema offers a `confirm` toggle.
    assert_eq!(schema["properties"]["confirm"]["type"], "boolean");
}

#[cfg(test)]
mod redaction_tests {
    use super::*;

    #[test]
    fn redact_secrets_replaces_every_occurrence_and_ignores_empty_strings() {
        let text = "connecting with hunter2 to host, retrying hunter2 again";
        let out = redact_secrets(text, &["hunter2".to_string(), "".to_string()]);
        assert_eq!(out, "connecting with *** to host, retrying *** again");
    }

    #[test]
    fn redact_secrets_is_a_no_op_with_no_secrets() {
        let text = "plain error, nothing to hide";
        assert_eq!(redact_secrets(text, &[]), text);
    }

    #[test]
    fn task_secret_values_finds_and_resolves_a_referenced_secret() {
        // Real OS keychain round-trip, same tolerant pattern Check C's own secret tests
        // use elsewhere in this codebase -- if this environment has no working keychain
        // backend at all, set_secret itself fails and the test abstains rather than
        // asserting a false failure.
        let profile = "tooler_test_redact";
        let key = "k";
        if crate::secrets::set_secret(profile, key, "s3cr3t-value").is_err() {
            return;
        }
        let task = Task {
            name: "t".to_string(),
            run: Some(RunSpec::Simple(format!(
                "echo {{{{secret.{profile}.{key}}}}}"
            ))),
            ..Default::default()
        };
        let found = task_secret_values(&task);
        assert!(
            found.contains(&"s3cr3t-value".to_string()),
            "expected to find the resolved secret value, got: {found:?}"
        );
        assert_eq!(
            redact_secrets("output was: s3cr3t-value", &found),
            "output was: ***"
        );
    }

    #[test]
    fn task_secret_values_is_empty_when_the_task_references_no_secret() {
        let task = Task {
            name: "t".to_string(),
            run: Some(RunSpec::Simple("echo hi".to_string())),
            ..Default::default()
        };
        assert!(task_secret_values(&task).is_empty());
    }
}
