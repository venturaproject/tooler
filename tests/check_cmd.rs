mod common;

use common::tooler;
use std::net::TcpListener;

#[test]
fn port_fails_against_a_closed_port() {
    let (mut cmd, _dir) = tooler();
    // Bind then immediately drop, so the OS is very likely to have nothing listening
    // there — a closed port needs no server at all.
    let port = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    cmd.args(["check", "port", "127.0.0.1", &port.to_string(), "-t", "1"])
        .assert()
        .failure();
}

#[test]
fn port_succeeds_against_a_listening_port() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    // Keep the listener alive for the duration of the check by accepting in a thread.
    std::thread::spawn(move || {
        let _ = listener.accept();
    });

    let (mut cmd, _dir) = tooler();
    cmd.args(["check", "port", "127.0.0.1", &port.to_string()])
        .assert()
        .success();
}

#[test]
fn url_fails_against_an_unreachable_host() {
    let (mut cmd, _dir) = tooler();
    let port = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    cmd.args([
        "check",
        "url",
        &format!("http://127.0.0.1:{port}/"),
        "-t",
        "1",
    ])
    .assert()
    .failure();
}
