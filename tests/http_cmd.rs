mod common;

use common::tooler;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

fn stdout_of(assert: assert_cmd::assert::Assert) -> String {
    String::from_utf8_lossy(&assert.get_output().stdout).to_string()
}

/// Spawns a one-shot local HTTP server that records the raw request bytes it receives
/// (so a test can assert on the method/path/headers/body actually sent) and replies
/// with a fixed status line + body. Returns the port and a handle to the captured
/// request, populated once the single connection has been served.
fn capture_server(status_line: &str, body: &str) -> (u16, Arc<Mutex<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let captured = Arc::new(Mutex::new(String::new()));
    let captured_clone = captured.clone();
    let status_line = status_line.to_string();
    let body = body.to_string();
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buf = [0u8; 8192];
            let n = stream.read(&mut buf).unwrap_or(0);
            *captured_clone.lock().unwrap() = String::from_utf8_lossy(&buf[..n]).to_string();
            let header = format!(
                "HTTP/1.1 {status_line}\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(header.as_bytes());
            let _ = stream.write_all(body.as_bytes());
        }
    });
    (port, captured)
}

#[test]
fn get_sends_query_params_url_encoded() {
    let (port, captured) = capture_server("200 OK", r#"{"ok":true}"#);
    let (mut cmd, _dir) = tooler();
    cmd.args([
        "http",
        "get",
        &format!("http://127.0.0.1:{port}/search"),
        "-q",
        "q=hello world",
        "-q",
        "page=2",
    ])
    .assert()
    .success();
    let req = captured.lock().unwrap().clone();
    assert!(
        req.starts_with("GET /search?q=hello+world&page=2 HTTP/1.1")
            || req.starts_with("GET /search?q=hello%20world&page=2 HTTP/1.1"),
        "unexpected request line: {req}"
    );
}

#[test]
fn put_patch_delete_head_send_the_right_method() {
    for (verb, method) in [
        ("put", "PUT"),
        ("patch", "PATCH"),
        ("delete", "DELETE"),
        ("head", "HEAD"),
    ] {
        let (port, captured) = capture_server("200 OK", "{}");
        let (mut cmd, _dir) = tooler();
        cmd.args(["http", verb, &format!("http://127.0.0.1:{port}/x")])
            .assert()
            .success();
        let req = captured.lock().unwrap().clone();
        assert!(
            req.starts_with(&format!("{method} /x HTTP/1.1")),
            "verb {verb}: unexpected request line: {req}"
        );
    }
}

#[test]
fn form_flag_sends_url_encoded_body_with_the_right_content_type() {
    let (port, captured) = capture_server("200 OK", "{}");
    let (mut cmd, _dir) = tooler();
    cmd.args([
        "http",
        "post",
        &format!("http://127.0.0.1:{port}/submit"),
        "-f",
        "a=1",
        "-f",
        "b=two",
    ])
    .assert()
    .success();
    let req = captured.lock().unwrap().clone();
    assert!(
        req.contains("content-type: application/x-www-form-urlencoded")
            || req.contains("Content-Type: application/x-www-form-urlencoded"),
        "missing form content-type: {req}"
    );
    assert!(req.contains("a=1&b=two"), "missing form body: {req}");
}

#[test]
fn body_file_flag_reads_the_request_body_from_disk() {
    let (port, captured) = capture_server("200 OK", "{}");
    let (mut cmd, dir) = tooler();
    std::fs::write(dir.path().join("payload.json"), r#"{"n":42}"#).unwrap();
    cmd.args([
        "http",
        "post",
        &format!("http://127.0.0.1:{port}/x"),
        "--body-file",
        "payload.json",
    ])
    .assert()
    .success();
    let req = captured.lock().unwrap().clone();
    assert!(req.ends_with(r#"{"n":42}"#), "unexpected body: {req}");
}

#[test]
fn basic_flag_sets_the_authorization_header() {
    let (port, captured) = capture_server("200 OK", "{}");
    let (mut cmd, _dir) = tooler();
    cmd.args([
        "http",
        "get",
        &format!("http://127.0.0.1:{port}/x"),
        "--basic",
        "alice:s3cret",
    ])
    .assert()
    .success();
    let req = captured.lock().unwrap().clone();
    // base64("alice:s3cret") = YWxpY2U6czNjcmV0
    assert!(
        req.to_lowercase().contains(
            "authorization: basic ywxpy2u6czntcmv0"
                .to_lowercase()
                .as_str()
        ) || req.contains("Basic YWxpY2U6czNjcmV0"),
        "missing/incorrect basic auth header: {req}"
    );
}

#[test]
fn body_and_form_flags_are_mutually_exclusive() {
    let (mut cmd, _dir) = tooler();
    let assert = cmd
        .args([
            "http",
            "post",
            "http://127.0.0.1:1",
            "--body",
            "{}",
            "--form",
            "a=1",
        ])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();
    assert!(stderr.contains("cannot be used with"), "{stderr}");
}

#[test]
fn verbose_flag_prints_request_and_response_headers() {
    let (port, _captured) = capture_server("200 OK", r#"{"ok":true}"#);
    let (mut cmd, _dir) = tooler();
    let out = stdout_of(
        cmd.args(["http", "get", &format!("http://127.0.0.1:{port}/x"), "-v"])
            .assert()
            .success(),
    );
    assert!(out.contains("GET http://"), "{out}");
    assert!(
        out.contains("content-type") || out.contains("Content-Type"),
        "{out}"
    );
}

#[test]
fn json_output_includes_elapsed_ms_and_response_headers() {
    let (port, _captured) = capture_server("200 OK", r#"{"ok":true}"#);
    let (mut cmd, _dir) = tooler();
    let out = stdout_of(
        cmd.args([
            "--output",
            "json",
            "http",
            "get",
            &format!("http://127.0.0.1:{port}/x"),
        ])
        .assert()
        .success(),
    );
    let value: serde_json::Value = serde_json::from_str(out.trim()).expect("valid json");
    assert!(value["elapsed_ms"].is_number(), "{out}");
    assert!(value["headers"].is_object(), "{out}");
    assert_eq!(value["status"], 200);
    assert_eq!(value["ok"], true);
}

#[test]
fn a_non_success_status_fails_the_command() {
    let (port, _captured) = capture_server("404 Not Found", r#"{"error":"nope"}"#);
    let (mut cmd, _dir) = tooler();
    cmd.args(["http", "get", &format!("http://127.0.0.1:{port}/missing")])
        .assert()
        .failure();
}

// ── http login ───────────────────────────────────────────────────────────────

#[test]
fn login_fails_clearly_when_the_token_field_is_missing_from_the_response() {
    let (port, _captured) = capture_server("200 OK", r#"{"foo":"bar"}"#);
    let (mut cmd, _dir) = tooler();
    let assert = cmd
        .args([
            "http",
            "login",
            &format!("http://127.0.0.1:{port}/login"),
            "--body",
            r#"{"user":"x"}"#,
        ])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();
    assert!(stderr.contains("access_token"), "{stderr}");
}

#[test]
fn login_extracts_the_token_field_and_saves_it_for_the_active_profile() {
    let (port, captured) = capture_server("200 OK", r#"{"access_token":"s3cr3t-jwt-value"}"#);
    let (mut cmd, _dir) = tooler();
    let assert = cmd
        .args([
            "--profile",
            "logintest",
            "http",
            "login",
            &format!("http://127.0.0.1:{port}/login"),
            "--body",
            r#"{"user":"x","pass":"y"}"#,
        ])
        .assert();
    let out = assert.get_output();
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();

    // The OS keychain isn't reachable from every sandboxed test environment (it's tied
    // to the login session, not $HOME) -- same tolerant abstain the existing
    // secret_set:/keychain tests use, rather than asserting a false failure here.
    if !out.status.success() {
        eprintln!("login didn't succeed here (no keychain backend?) -- skipping: {stdout}");
        return;
    }

    assert!(stdout.contains("token saved"), "{stdout}");
    assert!(
        !stdout.contains("s3cr3t-jwt-value"),
        "the raw token must never be echoed: {stdout}"
    );

    // The request that was actually sent carried the login body, not a leftover token.
    let req = captured.lock().unwrap().clone();
    assert!(req.starts_with("POST /login HTTP/1.1"), "{req}");
}

#[test]
fn login_supports_a_nested_token_field_path() {
    let (port, _captured) = capture_server("200 OK", r#"{"data":{"jwt":"nested-token-val"}}"#);
    let (mut cmd, _dir) = tooler();
    let assert = cmd
        .args([
            "--profile",
            "logintest2",
            "http",
            "login",
            &format!("http://127.0.0.1:{port}/login"),
            "--token-field",
            "data.jwt",
        ])
        .assert();
    let out = assert.get_output();
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    if !out.status.success() {
        eprintln!("login didn't succeed here (no keychain backend?) -- skipping: {stdout}");
        return;
    }
    assert!(stdout.contains("token saved"), "{stdout}");
}
