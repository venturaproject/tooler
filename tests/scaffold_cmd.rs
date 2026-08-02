mod common;

use common::tooler;

fn stdout_of(assert: assert_cmd::assert::Assert) -> String {
    String::from_utf8_lossy(&assert.get_output().stdout).to_string()
}

#[test]
fn list_shows_the_builtin_templates() {
    let (mut cmd, _dir) = tooler();
    let out = stdout_of(cmd.args(["scaffold", "list"]).assert().success());
    assert!(out.contains("rust-cli"));
}

#[test]
fn new_creates_the_project_directory_and_files() {
    let (mut cmd, dir) = tooler();
    cmd.args(["scaffold", "new", "rust-cli", "myproj"])
        .assert()
        .success();

    let project_dir = dir.path().join("myproj");
    assert!(project_dir.join("Cargo.toml").exists());
    assert!(project_dir.join("src/main.rs").exists());
}

#[test]
fn new_fails_when_destination_already_exists() {
    let (mut cmd, dir) = tooler();
    std::fs::create_dir(dir.path().join("myproj")).unwrap();
    cmd.args(["scaffold", "new", "rust-cli", "myproj"])
        .assert()
        .failure();
}
