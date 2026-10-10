/// Creates the platform's standard command interpreter with autorun disabled on Windows.
pub(crate) fn command(script: &str) -> std::process::Command {
    #[cfg(windows)]
    {
        let mut command = std::process::Command::new("cmd");
        command.args(["/D", "/S", "/C", script]);
        command
    }
    #[cfg(not(windows))]
    {
        let mut command = std::process::Command::new("sh");
        command.args(["-c", script]);
        command
    }
}
