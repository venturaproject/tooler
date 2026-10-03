use crate::{
    commands::{check, ssh::resolve_server},
    context::Context,
    db,
    output::OutputFormat,
};
use anyhow::{Context as _, Result, bail};
use clap::Args;
use colored::Colorize;
use std::time::Duration;

#[derive(Args)]
pub struct DeployArgs {
    /// Server profile (see: tooler server list)
    pub server: String,
    /// Remote path (git repo) to deploy
    #[arg(long)]
    pub path: String,
    /// Pull the latest code (git pull) in --path before restarting
    #[arg(long)]
    pub pull: bool,
    /// Command to run remotely in --path after pulling (e.g. a build step)
    #[arg(long)]
    pub build: Option<String>,
    /// Command to restart the service (e.g. "systemctl restart myapp")
    #[arg(long)]
    pub restart: Option<String>,
    /// URL to check after restarting
    #[arg(long)]
    pub health_url: Option<String>,
    /// Timeout in seconds for each health check attempt
    #[arg(long, default_value_t = 5)]
    pub health_timeout: u64,
    /// Number of health check attempts before giving up
    #[arg(long, default_value_t = 3)]
    pub health_retries: u32,
    /// Seconds to wait between health check attempts
    #[arg(long, default_value_t = 2)]
    pub health_delay: u64,
    /// Reset to the revision that was deployed before `--pull` if a later step fails
    #[arg(long)]
    pub rollback_on_failure: bool,
    /// Run the restart command via sudo
    #[arg(long)]
    pub sudo: bool,
    /// Sudo password [env: TOOLER_SUDO_PASS] (only used with --sudo; omit to rely on NOPASSWD)
    #[arg(long, env = "TOOLER_SUDO_PASS")]
    pub sudo_pass: Option<String>,
    /// Actually run the deploy (default is preview-only)
    #[arg(long)]
    pub confirm: bool,
}

pub fn run(args: DeployArgs, ctx: &Context) -> Result<()> {
    deploy(
        &args.server,
        &args.path,
        args.pull,
        args.build.as_deref(),
        args.restart.as_deref(),
        args.health_url.as_deref(),
        args.health_timeout,
        args.health_retries,
        args.health_delay,
        args.rollback_on_failure,
        args.sudo,
        args.sudo_pass.as_deref(),
        args.confirm,
        ctx,
    )
}

fn fail(json: bool, message: String) -> Result<()> {
    if json {
        println!("{}", serde_json::json!({ "error": message }));
        std::process::exit(1);
    }
    bail!(message);
}

fn pull_cmd(path: &str) -> String {
    format!("cd {} && git pull", db::shell_quote(path))
}

fn build_cmd(path: &str, build: &str) -> String {
    format!("cd {} && {build}", db::shell_quote(path))
}

fn restart_cmd(restart: &str, sudo: bool, sudo_pass: Option<&str>) -> String {
    format!("{}{restart}", db::sudo_prefix(sudo, sudo_pass.is_some()))
}

fn revision_cmd(path: &str) -> String {
    format!("cd {} && git rev-parse HEAD", db::shell_quote(path))
}

fn rollback_cmd(path: &str, revision: &str) -> String {
    format!(
        "cd {} && git reset --hard {}",
        db::shell_quote(path),
        db::shell_quote(revision)
    )
}

/// Human-readable description of each requested step, in execution order --
/// shared by the preview (--confirm not passed) and the plain-text step log.
fn plan_steps(
    path: &str,
    pull: bool,
    build: Option<&str>,
    restart: Option<&str>,
    health_url: Option<&str>,
    health_retries: u32,
    rollback_on_failure: bool,
) -> Vec<String> {
    let mut steps = Vec::new();
    if pull {
        steps.push(format!("git pull in {path}"));
    }
    if let Some(cmd) = build {
        steps.push(format!("run build: {cmd}"));
    }
    if let Some(cmd) = restart {
        steps.push(format!("restart: {cmd}"));
    }
    if let Some(url) = health_url {
        steps.push(format!(
            "health check {url} (up to {health_retries} retries)"
        ));
    }
    if rollback_on_failure {
        steps.push("on failure: reset to the revision before git pull and restart".to_string());
    }
    steps
}

#[allow(clippy::too_many_arguments)]
fn deploy(
    server_name: &str,
    path: &str,
    pull: bool,
    build: Option<&str>,
    restart: Option<&str>,
    health_url: Option<&str>,
    health_timeout: u64,
    health_retries: u32,
    health_delay: u64,
    rollback_on_failure: bool,
    sudo: bool,
    sudo_pass: Option<&str>,
    confirm: bool,
    ctx: &Context,
) -> Result<()> {
    let json = ctx.output == OutputFormat::Json;

    let steps = plan_steps(
        path,
        pull,
        build,
        restart,
        health_url,
        health_retries,
        rollback_on_failure,
    );
    if steps.is_empty() {
        return fail(
            json,
            "nothing to do — pass at least one of --pull, --build, --restart, --health-url"
                .to_string(),
        );
    }

    let server = match resolve_server(ctx, server_name) {
        Ok(s) => s,
        Err(e) => return fail(json, format!("{e:#}")),
    };

    if !confirm {
        if json {
            println!(
                "{}",
                serde_json::json!({
                    "server": server_name,
                    "path": path,
                    "plan": steps,
                    "confirmed": false,
                })
            );
            return Ok(());
        }
        println!(
            "Would run {} step(s) against {}:",
            steps.len(),
            server_name.cyan()
        );
        for (i, step) in steps.iter().enumerate() {
            println!("  {}) {}", i + 1, step);
        }
        println!("Re-run with --confirm to apply.");
        return Ok(());
    }

    if let Err(e) = apply_deploy_steps(
        &server,
        path,
        pull,
        build,
        restart,
        health_url,
        health_timeout,
        health_retries,
        health_delay,
        rollback_on_failure,
        sudo,
        sudo_pass,
    ) {
        return fail(json, e.to_string());
    }

    if json {
        println!(
            "{}",
            serde_json::json!({"server": server_name, "path": path, "deployed": true})
        );
        return Ok(());
    }
    println!(
        "{} deployed {} on {}",
        "✓".green().bold(),
        path.dimmed(),
        server_name.cyan()
    );
    Ok(())
}

/// Runs `deploy`'s actual side effects (pull/build/restart/health-check) against an
/// already-resolved server -- the caller is responsible for any confirm-gating.
/// Shared by the standalone `tooler deploy run` CLI command (`deploy`, above) and
/// `tooler play`'s native `deploy:` task, so both go through identical step logic.
/// Error messages are stable text ("git pull failed: ...", "build failed: ...",
/// "restart failed: ...", "health check failed after N attempt(s): ...") -- both
/// callers surface them verbatim.
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_deploy_steps(
    server: &crate::config::Server,
    path: &str,
    pull: bool,
    build: Option<&str>,
    restart: Option<&str>,
    health_url: Option<&str>,
    health_timeout: u64,
    health_retries: u32,
    health_delay: u64,
    rollback_on_failure: bool,
    sudo: bool,
    sudo_pass: Option<&str>,
) -> Result<()> {
    if rollback_on_failure && !pull {
        bail!("--rollback-on-failure requires --pull so tooler can restore a prior revision");
    }
    let previous_revision = if rollback_on_failure {
        Some(
            db::ssh_exec_capture(server, &revision_cmd(path))
                .context("could not determine the currently deployed git revision")?
                .trim()
                .to_string(),
        )
    } else {
        None
    };
    let deploy_result = apply_deploy_steps_inner(
        server,
        path,
        pull,
        build,
        restart,
        health_url,
        health_timeout,
        health_retries,
        health_delay,
        sudo,
        sudo_pass,
    );
    if let Err(deploy_error) = deploy_result {
        if let Some(revision) = previous_revision {
            let rollback =
                db::ssh_exec_capture(server, &rollback_cmd(path, &revision)).and_then(|_| {
                    match restart {
                        Some(cmd) => db::ssh_exec_capture_with_sudo_password(
                            server,
                            &restart_cmd(cmd, sudo, sudo_pass),
                            sudo_pass,
                        )
                        .map(|_| ()),
                        None => Ok(()),
                    }
                });
            return match rollback {
                Ok(()) => Err(deploy_error.context(format!(
                    "deployment rolled back to {revision} after failure"
                ))),
                Err(rollback_error) => Err(deploy_error.context(format!(
                    "deployment failed and rollback to {revision} also failed: {rollback_error:#}"
                ))),
            };
        }
        return Err(deploy_error);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn apply_deploy_steps_inner(
    server: &crate::config::Server,
    path: &str,
    pull: bool,
    build: Option<&str>,
    restart: Option<&str>,
    health_url: Option<&str>,
    health_timeout: u64,
    health_retries: u32,
    health_delay: u64,
    sudo: bool,
    sudo_pass: Option<&str>,
) -> Result<()> {
    if pull && let Err(e) = db::ssh_exec_capture(server, &pull_cmd(path)) {
        bail!("git pull failed: {e:#}");
    }

    if let Some(cmd) = build
        && let Err(e) = db::ssh_exec_capture(server, &build_cmd(path, cmd))
    {
        bail!("build failed: {e:#}");
    }

    if let Some(cmd) = restart
        && let Err(e) = db::ssh_exec_capture_with_sudo_password(
            server,
            &restart_cmd(cmd, sudo, sudo_pass),
            sudo_pass,
        )
    {
        bail!("restart failed: {e:#}");
    }

    if let Some(url) = health_url {
        let mut last_err = None;
        let mut healthy = false;
        for attempt in 0..health_retries {
            match check::probe_url(url, health_timeout) {
                Ok(status) if (200..300).contains(&status) => {
                    healthy = true;
                    break;
                }
                Ok(status) => last_err = Some(format!("HTTP {status}")),
                Err(e) => last_err = Some(e.to_string()),
            }
            if attempt + 1 < health_retries {
                std::thread::sleep(Duration::from_secs(health_delay));
            }
        }
        if !healthy {
            bail!(
                "health check failed after {} attempt(s): {}",
                health_retries,
                last_err.unwrap_or_default()
            );
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pull_cmd_quotes_path() {
        assert_eq!(pull_cmd("/var/www/app"), "cd '/var/www/app' && git pull");
    }

    #[test]
    fn build_cmd_quotes_path_not_command() {
        assert_eq!(
            build_cmd("/var/www/app", "cargo build --release"),
            "cd '/var/www/app' && cargo build --release"
        );
    }

    #[test]
    fn restart_cmd_without_sudo() {
        assert_eq!(
            restart_cmd("systemctl restart myapp", false, None),
            "systemctl restart myapp"
        );
    }

    #[test]
    fn restart_cmd_with_sudo_and_password() {
        assert_eq!(
            restart_cmd("systemctl restart myapp", true, Some("pw")),
            "sudo -S systemctl restart myapp"
        );
    }

    #[test]
    fn plan_steps_only_includes_requested_actions() {
        let steps = plan_steps(
            "/app",
            true,
            None,
            Some("systemctl restart myapp"),
            None,
            3,
            false,
        );
        assert_eq!(
            steps,
            vec!["git pull in /app", "restart: systemctl restart myapp"]
        );
    }

    #[test]
    fn plan_steps_empty_when_nothing_requested() {
        assert!(plan_steps("/app", false, None, None, None, 3, false).is_empty());
    }

    #[test]
    fn plan_steps_includes_health_check_with_retry_count() {
        let steps = plan_steps("/app", false, None, None, Some("https://x.test"), 5, false);
        assert_eq!(steps, vec!["health check https://x.test (up to 5 retries)"]);
    }

    #[test]
    fn rollback_commands_are_confined_to_the_deploy_repository() {
        assert_eq!(
            revision_cmd("/var/www/app"),
            "cd '/var/www/app' && git rev-parse HEAD"
        );
        assert_eq!(
            rollback_cmd("/var/www/app", "abc123"),
            "cd '/var/www/app' && git reset --hard 'abc123'"
        );
    }
}
