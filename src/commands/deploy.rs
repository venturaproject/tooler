use crate::{
    commands::{check, ssh::resolve_server},
    context::Context,
    db,
    output::OutputFormat,
};
use anyhow::{Result, bail};
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
    format!("{}{restart}", db::sudo_prefix(sudo, sudo_pass))
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
    sudo: bool,
    sudo_pass: Option<&str>,
    confirm: bool,
    ctx: &Context,
) -> Result<()> {
    let json = ctx.output == OutputFormat::Json;

    let steps = plan_steps(path, pull, build, restart, health_url, health_retries);
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

    if pull && let Err(e) = db::ssh_exec_capture(&server, &pull_cmd(path)) {
        return fail(json, format!("git pull failed: {e:#}"));
    }

    if let Some(cmd) = build
        && let Err(e) = db::ssh_exec_capture(&server, &build_cmd(path, cmd))
    {
        return fail(json, format!("build failed: {e:#}"));
    }

    if let Some(cmd) = restart
        && let Err(e) = db::ssh_exec_capture(&server, &restart_cmd(cmd, sudo, sudo_pass))
    {
        return fail(json, format!("restart failed: {e:#}"));
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
            return fail(
                json,
                format!(
                    "health check failed after {} attempt(s): {}",
                    health_retries,
                    last_err.unwrap_or_default()
                ),
            );
        }
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
            "echo 'pw' | sudo -S systemctl restart myapp"
        );
    }

    #[test]
    fn plan_steps_only_includes_requested_actions() {
        let steps = plan_steps("/app", true, None, Some("systemctl restart myapp"), None, 3);
        assert_eq!(
            steps,
            vec!["git pull in /app", "restart: systemctl restart myapp"]
        );
    }

    #[test]
    fn plan_steps_empty_when_nothing_requested() {
        assert!(plan_steps("/app", false, None, None, None, 3).is_empty());
    }

    #[test]
    fn plan_steps_includes_health_check_with_retry_count() {
        let steps = plan_steps("/app", false, None, None, Some("https://x.test"), 5);
        assert_eq!(steps, vec!["health check https://x.test (up to 5 retries)"]);
    }
}
