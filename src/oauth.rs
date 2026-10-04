use crate::{config::Profile, secrets};
use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    net::TcpListener,
    process::Command,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use url::Url;

const EXPIRY_SAFETY_MARGIN_SECS: u64 = 60;
const DEFAULT_EXPIRES_IN_SECS: u64 = 3600;

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: Option<u64>,
    /// Present when the provider rotates refresh tokens on every use (e.g. Exact
    /// Online). When present, it replaces the stored refresh token.
    refresh_token: Option<String>,
}

fn now_secs() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

fn is_expired(expiry_secs: u64, now_secs: u64) -> bool {
    now_secs >= expiry_secs
}

fn compute_expiry(now_secs: u64, expires_in: Option<u64>) -> u64 {
    now_secs
        + expires_in
            .unwrap_or(DEFAULT_EXPIRES_IN_SECS)
            .saturating_sub(EXPIRY_SAFETY_MARGIN_SECS)
}

fn store_token_response(profile_name: &str, body: TokenResponse) -> Result<()> {
    let expiry = compute_expiry(now_secs()?, body.expires_in);
    secrets::set_secret(profile_name, "access_token", &body.access_token)?;
    secrets::set_secret(profile_name, "access_token_expiry", &expiry.to_string())?;
    if let Some(new_refresh) = &body.refresh_token {
        secrets::set_secret(profile_name, "refresh_token", new_refresh)?;
    }
    Ok(())
}

fn cached_access_token(profile_name: &str) -> Result<Option<String>> {
    let Some(expiry) = secrets::get_secret(profile_name, "access_token_expiry")? else {
        return Ok(None);
    };
    let Ok(expiry) = expiry.parse::<u64>() else {
        return Ok(None);
    };
    if is_expired(expiry, now_secs()?) {
        return Ok(None);
    }
    secrets::get_secret(profile_name, "access_token")
}

fn refresh_access_token(profile_name: &str, profile: &Profile, token_url: &str) -> Result<String> {
    let Some(refresh_token) = secrets::get_secret(profile_name, "refresh_token")? else {
        bail!(
            "Profile '{profile_name}' has a token_url but no refresh_token configured.\n  \
             Set one with: tooler config set profile.{profile_name}.refresh_token <token>"
        );
    };
    let client_id = profile.client_id.clone().unwrap_or_default();
    let client_secret = secrets::get_secret(profile_name, "client_secret")?.unwrap_or_default();

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;

    let mut form = vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token.as_str()),
        ("client_id", client_id.as_str()),
    ];
    if !client_secret.is_empty() {
        form.push(("client_secret", client_secret.as_str()));
    }
    let response = client.post(token_url).form(&form).send().with_context(|| {
        format!("OAuth2 token refresh request failed for profile '{profile_name}'")
    })?;

    let status = response.status();
    if !status.is_success() {
        bail!(
            "OAuth2 token refresh failed for profile '{profile_name}': HTTP {status}\n  \
             The refresh_token may be expired or revoked; reconfigure it with:\n  \
             tooler config set profile.{profile_name}.refresh_token <token>"
        );
    }

    let body: TokenResponse = response.json().with_context(|| {
        format!("Unexpected OAuth2 token response shape for profile '{profile_name}'")
    })?;

    let access_token = body.access_token.clone();
    store_token_response(profile_name, body)?;
    Ok(access_token)
}

fn random_urlsafe(bytes: usize) -> Result<String> {
    let mut random = vec![0_u8; bytes];
    getrandom::fill(&mut random)
        .map_err(|error| anyhow::anyhow!("generating OAuth2 PKCE randomness: {error}"))?;
    Ok(URL_SAFE_NO_PAD.encode(random))
}

fn code_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn loopback_listener(redirect_uri: Option<&str>) -> Result<(TcpListener, String)> {
    let address = match redirect_uri {
        Some(uri) => {
            let parsed = Url::parse(uri).context("invalid OAuth2 redirect_uri")?;
            if parsed.scheme() != "http"
                || parsed.host_str() != Some("127.0.0.1")
                || parsed.port().is_none()
            {
                bail!("OAuth2 redirect_uri must be an http://127.0.0.1:<port>/ callback");
            }
            format!("127.0.0.1:{}", parsed.port().expect("validated port"))
        }
        None => "127.0.0.1:0".to_string(),
    };
    let listener = TcpListener::bind(&address)
        .with_context(|| format!("binding OAuth2 loopback callback on {address}"))?;
    let callback = redirect_uri.map(str::to_string).unwrap_or_else(|| {
        format!(
            "http://127.0.0.1:{}/",
            listener.local_addr().expect("listener address").port()
        )
    });
    listener.set_nonblocking(true)?;
    Ok((listener, callback))
}

fn open_browser(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    let mut command = Command::new("open");
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut c = Command::new("cmd");
        c.args(["/C", "start", "", url]);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = Command::new("xdg-open");
    #[cfg(not(target_os = "windows"))]
    command.arg(url);
    command
        .spawn()
        .context("opening OAuth2 authorization URL")?;
    Ok(())
}

fn wait_for_callback(
    listener: TcpListener,
    expected_state: &str,
    timeout: Duration,
) -> Result<String> {
    let deadline = Instant::now() + timeout;
    loop {
        if Instant::now() >= deadline {
            bail!("OAuth2 authorization timed out waiting for the loopback callback");
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                let mut request = [0_u8; 8192];
                let size = stream.read(&mut request)?;
                let target = std::str::from_utf8(&request[..size])
                    .context("invalid OAuth2 callback request")?
                    .split_whitespace()
                    .nth(1)
                    .context("invalid OAuth2 callback request line")?;
                let parsed = Url::parse(&format!("http://127.0.0.1{target}"))?;
                let params: std::collections::HashMap<_, _> =
                    parsed.query_pairs().into_owned().collect();
                let code = params.get("code").cloned();
                let state = params.get("state").map(String::as_str);
                let error = params.get("error").cloned();
                let body = if code.is_some() && state == Some(expected_state) {
                    "Authorization received. You can return to tooler."
                } else {
                    "Authorization could not be verified. You can close this page."
                };
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{body}", body.len()).as_bytes())?;
                if state != Some(expected_state) {
                    bail!("OAuth2 callback state did not match");
                }
                if let Some(error) = error {
                    bail!("OAuth2 authorization failed: {error}");
                }
                return code.context("OAuth2 callback did not include an authorization code");
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(100))
            }
            Err(error) => return Err(error.into()),
        }
    }
}

/// Performs an OAuth2 authorization-code login using a local loopback callback and S256 PKCE.
/// Token values, the verifier, and state never reach CLI output or audit records.
pub fn login(
    profile_name: &str,
    profile: &Profile,
    scopes: &[String],
    no_open: bool,
    timeout: Duration,
) -> Result<()> {
    let authorization_url = profile
        .authorization_url
        .as_deref()
        .context("profile has no authorization_url; set profile.<name>.authorization_url first")?;
    let token_url = profile
        .token_url
        .as_deref()
        .context("profile has no token_url; set profile.<name>.token_url first")?;
    let client_id = profile
        .client_id
        .as_deref()
        .context("profile has no client_id; set profile.<name>.client_id first")?;
    let verifier = random_urlsafe(48)?;
    let state = random_urlsafe(32)?;
    let (listener, redirect_uri) = loopback_listener(profile.redirect_uri.as_deref())?;
    let mut url = Url::parse(authorization_url).context("invalid OAuth2 authorization_url")?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("code_challenge", &code_challenge(&verifier))
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state);
    if !scopes.is_empty() {
        url.query_pairs_mut()
            .append_pair("scope", &scopes.join(" "));
    }
    eprintln!("Open this OAuth2 authorization URL in a browser:\n{url}");
    if !no_open {
        open_browser(url.as_str())?;
    }
    let code = wait_for_callback(listener, &state, timeout)?;
    let client_secret = secrets::get_secret(profile_name, "client_secret")?;
    let mut form = vec![
        ("grant_type", "authorization_code"),
        ("code", code.as_str()),
        ("redirect_uri", redirect_uri.as_str()),
        ("client_id", client_id),
        ("code_verifier", verifier.as_str()),
    ];
    if let Some(client_secret) = client_secret.as_deref().filter(|value| !value.is_empty()) {
        form.push(("client_secret", client_secret));
    }
    let response = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()?
        .post(token_url)
        .form(&form)
        .send()
        .context("OAuth2 token exchange request failed")?;
    if !response.status().is_success() {
        bail!("OAuth2 token exchange failed: HTTP {}", response.status());
    }
    let body: TokenResponse = response
        .json()
        .context("unexpected OAuth2 token response shape")?;
    store_token_response(profile_name, body)
}

/// Returns a valid access token for an OAuth2-managed profile (one with `token_url`
/// set), refreshing and caching it as needed. Returns `Ok(None)` if the profile isn't
/// OAuth2-managed at all, so callers can fall back to a static bearer token.
pub fn get_valid_access_token(profile_name: &str, profile: &Profile) -> Result<Option<String>> {
    let Some(token_url) = &profile.token_url else {
        return Ok(None);
    };
    if let Some(cached) = cached_access_token(profile_name)? {
        return Ok(Some(cached));
    }
    refresh_access_token(profile_name, profile, token_url).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_expired_true_when_past() {
        assert!(is_expired(100, 200));
        assert!(is_expired(100, 100));
    }

    #[test]
    fn is_expired_false_when_future() {
        assert!(!is_expired(200, 100));
    }

    #[test]
    fn compute_expiry_applies_safety_margin() {
        assert_eq!(compute_expiry(1000, Some(3600)), 1000 + 3600 - 60);
    }

    #[test]
    fn compute_expiry_falls_back_to_default_when_missing() {
        assert_eq!(compute_expiry(1000, None), 1000 + 3600 - 60);
    }

    #[test]
    fn token_response_deserializes_without_rotated_refresh_token() {
        let raw = r#"{"access_token":"abc123","expires_in":3600}"#;
        let parsed: TokenResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.access_token, "abc123");
        assert_eq!(parsed.expires_in, Some(3600));
        assert_eq!(parsed.refresh_token, None);
    }

    #[test]
    fn token_response_deserializes_with_rotated_refresh_token() {
        let raw = r#"{"access_token":"abc123","expires_in":600,"refresh_token":"new-refresh"}"#;
        let parsed: TokenResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.access_token, "abc123");
        assert_eq!(parsed.expires_in, Some(600));
        assert_eq!(parsed.refresh_token.as_deref(), Some("new-refresh"));
    }

    #[test]
    fn pkce_challenge_uses_s256_urlsafe_encoding() {
        assert_eq!(
            code_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }
}
