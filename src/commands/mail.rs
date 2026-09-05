use crate::{commands::play::ImapCreds, context::Context, output::OutputFormat};
use anyhow::{Context as _, Result, bail};
use clap::{Args, Subcommand};
use colored::Colorize;
use serde::Serialize;
use std::path::PathBuf;

#[derive(Args)]
pub struct MailArgs {
    #[command(subcommand)]
    pub subcommand: MailSubcommand,
}

// `Send`'s field count dwarfs `Check`'s, but this is a CLI arg-parsing enum -- one
// value lives on the stack for the length of a single `tooler mail` invocation, never
// stored in a `Vec`/collection where the size difference would actually cost anything.
// Boxing individual clap-derived `Option<String>` fields (clippy's own suggestion) would
// only complicate the arg-parsing derive for no real benefit here.
#[allow(clippy::large_enum_variant)]
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
        /// Local file path to attach (repeatable)
        #[arg(long = "attach")]
        attach: Vec<String>,
    },
    /// Read a mail profile's inbox over IMAP (defaults to unseen messages only)
    Check {
        /// Mail profile to read from (see: tooler config set mail.<name>.imap_port, etc)
        #[arg(long)]
        server: String,
        #[arg(long, default_value = "INBOX")]
        folder: String,
        /// Fetch every message in the folder, not just unseen ones (default: unseen only)
        #[arg(long)]
        all: bool,
        /// Fetch each message's plain-text body too, not just headers
        #[arg(long)]
        include_body: bool,
        #[arg(long, default_value_t = 10)]
        limit: u32,
        /// Mark fetched messages \Seen afterward, so a later --unseen-only run doesn't
        /// see them again
        #[arg(long)]
        mark_seen: bool,
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
            attach,
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
            let attachments: Vec<PathBuf> = attach.iter().map(PathBuf::from).collect();
            let count = crate::commands::play::send_mail(
                &creds,
                &to,
                cc.as_deref(),
                bcc.as_deref(),
                &subject,
                &body,
                html,
                &attachments,
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
        MailSubcommand::Check {
            server,
            folder,
            all,
            include_body,
            limit,
            mark_seen,
        } => {
            // Reuses the exact same credential-resolution `mail_check:` playbook tasks
            // use — see `commands::play::resolve_imap_creds`.
            let creds = crate::commands::play::resolve_imap_creds(ctx, &server)?;
            let messages = fetch_mail(&creds, &folder, !all, limit, include_body, mark_seen)?;

            if ctx.output == OutputFormat::Json {
                println!(
                    "{}",
                    serde_json::json!({"server": server, "messages": messages})
                );
                return Ok(());
            }
            if messages.is_empty() {
                println!("{}", "(no messages)".dimmed());
                return Ok(());
            }
            for m in &messages {
                println!(
                    "  {} {} {} {}",
                    format!("#{}", m.uid).dimmed(),
                    m.date.dimmed(),
                    m.from.cyan(),
                    m.subject
                );
            }
        }
    }
    Ok(())
}

/// One fetched IMAP message, headers-only unless `include_body` was requested.
#[derive(Debug, Serialize)]
pub(crate) struct MailMessage {
    pub(crate) uid: u32,
    pub(crate) from: String,
    pub(crate) subject: String,
    pub(crate) date: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) body: Option<String>,
}

/// The one place `imap` is touched. Connects, logs in, selects `folder`, searches
/// (`UNSEEN` or `ALL`), fetches up to `limit` of the most recent matching messages
/// (highest UIDs), and — if `mark_seen` — flags them `\Seen` afterward so a later
/// `unseen_only` run doesn't reprocess them. Always logs out, even on a mid-fetch error,
/// so a failed run never leaves a dangling IMAP session on the server.
///
/// Header/body decoding is best-effort: `subject`/`from`/`date`/`body` are read as raw
/// bytes via `String::from_utf8_lossy` -- a subject using RFC 2047 encoded-words
/// (non-ASCII via `=?UTF-8?B?...?=`) or a body sent quoted-printable/base64 comes back
/// un-decoded rather than as readable text. Good enough for the common case (ASCII
/// subjects, most transactional/notification mail) without pulling in a full
/// MIME-decoding dependency for a v1 feature.
pub(crate) fn fetch_mail(
    creds: &ImapCreds,
    folder: &str,
    unseen_only: bool,
    limit: u32,
    include_body: bool,
    mark_seen: bool,
) -> Result<Vec<MailMessage>> {
    let client = imap::ClientBuilder::new(creds.host.clone(), creds.port)
        .connect()
        .with_context(|| format!("connecting to {}:{}", creds.host, creds.port))?;
    let mut session = client
        .login(&creds.user, &creds.password)
        .map_err(|(e, _)| e)
        .with_context(|| format!("logging into {} as {}", creds.host, creds.user))?;

    let result = fetch_mail_inner(
        &mut session,
        folder,
        unseen_only,
        limit,
        include_body,
        mark_seen,
    );
    let _ = session.logout();
    result
}

fn fetch_mail_inner(
    session: &mut imap::Session<imap::Connection>,
    folder: &str,
    unseen_only: bool,
    limit: u32,
    include_body: bool,
    mark_seen: bool,
) -> Result<Vec<MailMessage>> {
    session
        .select(folder)
        .with_context(|| format!("selecting folder '{folder}'"))?;

    let query = if unseen_only { "UNSEEN" } else { "ALL" };
    let mut uids: Vec<u32> = session
        .uid_search(query)
        .with_context(|| format!("searching '{query}' in '{folder}'"))?
        .into_iter()
        .collect();
    uids.sort_unstable();
    let limit = limit as usize;
    if uids.len() > limit {
        // Keep the most recent `limit` (highest UIDs), not an arbitrary head slice.
        uids = uids.split_off(uids.len() - limit);
    }
    if uids.is_empty() {
        return Ok(Vec::new());
    }

    let uid_set = uids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let fetch_query = if include_body {
        "(ENVELOPE BODY.PEEK[TEXT])"
    } else {
        "(ENVELOPE)"
    };
    let fetches = session
        .uid_fetch(&uid_set, fetch_query)
        .context("fetching message envelopes")?;

    let mut messages = Vec::with_capacity(fetches.iter().count());
    for fetch in fetches.iter() {
        let envelope = fetch.envelope();
        let from = envelope
            .and_then(|e| e.from.as_ref())
            .and_then(|addrs| addrs.first())
            .map(format_address)
            .unwrap_or_default();
        let subject = envelope
            .and_then(|e| e.subject.as_ref())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .unwrap_or_default();
        let date = envelope
            .and_then(|e| e.date.as_ref())
            .map(|d| String::from_utf8_lossy(d).into_owned())
            .unwrap_or_default();
        let body = if include_body {
            fetch
                .text()
                .map(|b| String::from_utf8_lossy(b).into_owned())
        } else {
            None
        };
        messages.push(MailMessage {
            uid: fetch.uid.unwrap_or(0),
            from,
            subject,
            date,
            body,
        });
    }
    drop(fetches);

    if mark_seen {
        session
            .uid_store(&uid_set, "+FLAGS (\\Seen)")
            .context("marking messages \\Seen")?;
    }

    Ok(messages)
}

/// Renders one IMAP envelope `Address` as `"name <mailbox@host>"`, or just
/// `"mailbox@host"` when there's no display name.
fn format_address(addr: &imap_proto::types::Address) -> String {
    let mailbox = addr
        .mailbox
        .as_deref()
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .unwrap_or_default();
    let host = addr
        .host
        .as_deref()
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .unwrap_or_default();
    let email = if host.is_empty() {
        mailbox
    } else {
        format!("{mailbox}@{host}")
    };
    match addr.name.as_deref().map(|b| String::from_utf8_lossy(b)) {
        Some(name) if !name.is_empty() => format!("{name} <{email}>"),
        _ => email,
    }
}
