use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde_json::{Map, Value};

/// Apply `mutate` to `<profile_dir>/.claude.json`, creating the file when it
/// does not exist. Claude Code keeps its top-level state in this file, so all
/// unrelated fields are preserved and the write is atomic and private (0600).
pub fn update(profile_dir: &Path, mutate: impl FnOnce(&mut Map<String, Value>)) -> Result<()> {
    let config_path = profile_dir.join(".claude.json");
    let mut config = match fs::read(&config_path) {
        Ok(contents) => serde_json::from_slice::<Value>(&contents)
            .with_context(|| format!("failed to parse {}", config_path.display()))?,
        Err(error) if error.kind() == ErrorKind::NotFound => Value::Object(Map::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", config_path.display()));
        }
    };
    let object = config
        .as_object_mut()
        .with_context(|| format!("{} must contain a JSON object", config_path.display()))?;
    mutate(object);

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = profile_dir.join(format!(".claude.json.tmp.{}.{}", std::process::id(), nonce));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)
            .with_context(|| format!("failed to create {}", temporary.display()))?;
        serde_json::to_writer_pretty(&mut file, &config)
            .context("failed to serialize Claude state")?;
        file.write_all(b"\n")
            .context("failed to finish Claude state")?;
        file.sync_all().context("failed to sync Claude state")?;
        fs::rename(&temporary, &config_path)
            .with_context(|| format!("failed to update {}", config_path.display()))?;
        fs::set_permissions(&config_path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to protect {}", config_path.display()))?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// Mark Claude Code's local onboarding as completed so the first launch in the
/// profile uses the saved login without asking to authenticate again.
pub fn complete_onboarding(profile_dir: &Path) -> Result<()> {
    update(profile_dir, |config| {
        config.insert("hasCompletedOnboarding".to_owned(), Value::Bool(true));
    })
}

/// Read the account-identity metadata Claude Code keeps in `.claude.json`:
/// the `oauthAccount` object (email, account ids) and the `userID` string.
/// These describe which account is logged in; they are not credentials.
pub fn read_identity(profile_dir: &Path) -> Result<Option<Value>> {
    let config_path = profile_dir.join(".claude.json");
    let contents = match fs::read(&config_path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", config_path.display()));
        }
    };
    let config: Value = serde_json::from_slice(&contents)
        .with_context(|| format!("failed to parse {}", config_path.display()))?;
    let Some(object) = config.as_object() else {
        return Ok(None);
    };
    let Some(oauth_account) = object.get("oauthAccount").filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let mut identity = Map::new();
    identity.insert("oauthAccount".to_owned(), oauth_account.clone());
    if let Some(user_id) = object.get("userID").filter(|value| !value.is_null()) {
        identity.insert("userID".to_owned(), user_id.clone());
    }
    Ok(Some(Value::Object(identity)))
}

/// Write a previously captured identity back into `.claude.json`, replacing
/// whichever member's identity is currently there.
pub fn write_identity(profile_dir: &Path, identity: &Value) -> Result<()> {
    let oauth_account = identity.get("oauthAccount").cloned();
    let user_id = identity.get("userID").cloned();
    update(profile_dir, move |config| {
        match oauth_account {
            Some(value) => {
                config.insert("oauthAccount".to_owned(), value);
            }
            None => {
                config.remove("oauthAccount");
            }
        }
        match user_id {
            Some(value) => {
                config.insert("userID".to_owned(), value);
            }
            None => {
                config.remove("userID");
            }
        }
    })
}

/// The email address inside a captured identity, for display purposes.
pub fn identity_email(identity: &Value) -> Option<&str> {
    let account = identity.get("oauthAccount")?;
    account
        .get("emailAddress")
        .or_else(|| account.get("email"))?
        .as_str()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state;

    #[test]
    fn onboarding_update_preserves_existing_claude_state() {
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join("profile");
        state::ensure_private_dir(&profile).unwrap();
        let config_path = profile.join(".claude.json");
        fs::write(
            &config_path,
            r#"{"existing":{"setting":"preserved"},"hasCompletedOnboarding":false}"#,
        )
        .unwrap();

        complete_onboarding(&profile).unwrap();

        let updated: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
        assert_eq!(updated["existing"]["setting"], "preserved");
        assert_eq!(updated["hasCompletedOnboarding"], true);
        assert_eq!(
            fs::metadata(config_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn identity_round_trip_swaps_only_identity_fields() {
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join("profile");
        state::ensure_private_dir(&profile).unwrap();
        fs::write(
            profile.join(".claude.json"),
            r#"{"oauthAccount":{"emailAddress":"a@example.com"},"userID":"uid-a","numStartups":7}"#,
        )
        .unwrap();

        let identity_a = read_identity(&profile).unwrap().unwrap();
        assert_eq!(identity_email(&identity_a), Some("a@example.com"));

        let identity_b: Value = serde_json::from_str(
            r#"{"oauthAccount":{"emailAddress":"b@example.com"},"userID":"uid-b"}"#,
        )
        .unwrap();
        write_identity(&profile, &identity_b).unwrap();

        let updated: Value =
            serde_json::from_slice(&fs::read(profile.join(".claude.json")).unwrap()).unwrap();
        assert_eq!(updated["oauthAccount"]["emailAddress"], "b@example.com");
        assert_eq!(updated["userID"], "uid-b");
        assert_eq!(updated["numStartups"], 7);

        write_identity(&profile, &identity_a).unwrap();
        let restored = read_identity(&profile).unwrap().unwrap();
        assert_eq!(identity_email(&restored), Some("a@example.com"));
    }

    #[test]
    fn missing_or_logged_out_files_have_no_identity() {
        let temp = tempfile::tempdir().unwrap();
        assert!(read_identity(temp.path()).unwrap().is_none());
        fs::write(temp.path().join(".claude.json"), r#"{"numStartups":1}"#).unwrap();
        assert!(read_identity(temp.path()).unwrap().is_none());
    }
}
