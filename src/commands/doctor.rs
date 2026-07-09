use crate::{commands::ssh::expand_tilde, config, context::Context, output::OutputFormat, secrets};
use anyhow::Result;
use clap::Args;
use colored::Colorize;
use serde::Serialize;

#[derive(Args)]
pub struct DoctorArgs {}

#[derive(Serialize)]
struct DoctorCheck {
    name: &'static str,
    status: Status,
    message: String,
}

#[derive(Serialize, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Status {
    Ok,
    Warn,
    Fail,
}

pub fn run(_args: DoctorArgs, ctx: &Context) -> Result<()> {
    let mut checks = Vec::new();
    checks.extend(check_git());
    checks.push(check_keychain());
    checks.extend(check_ssh(ctx));
    checks.push(check_current_exe());
    checks.push(check_config_summary(ctx));

    let has_fail = checks.iter().any(|c| c.status == Status::Fail);

    if ctx.output == OutputFormat::Json {
        println!(
            "{}",
            serde_json::json!({"checks": checks, "healthy": !has_fail})
        );
        if has_fail {
            std::process::exit(1);
        }
        return Ok(());
    }

    for c in &checks {
        let icon = match c.status {
            Status::Ok => "✓".green().bold(),
            Status::Warn => "⚠".yellow().bold(),
            Status::Fail => "✗".red().bold(),
        };
        println!("{icon} {} — {}", c.name.bold(), c.message);
    }
    if has_fail {
        anyhow::bail!("doctor: one or more checks failed");
    }
    Ok(())
}

fn check_git() -> Vec<DoctorCheck> {
    let installed = std::process::Command::new("git").arg("--version").output();
    let mut out = vec![match installed {
        Ok(o) if o.status.success() => DoctorCheck {
            name: "git",
            status: Status::Ok,
            message: String::from_utf8_lossy(&o.stdout).trim().to_string(),
        },
        _ => DoctorCheck {
            name: "git",
            status: Status::Fail,
            message: "git not found on PATH".into(),
        },
    }];

    let name_set = std::process::Command::new("git")
        .args(["config", "--get", "user.name"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    let email_set = std::process::Command::new("git")
        .args(["config", "--get", "user.email"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    out.push(if name_set && email_set {
        DoctorCheck {
            name: "git_identity",
            status: Status::Ok,
            message: "user.name and user.email configured".into(),
        }
    } else {
        DoctorCheck {
            name: "git_identity",
            status: Status::Warn,
            message: "git user.name/user.email not set".into(),
        }
    });
    out
}

fn check_keychain() -> DoctorCheck {
    const PROBE: &str = "__tooler_doctor_probe__";
    let result = secrets::set_token(PROBE, "probe").and_then(|_| {
        let value = secrets::get_token(PROBE)?;
        secrets::delete_token(PROBE)?;
        Ok(value)
    });
    match result {
        Ok(Some(v)) if v == "probe" => DoctorCheck {
            name: "keychain",
            status: Status::Ok,
            message: "OS keychain read/write round-trip succeeded".into(),
        },
        Ok(_) => DoctorCheck {
            name: "keychain",
            status: Status::Fail,
            message: "keychain round-trip returned an unexpected value".into(),
        },
        Err(e) => DoctorCheck {
            name: "keychain",
            status: Status::Fail,
            message: e.to_string(),
        },
    }
}

fn check_ssh(ctx: &Context) -> Vec<DoctorCheck> {
    let mut out = vec![if std::env::var_os("SSH_AUTH_SOCK").is_some() {
        DoctorCheck {
            name: "ssh_agent",
            status: Status::Ok,
            message: "SSH_AUTH_SOCK is set".into(),
        }
    } else {
        DoctorCheck {
            name: "ssh_agent",
            status: Status::Warn,
            message: "SSH_AUTH_SOCK not set (key-based auth without an agent may still work)"
                .into(),
        }
    }];

    let mut names: Vec<&String> = ctx.config.server.keys().collect();
    names.sort();
    for name in names {
        let Some(key) = ctx.config.server[name].key.as_deref() else {
            continue;
        };
        let path = expand_tilde(key);
        out.push(if path.is_file() {
            DoctorCheck {
                name: "ssh_key",
                status: Status::Ok,
                message: format!("{name}: {} readable", path.display()),
            }
        } else {
            DoctorCheck {
                name: "ssh_key",
                status: Status::Fail,
                message: format!("{name}: {} not found", path.display()),
            }
        });
    }
    out
}

fn check_current_exe() -> DoctorCheck {
    match std::env::current_exe() {
        Ok(p) => DoctorCheck {
            name: "self_exe",
            status: Status::Ok,
            message: p.display().to_string(),
        },
        Err(e) => DoctorCheck {
            name: "self_exe",
            status: Status::Fail,
            message: format!("cannot resolve tooler binary (breaks MCP tool execution): {e}"),
        },
    }
}

fn check_config_summary(ctx: &Context) -> DoctorCheck {
    DoctorCheck {
        name: "config",
        status: Status::Ok,
        message: format!(
            "{} — {} profile(s), {} server(s)",
            config::config_path().display(),
            ctx.config.profile.len(),
            ctx.config.server.len(),
        ),
    }
}
