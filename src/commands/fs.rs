use crate::{commands::ssh::resolve_server, context::Context, db, output::OutputFormat};
use anyhow::{Result, bail};
use clap::{Args, Subcommand};
use colored::Colorize;

#[derive(Args)]
pub struct FsArgs {
    #[command(subcommand)]
    pub subcommand: FsSubcommand,
}

#[derive(Subcommand)]
pub enum FsSubcommand {
    /// Print a remote file's contents
    Cat {
        /// Server profile (see: tooler server list)
        server: String,
        /// Remote file path
        path: String,
    },
    /// Overwrite a remote file. Preview-only unless --confirm is passed
    Write {
        server: String,
        path: String,
        /// Local file to read the new content from (binary-safe)
        #[arg(long, conflicts_with = "content")]
        from_file: Option<String>,
        /// Literal text to write
        #[arg(long, conflicts_with = "from_file")]
        content: Option<String>,
        /// Actually write the file (default is preview-only)
        #[arg(long)]
        confirm: bool,
    },
    /// Diff a remote file against a local file (unified diff)
    Diff {
        server: String,
        /// Remote file path
        path: String,
        /// Local file to compare against
        #[arg(long)]
        local: String,
    },
}

pub fn run(args: FsArgs, ctx: &Context) -> Result<()> {
    match args.subcommand {
        FsSubcommand::Cat { server, path } => cat(&server, &path, ctx),
        FsSubcommand::Write {
            server,
            path,
            from_file,
            content,
            confirm,
        } => write(
            &server,
            &path,
            from_file.as_deref(),
            content.as_deref(),
            confirm,
            ctx,
        ),
        FsSubcommand::Diff {
            server,
            path,
            local,
        } => diff(&server, &path, &local, ctx),
    }
}

fn fail(json: bool, message: String) -> Result<()> {
    if json {
        println!("{}", serde_json::json!({ "error": message }));
        std::process::exit(1);
    }
    bail!(message);
}

pub(crate) fn cat_cmd(path: &str) -> String {
    format!("cat {}", db::shell_quote(path))
}

pub(crate) fn write_cmd(path: &str) -> String {
    format!("cat > {}", db::shell_quote(path))
}

fn cat(server_name: &str, path: &str, ctx: &Context) -> Result<()> {
    let json = ctx.output == OutputFormat::Json;

    let server = match resolve_server(ctx, server_name) {
        Ok(s) => s,
        Err(e) => return fail(json, format!("{e:#}")),
    };
    let content = match db::ssh_exec_capture(&server, &cat_cmd(path)) {
        Ok(c) => c,
        Err(e) => return fail(json, format!("{e:#}")),
    };

    if json {
        println!(
            "{}",
            serde_json::json!({
                "server": server_name,
                "path": path,
                "bytes": content.len(),
                "content": content,
            })
        );
        return Ok(());
    }
    print!("{content}");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn write(
    server_name: &str,
    path: &str,
    from_file: Option<&str>,
    content: Option<&str>,
    confirm: bool,
    ctx: &Context,
) -> Result<()> {
    let json = ctx.output == OutputFormat::Json;

    let bytes = match (from_file, content) {
        (Some(f), None) => match std::fs::read(f) {
            Ok(b) => b,
            Err(e) => return fail(json, format!("reading {f}: {e}")),
        },
        (None, Some(c)) => c.as_bytes().to_vec(),
        (None, None) => {
            return fail(
                json,
                "specify exactly one of --from-file or --content".to_string(),
            );
        }
        (Some(_), Some(_)) => {
            return fail(
                json,
                "specify only one of --from-file or --content".to_string(),
            );
        }
    };

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
                    "bytes": bytes.len(),
                    "confirmed": false,
                })
            );
            return Ok(());
        }
        println!(
            "Would write {} bytes to {} on {}. Re-run with --confirm to apply.",
            bytes.len(),
            path.dimmed(),
            server_name.cyan()
        );
        return Ok(());
    }

    let (_, stderr, success) = match db::ssh_exec_with_stdin(&server, &write_cmd(path), &bytes) {
        Ok(r) => r,
        Err(e) => return fail(json, format!("{e:#}")),
    };
    if !success {
        return fail(json, format!("write failed: {}", stderr.trim()));
    }

    if json {
        println!(
            "{}",
            serde_json::json!({
                "server": server_name,
                "path": path,
                "written": true,
                "bytes": bytes.len(),
            })
        );
        return Ok(());
    }
    println!(
        "{} wrote {} bytes to {} on {}",
        "✓".green().bold(),
        bytes.len(),
        path.dimmed(),
        server_name.cyan()
    );
    Ok(())
}

fn diff(server_name: &str, path: &str, local: &str, ctx: &Context) -> Result<()> {
    let json = ctx.output == OutputFormat::Json;

    let server = match resolve_server(ctx, server_name) {
        Ok(s) => s,
        Err(e) => return fail(json, format!("{e:#}")),
    };
    let remote_content = match db::ssh_exec_capture(&server, &cat_cmd(path)) {
        Ok(c) => c,
        Err(e) => return fail(json, format!("{e:#}")),
    };

    let tmp_path =
        std::env::temp_dir().join(format!("tooler_fs_diff_{}_{}.tmp", std::process::id(), {
            use std::time::{SystemTime, UNIX_EPOCH};
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        }));
    if let Err(e) = std::fs::write(&tmp_path, &remote_content) {
        return fail(json, format!("writing temp file: {e}"));
    }

    let output = std::process::Command::new("diff")
        .arg("-u")
        .arg(local)
        .arg(&tmp_path)
        .output();
    let _ = std::fs::remove_file(&tmp_path);

    let output = match output {
        Ok(o) => o,
        Err(e) => {
            return fail(
                json,
                format!("failed to launch diff — is it installed? {e}"),
            );
        }
    };

    match output.status.code() {
        Some(0) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "server": server_name,
                        "path": path,
                        "local": local,
                        "identical": true,
                        "diff": null,
                    })
                );
                return Ok(());
            }
            println!("{} identical", "✓".green().bold());
            Ok(())
        }
        Some(1) => {
            let diff_text = String::from_utf8_lossy(&output.stdout).into_owned();
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "server": server_name,
                        "path": path,
                        "local": local,
                        "identical": false,
                        "diff": diff_text,
                    })
                );
                return Ok(());
            }
            print!("{diff_text}");
            Ok(())
        }
        _ => fail(
            json,
            format!(
                "diff failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cat_cmd_quotes_path_with_spaces() {
        assert_eq!(
            cat_cmd("/etc/my app/config.yml"),
            "cat '/etc/my app/config.yml'"
        );
    }

    #[test]
    fn write_cmd_quotes_path() {
        assert_eq!(write_cmd("/tmp/it's.conf"), "cat > '/tmp/it'\\''s.conf'");
    }
}
