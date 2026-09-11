//! Mail credential resolution (`mail:`/`mail_check:` and `tooler mail send`) and the
//! `lettre`-based `send_mail` — the one place that crate is touched.
use super::*;
use anyhow::{Result, bail};
use std::path::PathBuf;

/// Resolved SMTP connection details for a `mail:` task or `tooler mail send` — always the
/// output of `resolve_mail_creds`, never built directly.
#[derive(Debug)]
pub(crate) struct MailCreds {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) user: String,
    pub(crate) password: String,
    pub(crate) from: String,
    pub(crate) tls: String,
}

/// Resolves SMTP connection details for `mail:`/`tooler mail send`: explicit fields win,
/// falling back to the named `server:` profile's config fields (`config.mail.<name>`),
/// falling back to `TOOLER_MAIL_PASSWORD` for the password specifically — mirrors
/// `db_query:`'s `TOOLER_DB_PASSWORD` pattern in `commands::db`. TLS mode: explicit `tls`
/// wins, else the profile's `tls`, else inferred from `port` (465 -> "tls", else
/// "starttls").
#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve_mail_creds(
    ctx: &Context,
    server: Option<&str>,
    host: Option<&str>,
    port: Option<u16>,
    user: Option<&str>,
    password: Option<&str>,
    from: Option<&str>,
    tls: Option<&str>,
) -> Result<MailCreds> {
    let profile = server.and_then(|name| ctx.config.mail.get(name));
    if let Some(name) = server
        && profile.is_none()
        && host.is_none()
    {
        bail!(
            "No mail profile '{name}' configured. Set it with: tooler config set mail.{name}.host <host>"
        );
    }

    let host = host
        .map(String::from)
        .or_else(|| profile.map(|p| p.host.clone()))
        .filter(|h| !h.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("mail needs 'host' or a 'server:' mail profile with a host set")
        })?;
    let user = user
        .map(String::from)
        .or_else(|| profile.map(|p| p.user.clone()))
        .filter(|u| !u.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("mail needs 'user' or a 'server:' mail profile with a user set")
        })?;
    let port = port
        .or_else(|| profile.map(|p| p.port))
        .filter(|&p| p != 0)
        .unwrap_or(587);
    let from = from
        .map(String::from)
        .or_else(|| profile.and_then(|p| p.from.clone()))
        .unwrap_or_else(|| user.clone());
    let tls = tls
        .map(String::from)
        .or_else(|| profile.and_then(|p| p.tls.clone()))
        .unwrap_or_else(|| {
            if port == 465 {
                "tls".to_string()
            } else {
                "starttls".to_string()
            }
        });

    let password = if let Some(p) = password {
        p.to_string()
    } else if let Some(name) = server {
        match crate::secrets::get_secret(&format!("mail:{name}"), "password")? {
            Some(p) => p,
            None => std::env::var("TOOLER_MAIL_PASSWORD").map_err(|_| {
                anyhow::anyhow!(
                    "No password for mail profile '{name}' (or the OS keychain is locked) — \
                     set it with: tooler config set mail.{name}.password <value>, or set \
                     TOOLER_MAIL_PASSWORD"
                )
            })?,
        }
    } else {
        std::env::var("TOOLER_MAIL_PASSWORD").map_err(|_| {
            anyhow::anyhow!(
                "mail needs 'password', a 'server:' profile's stored password, or \
                 TOOLER_MAIL_PASSWORD"
            )
        })?
    };

    Ok(MailCreds {
        host,
        port,
        user,
        password,
        from,
        tls,
    })
}

/// Resolved IMAP connection details for `mail_check:`/`tooler mail check` — always the
/// output of `resolve_imap_creds`, never built directly.
#[derive(Debug)]
pub(crate) struct ImapCreds {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) user: String,
    pub(crate) password: String,
}

/// Resolves IMAP connection details for a mail profile — profile-only (no inline
/// host/user/password override the way `resolve_mail_creds` allows for SMTP, since
/// `mail_check:`/`tooler mail check` are narrower/newer and a profile is the only
/// supported path in v1). `imap_host` defaults to the profile's SMTP `host` (the common
/// case: one mailbox, two protocols, same server); `imap_port` defaults to `993`. The
/// password is the exact same keychain entry `resolve_mail_creds` reads (`mail:<name>`) —
/// one login shared by both protocols.
/// The pure, keychain-free half of `resolve_imap_creds`'s resolution: `imap_host`
/// defaults to the profile's SMTP `host` (one mailbox, two protocols, same server —
/// exactly the case with every mail profile set up so far), `imap_port` defaults to
/// `993`. Split out so this defaulting logic is unit-testable without a real OS
/// credential store in the loop.
pub(crate) fn resolve_imap_host_port(profile: &crate::config::MailServer) -> (Option<String>, u16) {
    let host = profile
        .imap_host
        .clone()
        .filter(|h| !h.is_empty())
        .or_else(|| Some(profile.host.clone()).filter(|h| !h.is_empty()));
    let port = profile.imap_port.filter(|&p| p != 0).unwrap_or(993);
    (host, port)
}

pub(crate) fn resolve_imap_creds(ctx: &Context, server: &str) -> Result<ImapCreds> {
    let profile = ctx.config.mail.get(server).ok_or_else(|| {
        anyhow::anyhow!(
            "No mail profile '{server}' configured. Set it with: tooler config set mail.{server}.host <host>"
        )
    })?;
    let (host, port) = resolve_imap_host_port(profile);
    let host = host.ok_or_else(|| {
        anyhow::anyhow!(
            "mail profile '{server}' has no host set (imap_host or host) -- set it with: \
             tooler config set mail.{server}.host <host>"
        )
    })?;
    let user = if profile.user.is_empty() {
        bail!(
            "mail profile '{server}' has no user set -- set it with: tooler config set \
             mail.{server}.user <user>"
        );
    } else {
        profile.user.clone()
    };
    let password = crate::secrets::get_secret(&format!("mail:{server}"), "password")?
        .or_else(|| std::env::var("TOOLER_MAIL_PASSWORD").ok())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "No password for mail profile '{server}' (or the OS keychain is locked) — \
                 set it with: tooler config set mail.{server}.password <value>, or set \
                 TOOLER_MAIL_PASSWORD"
            )
        })?;

    Ok(ImapCreds {
        host,
        port,
        user,
        password,
    })
}

/// The one place `lettre` is touched. Builds a `Message` from `,`-separated to/cc/bcc
/// lists and sends it over SMTP per `creds.tls` — "starttls" -> `starttls_relay` (upgrade
/// an unencrypted connection, port 587 territory), "tls" -> `relay` (implicit/wrapper TLS,
/// port 465 territory), "none" -> `builder_dangerous` (no TLS at all — an escape hatch for
/// a local, unauthenticated relay only). Returns the number of recipients (to+cc+bcc).
// One parameter per distinct email field (to/cc/bcc/subject/body/html/attachments) plus
// creds -- splitting this into a builder/options struct wouldn't reduce real complexity,
// just move the same 8 fields into a different shape.
#[allow(clippy::too_many_arguments)]
pub(crate) fn send_mail(
    creds: &MailCreds,
    to: &str,
    cc: Option<&str>,
    bcc: Option<&str>,
    subject: &str,
    body: &str,
    html: bool,
    attachments: &[PathBuf],
) -> Result<usize> {
    use lettre::message::{Attachment, Mailbox, MultiPart, SinglePart};
    use lettre::transport::smtp::authentication::Credentials;
    use lettre::{Message, SmtpTransport, Transport};

    // Read every attachment up front, before opening any network connection, so a typo'd
    // path fails fast and clearly instead of after a (possibly slow) SMTP handshake.
    let attachment_bytes: Vec<(String, Vec<u8>, &'static str)> = attachments
        .iter()
        .map(|path| {
            let bytes = std::fs::read(path)
                .with_context(|| format!("reading attachment '{}'", path.display()))?;
            let filename = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "attachment".to_string());
            Ok((filename, bytes, guess_mime(path)))
        })
        .collect::<Result<Vec<_>>>()?;

    let mut builder = Message::builder()
        .from(
            creds
                .from
                .parse::<Mailbox>()
                .with_context(|| format!("invalid from address '{}'", creds.from))?,
        )
        .subject(subject);

    let mut recipient_count = 0usize;
    for addr in to.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        builder = builder.to(addr
            .parse::<Mailbox>()
            .with_context(|| format!("invalid to address '{addr}'"))?);
        recipient_count += 1;
    }
    if recipient_count == 0 {
        bail!("mail: 'to' has no addresses");
    }
    if let Some(cc) = cc {
        for addr in cc.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            builder = builder.cc(addr
                .parse::<Mailbox>()
                .with_context(|| format!("invalid cc address '{addr}'"))?);
            recipient_count += 1;
        }
    }
    if let Some(bcc) = bcc {
        for addr in bcc.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            builder = builder.bcc(
                addr.parse::<Mailbox>()
                    .with_context(|| format!("invalid bcc address '{addr}'"))?,
            );
            recipient_count += 1;
        }
    }

    let part = if html {
        SinglePart::html(body.to_string())
    } else {
        SinglePart::plain(body.to_string())
    };
    let message = if attachment_bytes.is_empty() {
        builder.singlepart(part).context("building email message")?
    } else {
        let mut multipart = MultiPart::mixed().singlepart(part);
        for (filename, bytes, mime) in attachment_bytes {
            let content_type = lettre::message::header::ContentType::parse(mime)
                .with_context(|| format!("invalid content type '{mime}' for '{filename}'"))?;
            multipart = multipart.singlepart(Attachment::new(filename).body(bytes, content_type));
        }
        builder
            .multipart(multipart)
            .context("building email message")?
    };

    let transport = match creds.tls.as_str() {
        "starttls" => SmtpTransport::starttls_relay(&creds.host)?.port(creds.port),
        "tls" => SmtpTransport::relay(&creds.host)?.port(creds.port),
        "none" => SmtpTransport::builder_dangerous(&creds.host).port(creds.port),
        other => bail!("mail: unknown tls mode '{other}' — use starttls, tls, or none"),
    }
    .credentials(Credentials::new(creds.user.clone(), creds.password.clone()))
    .build();

    transport.send(&message).context("sending email")?;
    Ok(recipient_count)
}

/// Best-effort content type from a file extension, for `mail:` attachments. Unknown or
/// missing extensions fall back to a generic binary type — attachments still work, mail
/// clients just won't show a specific icon/preview for them.
pub(crate) fn guess_mime(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("pdf") => "application/pdf",
        Some("csv") => "text/csv",
        Some("txt") => "text/plain",
        Some("json") => "application/json",
        Some("html") | Some("htm") => "text/html",
        Some("xlsx") => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        Some("xls") => "application/vnd.ms-excel",
        Some("docx") => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        Some("zip") => "application/zip",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        _ => "application/octet-stream",
    }
}
