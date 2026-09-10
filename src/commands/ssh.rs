use crate::{config::Server, context::Context};
use anyhow::{Context as _, Result, bail};
use clap::{Args, Subcommand};
use colored::Colorize;
use std::path::{Path, PathBuf};

#[derive(Args)]
pub struct SshArgs {
    #[command(subcommand)]
    pub subcommand: SshSubcommand,
}

#[derive(Subcommand)]
pub enum SshSubcommand {
    /// Test SSH connectivity to a server
    Check {
        /// Server profile name
        server: String,
    },

    /// Execute a command on a remote server
    Exec {
        /// Server profile name
        server: String,
        /// Command to run
        command: String,
        /// Run command with sudo
        #[arg(long)]
        sudo: bool,
    },

    /// Upload a local file to a remote server
    Copy {
        /// Local file path
        local: String,
        /// Destination as server:path (e.g. gdn:/tmp/cert.crt)
        remote: String,
    },

    /// Deploy SSL certificates to a server and reload nginx
    Ssl {
        /// Server profile name
        server: String,
        /// Local .pfx certificate file
        #[arg(long)]
        pfx: String,
        /// Local private key file
        #[arg(long)]
        key: String,
        /// PFX password [env: TOOLER_PFX_PASS]
        #[arg(long, env = "TOOLER_PFX_PASS")]
        pfx_password: Option<String>,
        /// Sudo password for remote operations [env: TOOLER_SUDO_PASS]
        #[arg(long, env = "TOOLER_SUDO_PASS")]
        sudo_pass: Option<String>,
        /// Remote SSL directory [default: server ssl_dir or /etc/nginx/ssl]
        #[arg(long)]
        remote_dir: Option<String>,
        /// Certificate filename on the server [default: wildcard.crt]
        #[arg(long, default_value = "wildcard.crt")]
        cert_name: String,
        /// Key filename on the server [default: wildcard.key]
        #[arg(long, default_value = "wildcard.key")]
        key_name: String,
    },
}

// ── Helpers ───────────────────────────────────────────────────────────────────

pub(crate) fn expand_tilde(path: &str) -> PathBuf {
    if let Some(stripped) = path.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(stripped);
    }
    PathBuf::from(path)
}

pub(crate) fn resolve_server(ctx: &Context, name: &str) -> Result<Server> {
    ctx.config
        .server
        .get(name)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("Server '{}' not found. Add it with: tooler server add {} --host <ip> --user <user> --key <key>", name, name))
}

fn run_ssh(server: &Server, command: &str) -> Result<()> {
    let status = std::process::Command::new("ssh")
        .args(server.ssh_args())
        .arg(server.host_target())
        .arg(command)
        .status()
        .context("Failed to launch ssh — is it installed?")?;

    if !status.success() {
        bail!("SSH command failed (exit {})", status.code().unwrap_or(1));
    }
    Ok(())
}

pub(crate) fn run_scp(server: &Server, local: &Path, remote_path: &str) -> Result<()> {
    let dest = format!("{}:{}", server.host_target(), remote_path);
    let scp_args = server.scp_args();

    let status = std::process::Command::new("scp")
        .args(&scp_args)
        .arg(local)
        .arg(&dest)
        .status()
        .context("Failed to launch scp — is it installed?")?;

    if !status.success() {
        bail!("scp failed (exit {})", status.code().unwrap_or(1));
    }
    Ok(())
}

// ── Entrypoint ────────────────────────────────────────────────────────────────

pub fn run(args: SshArgs, ctx: &Context) -> Result<()> {
    match args.subcommand {
        SshSubcommand::Check { server } => check(ctx, &server),
        SshSubcommand::Exec {
            server,
            command,
            sudo,
        } => exec(ctx, &server, &command, sudo),
        SshSubcommand::Copy { local, remote } => copy(ctx, &local, &remote),
        SshSubcommand::Ssl {
            server,
            pfx,
            key,
            pfx_password,
            sudo_pass,
            remote_dir,
            cert_name,
            key_name,
        } => ssl(
            ctx,
            &server,
            &pfx,
            &key,
            pfx_password.as_deref(),
            sudo_pass.as_deref(),
            remote_dir.as_deref(),
            &cert_name,
            &key_name,
        ),
    }
}

// ── Commands ──────────────────────────────────────────────────────────────────

fn check(ctx: &Context, name: &str) -> Result<()> {
    let server = resolve_server(ctx, name)?;
    print!(
        "checking {} ({})... ",
        name.cyan(),
        server.host_target().dimmed()
    );

    let status = std::process::Command::new("ssh")
        .args(server.ssh_args())
        .arg("-o")
        .arg("ConnectTimeout=5")
        .arg(server.host_target())
        .arg("echo ok")
        .output()
        .context("Failed to launch ssh")?;

    if status.status.success() {
        println!("{}", "✓ connected".green().bold());
        Ok(())
    } else {
        println!("{}", "✗ failed".red().bold());
        bail!("{}", String::from_utf8_lossy(&status.stderr).trim())
    }
}

fn exec(ctx: &Context, name: &str, command: &str, sudo: bool) -> Result<()> {
    let server = resolve_server(ctx, name)?;
    let full_cmd = if sudo {
        format!("sudo {command}")
    } else {
        command.to_string()
    };
    println!(
        "{} {} {}",
        "→".bold(),
        server.host_target().cyan(),
        full_cmd.dimmed()
    );
    run_ssh(&server, &full_cmd)
}

fn copy(ctx: &Context, local: &str, remote: &str) -> Result<()> {
    let (server_name, remote_path) = remote.split_once(':').ok_or_else(|| {
        anyhow::anyhow!("Remote must be in format server:path (e.g. gdn:/tmp/file)")
    })?;

    let server = resolve_server(ctx, server_name)?;
    println!(
        "{} {} → {}:{}",
        "→".bold(),
        local.dimmed(),
        server_name.cyan(),
        remote_path.dimmed()
    );
    run_scp(&server, Path::new(local), remote_path)?;
    println!("{}", "✓ uploaded".green().bold());
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn ssl(
    ctx: &Context,
    name: &str,
    pfx_path: &str,
    key_path: &str,
    pfx_password: Option<&str>,
    sudo_pass: Option<&str>,
    remote_dir: Option<&str>,
    cert_name: &str,
    key_name: &str,
) -> Result<()> {
    let server = resolve_server(ctx, name)?;
    let ssl_dir = remote_dir
        .or(server.ssl_dir.as_deref())
        .unwrap_or("/etc/nginx/ssl");

    println!(
        "{} {}",
        "SSL deploy →".bold().cyan(),
        server.host_target().green()
    );
    println!("{}", "─".repeat(50).dimmed());

    // Step 1: Extract certificate from PFX locally
    let tmp_cert = std::env::temp_dir().join("tooler_ssl_cert.crt");
    println!(
        "{} Extracting certificate from PFX...",
        "1/5".bold().dimmed()
    );

    let mut openssl_cmd = std::process::Command::new("openssl");
    openssl_cmd
        .args(["pkcs12", "-in", pfx_path, "-clcerts", "-nokeys", "-out"])
        .arg(&tmp_cert)
        .arg("-legacy");

    if let Some(pass) = pfx_password {
        openssl_cmd.args(["-password", &format!("pass:{pass}")]);
    } else {
        openssl_cmd.args(["-password", "pass:"]);
    }

    let result = openssl_cmd
        .status()
        .context("Failed to run openssl — is it installed?")?;
    if !result.success() {
        bail!("openssl pkcs12 extraction failed. Check your PFX password.");
    }
    println!("     {} certificate extracted", "✓".green());

    // Step 2: Upload certificate
    println!("{} Uploading certificate...", "2/5".bold().dimmed());
    run_scp(&server, &tmp_cert, &format!("/tmp/{cert_name}"))?;
    println!("     {} /tmp/{cert_name}", "✓".green());

    // Step 3: Upload private key
    println!("{} Uploading private key...", "3/5".bold().dimmed());
    run_scp(&server, Path::new(key_path), &format!("/tmp/{key_name}"))?;
    println!("     {} /tmp/{key_name}", "✓".green());

    // Step 4: Move files into ssl_dir with correct permissions
    println!("{} Installing on server...", "4/5".bold().dimmed());

    let sudo_prefix = match sudo_pass {
        Some(pass) => format!("echo '{pass}' | sudo -S"),
        None => "sudo".to_string(),
    };

    let install_cmd = format!(
        "mkdir -p {ssl_dir} && \
         {sudo_prefix} mv /tmp/{cert_name} {ssl_dir}/{cert_name} && \
         {sudo_prefix} mv /tmp/{key_name} {ssl_dir}/{key_name} && \
         {sudo_prefix} chmod 644 {ssl_dir}/{cert_name} && \
         {sudo_prefix} chmod 600 {ssl_dir}/{key_name}",
    );
    run_ssh(&server, &install_cmd)?;
    println!("     {} files installed in {ssl_dir}", "✓".green());

    // Step 5: Verify nginx config and reload
    println!("{} Verifying and reloading nginx...", "5/5".bold().dimmed());
    let reload_cmd = format!("{sudo_prefix} nginx -t && {sudo_prefix} systemctl reload nginx");
    run_ssh(&server, &reload_cmd)?;
    println!("     {} nginx reloaded", "✓".green());

    // Cleanup local temp file
    let _ = std::fs::remove_file(&tmp_cert);

    println!(
        "\n{} SSL certificate deployed successfully",
        "✓".green().bold()
    );
    Ok(())
}
