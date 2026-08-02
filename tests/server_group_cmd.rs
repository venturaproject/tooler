mod common;

use common::tooler_in;
use tempfile::tempdir;

fn stdout_of(assert: assert_cmd::assert::Assert) -> String {
    String::from_utf8_lossy(&assert.get_output().stdout).to_string()
}

#[test]
fn server_add_list_show_remove() {
    let dir = tempdir().unwrap();
    tooler_in(dir.path())
        .args(["server", "add", "web1", "--host", "1.2.3.4"])
        .assert()
        .success();

    let list = stdout_of(
        tooler_in(dir.path())
            .args(["server", "list"])
            .assert()
            .success(),
    );
    assert!(list.contains("web1"));

    let show = stdout_of(
        tooler_in(dir.path())
            .args(["server", "show", "web1"])
            .assert()
            .success(),
    );
    assert!(show.contains("1.2.3.4"));

    tooler_in(dir.path())
        .args(["server", "remove", "web1"])
        .assert()
        .success();
    tooler_in(dir.path())
        .args(["server", "show", "web1"])
        .assert()
        .failure();
}

#[test]
fn group_add_rejects_unknown_member() {
    let dir = tempdir().unwrap();
    tooler_in(dir.path())
        .args(["group", "add", "web", "--members", "bogus"])
        .assert()
        .failure();
}

#[test]
fn group_add_list_show_remove_with_valid_member() {
    let dir = tempdir().unwrap();
    tooler_in(dir.path())
        .args(["server", "add", "web1", "--host", "1.2.3.4"])
        .assert()
        .success();
    tooler_in(dir.path())
        .args(["server", "add", "web2", "--host", "5.6.7.8"])
        .assert()
        .success();

    tooler_in(dir.path())
        .args(["group", "add", "web", "--members", "web1,web2"])
        .assert()
        .success();

    let list = stdout_of(
        tooler_in(dir.path())
            .args(["group", "list"])
            .assert()
            .success(),
    );
    assert!(list.contains("web"));

    let show = stdout_of(
        tooler_in(dir.path())
            .args(["group", "show", "web"])
            .assert()
            .success(),
    );
    assert!(show.contains("web1"));
    assert!(show.contains("web2"));

    tooler_in(dir.path())
        .args(["group", "remove", "web"])
        .assert()
        .success();
    tooler_in(dir.path())
        .args(["group", "show", "web"])
        .assert()
        .failure();
}
