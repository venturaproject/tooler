use crate::{config::Profile, secrets};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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

    let response = client
        .post(token_url)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token.as_str()),
            ("client_id", client_id.as_str()),
            ("client_secret", client_secret.as_str()),
        ])
        .send()
        .with_context(|| {
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

    let expiry = compute_expiry(now_secs()?, body.expires_in);
    secrets::set_secret(profile_name, "access_token", &body.access_token)?;
    secrets::set_secret(profile_name, "access_token_expiry", &expiry.to_string())?;
    if let Some(new_refresh) = &body.refresh_token {
        secrets::set_secret(profile_name, "refresh_token", new_refresh)?;
    }

    Ok(body.access_token)
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
}
