use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path};

pub const REQUIRED_LABELS: [&str; 2] = ["ai-ready", "ai-low-risk"];
pub const FORBIDDEN_LABELS: [&str; 5] = [
    "security",
    "database",
    "migration",
    "production",
    "high-risk",
];
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Issue {
    pub number: u64,
    pub title: String,
    pub body: String,
    pub url: String,
    pub labels: Vec<String>,
}

impl Issue {
    pub fn validate_for_automation(&self) -> Result<()> {
        for required in REQUIRED_LABELS {
            if !self
                .labels
                .iter()
                .any(|label| label.eq_ignore_ascii_case(required))
            {
                bail!("issue must have both labels: ai-ready and ai-low-risk");
            }
        }
        for forbidden in FORBIDDEN_LABELS {
            if self
                .labels
                .iter()
                .any(|label| label.eq_ignore_ascii_case(forbidden))
            {
                bail!("issue has excluded high-risk label '{forbidden}'");
            }
        }
        Ok(())
    }
}

pub fn validate_changed_path(path: &Path) -> Result<()> {
    if path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        bail!("unsafe changed path: {}", path.display());
    }
    let value = path.to_string_lossy().replace('\\', "/");
    let lower = value.to_ascii_lowercase();
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let protected = matches!(value.as_str(), "Cargo.toml" | "Cargo.lock" | ".gitmodules")
        || lower.starts_with(".github/workflows/")
        || ["migrations/", "schema/", "credentials/", "secrets/"]
            .iter()
            .any(|prefix| lower.starts_with(prefix) || lower.contains(&format!("/{prefix}")))
        || (file_name.starts_with(".env") && file_name != ".env.example")
        || [".pem", ".p12", ".pfx", ".tfstate"]
            .iter()
            .any(|extension| lower.ends_with(extension))
        || lower.contains("private_key")
        || path.file_name().and_then(|name| name.to_str()) == Some("secrets.rs");
    if protected {
        bail!("changed path requires manual handling: {}", path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eligible_issue(labels: &[&str]) -> Issue {
        Issue {
            number: 1,
            title: "test".into(),
            body: "".into(),
            url: "https://example.test/1".into(),
            labels: labels.iter().map(|label| (*label).into()).collect(),
        }
    }

    #[test]
    fn issue_policy_requires_opt_in_and_rejects_risk_labels() {
        assert!(
            eligible_issue(&["ai-ready", "ai-low-risk"])
                .validate_for_automation()
                .is_ok()
        );
        assert!(
            eligible_issue(&["ai-ready"])
                .validate_for_automation()
                .is_err()
        );
        assert!(
            eligible_issue(&["ai-ready", "ai-low-risk", "security"])
                .validate_for_automation()
                .is_err()
        );
    }

    #[test]
    fn changed_path_policy_rejects_sensitive_and_escape_paths() {
        for path in [
            "../outside",
            ".env",
            "Cargo.toml",
            ".github/workflows/ci.yml",
            "src/secrets.rs",
        ] {
            assert!(validate_changed_path(Path::new(path)).is_err(), "{path}");
        }
        assert!(validate_changed_path(Path::new("README.md")).is_ok());
        assert!(validate_changed_path(Path::new(".env.example")).is_ok());
    }
}
