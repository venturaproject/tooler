use crate::{context::Context, output::OutputFormat};
use anyhow::{Result, bail};
use clap::{Args, Subcommand};
use colored::Colorize;

#[derive(Args)]
pub struct MailArgs {
    #[command(subcommand)]
    pub subcommand: MailSubcommand,
}

#[derive(Subcommand)]
pub enum MailSubcommand {
    /// Send an email over SMTP, either through a configured `mail.<name>` profile (see:
    /// tooler config set mail.<name>.host) or fully inline --host/--user/--password
    Send {
        /// Recipient address(es), comma-separated
        #[arg(long)]
        to: String,
        /// Cc address(es), comma-separated
        #[arg(long)]
        cc: Option<String>,
        /// Bcc address(es), comma-separated
        #[arg(long)]
        bcc: Option<String>,
        #[arg(long)]
        subject: String,
        /// Message body (mutually exclusive with --body-file)
        #[arg(long)]
        body: Option<String>,
        /// Read the message body from a local file (mutually exclusive with --body)
        #[arg(long)]
        body_file: Option<String>,
        /// Send the body as text/html instead of text/plain
        #[arg(long)]
        html: bool,
        /// From address. Defaults to the profile's `from` (or its `user`) when using --server
        #[arg(long)]
        from: Option<String>,
        /// Mail profile to send through (see: tooler config set mail.<name>.host)
        #[arg(long)]
        server: Option<String>,
        /// SMTP host (mutually exclusive with --server)
        #[arg(long)]
        host: Option<String>,
        /// SMTP port (defaults to 587, or 465 when --tls tls; ignored with --server unless
        /// it overrides the profile)
        #[arg(long)]
        port: Option<u16>,
        /// SMTP username (when not using --server)
        #[arg(long)]
        user: Option<String>,
        /// SMTP password [env: TOOLER_MAIL_PASSWORD] (when not using --server)
        #[arg(long, env = "TOOLER_MAIL_PASSWORD")]
        password: Option<String>,
        /// "starttls" | "tls" | "none" — overrides the profile's tls / the port-based default
        #[arg(long)]
        tls: Option<String>,
    },
}

pub fn run(args: MailArgs, ctx: &Context) -> Result<()> {
    match args.subcommand {
        MailSubcommand::Send {
            to,
            cc,
            bcc,
            subject,
            body,
            body_file,
            html,
            from,
            server,
            host,
            port,
            user,
            password,
            tls,
        } => {
            if server.is_some() && host.is_some() {
                bail!("--server and --host are mutually exclusive");
            }
            let body = match (body, body_file) {
                (Some(_), Some(_)) => bail!("--body and --body-file are mutually exclusive"),
                (Some(b), None) => b,
                (None, Some(path)) => std::fs::read_to_string(&path)
                    .map_err(|e| anyhow::anyhow!("reading --body-file '{path}': {e}"))?,
                (None, None) => bail!("one of --body or --body-file is required"),
            };

            // Reuses the exact same credential-resolution and send engine `mail:` playbook
            // tasks use — see `commands::play::{resolve_mail_creds, send_mail}`.
            let creds = crate::commands::play::resolve_mail_creds(
                ctx,
                server.as_deref(),
                host.as_deref(),
                port,
                user.as_deref(),
                password.as_deref(),
                from.as_deref(),
                tls.as_deref(),
            )?;
            let count = crate::commands::play::send_mail(
                &creds,
                &to,
                cc.as_deref(),
                bcc.as_deref(),
                &subject,
                &body,
                html,
            )?;

            if ctx.output == OutputFormat::Json {
                let to_list: Vec<&str> = to.split(',').map(str::trim).collect();
                println!(
                    "{}",
                    serde_json::json!({"sent": true, "to": to_list, "recipients": count})
                );
            } else {
                println!("{} sent to {} recipient(s)", "✓ ok".green().bold(), count);
            }
        }
    }
    Ok(())
}
