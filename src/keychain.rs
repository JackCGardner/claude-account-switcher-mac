//! Read-only access to the login-keychain items Claude Code stores its OAuth
//! credentials in. Used only by the opt-in live usage path; nothing here ever
//! writes, deletes, or copies credentials anywhere.

use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

use crate::process;

/// Absolute path so a malicious `security` earlier on PATH cannot intercept
/// the read.
const SECURITY_BINARY: &str = "/usr/bin/security";

/// The keychain service name Claude Code derives for a CLAUDE_CONFIG_DIR
/// value: the raw environment string, NFC-normalized and *not* path-resolved,
/// SHA-256 hashed and truncated to 8 hex characters. `None` (the variable
/// unset — Claude's default ~/.claude directory) uses the unsuffixed name.
pub fn service_name(config_dir_value: Option<&str>) -> String {
    match config_dir_value {
        None => "Claude Code-credentials".to_owned(),
        Some(value) => {
            let normalized: String = value.nfc().collect();
            let digest = Sha256::digest(normalized.as_bytes());
            let suffix: String = digest
                .iter()
                .take(4)
                .map(|byte| format!("{byte:02x}"))
                .collect();
            format!("Claude Code-credentials-{suffix}")
        }
    }
}

/// The CLAUDE_CONFIG_DIR value a profile's launches use: `None` for the
/// default ~/.claude directory (the wrapper unsets the variable there).
pub fn config_dir_value(config_dir: &Path, home: Option<&Path>) -> Option<String> {
    if process::is_default_claude_config_dir(config_dir, home) {
        None
    } else {
        Some(config_dir.to_string_lossy().into_owned())
    }
}

/// Read a profile's OAuth access token from the login keychain (read-only).
/// Claude Code creates its keychain items through this same
/// `/usr/bin/security` binary, so the read succeeds without a user-facing
/// keychain prompt. macOS only.
pub fn read_access_token(config_dir: &Path, home: Option<&Path>) -> Result<String> {
    if !cfg!(target_os = "macos") {
        bail!("live usage reads require macOS keychain credential storage");
    }
    let service = service_name(config_dir_value(config_dir, home).as_deref());
    let output = Command::new(SECURITY_BINARY)
        .args(["find-generic-password", "-s", &service, "-w"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .context("failed to run /usr/bin/security")?;
    if !output.status.success() {
        bail!("no keychain login found under service `{service}`");
    }
    let raw = String::from_utf8(output.stdout).context("keychain item is not valid UTF-8")?;
    extract_access_token(&raw)
}

/// Pull the access token out of Claude's credential JSON and refuse tokens
/// containing characters that could escape the quoted curl-config value they
/// are later embedded in (defense in depth against a tampered keychain item).
fn extract_access_token(raw: &str) -> Result<String> {
    let value: serde_json::Value = serde_json::from_str(raw.trim())
        .context("keychain item does not contain Claude's credential JSON")?;
    let token = value
        .get("claudeAiOauth")
        .and_then(|oauth| oauth.get("accessToken"))
        .and_then(|token| token.as_str())
        .context("keychain item has no OAuth access token")?;
    if token.is_empty()
        || token
            .chars()
            .any(|character| character == '"' || character == '\\' || character.is_control())
    {
        bail!("keychain access token has an unexpected format");
    }
    Ok(token.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_names_match_claude_codes_derivation() {
        assert_eq!(service_name(None), "Claude Code-credentials");
        // printf %s "/Users/jackgardner/.claude-work-3" | shasum -a 256
        assert_eq!(
            service_name(Some("/Users/jackgardner/.claude-work-3")),
            "Claude Code-credentials-7661794d"
        );
    }

    #[test]
    fn service_names_are_nfc_normalized() {
        let nfd = "/tmp/cafe\u{0301}"; // "café" with a combining accent
        let nfc = "/tmp/caf\u{00e9}";
        assert_eq!(service_name(Some(nfd)), service_name(Some(nfc)));
        assert_eq!(service_name(Some(nfd)), "Claude Code-credentials-0873cca0");
    }

    #[test]
    fn tokens_with_config_breaking_characters_are_refused() {
        let good = r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-abc_DEF.123~xyz"}}"#;
        assert_eq!(
            extract_access_token(good).unwrap(),
            "sk-ant-oat01-abc_DEF.123~xyz"
        );
        for bad in [
            r#"{"claudeAiOauth":{"accessToken":"a\"b"}}"#,
            r#"{"claudeAiOauth":{"accessToken":"a\\b"}}"#,
            r#"{"claudeAiOauth":{"accessToken":"a\nproxy = \"http://evil/\""}}"#,
            r#"{"claudeAiOauth":{"accessToken":""}}"#,
        ] {
            let error = extract_access_token(bad).unwrap_err();
            assert!(
                error.to_string().contains("unexpected format"),
                "{bad}: {error:#}"
            );
        }
    }

    #[test]
    fn default_directory_uses_the_unsuffixed_item() {
        let home = Path::new("/home/user");
        assert_eq!(
            config_dir_value(Path::new("/home/user/.claude"), Some(home)),
            None
        );
        assert_eq!(
            config_dir_value(Path::new("/home/user/.claude-work"), Some(home)),
            Some("/home/user/.claude-work".to_owned())
        );
    }
}
