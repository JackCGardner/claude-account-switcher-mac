use std::env;
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::paths::AppPaths;
use crate::state;

const AUTH_ENVIRONMENT: [&str; 3] = [
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN",
];

/// The environment variable that pins a launch to a target (profile or
/// workspace), overriding the directory mapping and the default target. The
/// wrapper re-exports it to the child holding the *resolved* profile name, so
/// nested `claude` invocations inside a session stay on the same login even
/// when a workspace's selection rotates mid-session.
pub const TARGET_ENVIRONMENT_VARIABLE: &str = "CLAUDE_ACCOUNT_PROFILE";

pub fn exec_active_profile(paths: &AppPaths, arguments: &[OsString]) -> Result<()> {
    let state = state::load(paths)?;
    let target = if let Some(value) = env::var_os(TARGET_ENVIRONMENT_VARIABLE) {
        value
            .into_string()
            .ok()
            .with_context(|| format!("{TARGET_ENVIRONMENT_VARIABLE} is not valid UTF-8"))?
    } else if let Some(mapped) = env::current_dir()
        .ok()
        .as_deref()
        .and_then(|cwd| state.mapped_target(cwd))
    {
        mapped.to_owned()
    } else {
        state.active.clone().context(
            "no active profile; run `claude account add NAME` or `claude account use NAME`",
        )?
    };
    exec_target(&state, &target, arguments)
}

pub fn exec_target(state: &state::State, target: &str, arguments: &[OsString]) -> Result<()> {
    let (profile_name, profile) = state.resolve_target(target)?;
    let real_claude = state
        .real_claude
        .as_deref()
        .context("real Claude executable is not configured; run `claude-account install`")?;
    validate_executable(real_claude)?;

    let mut command = managed_command(real_claude, &profile.config_dir);
    command.env(TARGET_ENVIRONMENT_VARIABLE, &profile_name);
    command.args(arguments);
    let error = command.exec();
    Err(error).with_context(|| format!("failed to execute {}", real_claude.display()))
}

pub fn managed_command(real_claude: &Path, config_dir: &Path) -> Command {
    let mut command = Command::new(real_claude);

    if is_default_claude_config_dir(
        config_dir,
        env::var_os("HOME").map(PathBuf::from).as_deref(),
    ) {
        // Claude Code derives its credential-storage key from CLAUDE_CONFIG_DIR
        // when the variable is set, so pointing it at ~/.claude selects a
        // different keychain entry than leaving it unset. Unset it to reach the
        // same credentials as a plain `claude` invocation.
        command.env_remove("CLAUDE_CONFIG_DIR");
    } else {
        command.env("CLAUDE_CONFIG_DIR", config_dir);
    }
    // An inherited storage override would re-key credentials away from the
    // profile directory chosen above.
    command.env_remove("CLAUDE_SECURESTORAGE_CONFIG_DIR");

    if env::var_os("CLAUDE_ACCOUNT_PRESERVE_AUTH_ENV").as_deref() != Some("1".as_ref()) {
        for variable in AUTH_ENVIRONMENT {
            command.env_remove(variable);
        }
    }
    command
}

pub fn is_default_claude_config_dir(config_dir: &Path, home: Option<&Path>) -> bool {
    home.is_some_and(|home| home.join(".claude") == config_dir)
}

#[derive(Debug, Deserialize)]
pub struct AuthStatus {
    #[serde(rename = "loggedIn")]
    pub logged_in: bool,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default, rename = "subscriptionType")]
    pub subscription_type: Option<String>,
}

pub fn auth_status(real_claude: &Path, config_dir: &Path, quiet: bool) -> Result<AuthStatus> {
    auth_status_from(managed_command(real_claude, config_dir), quiet)
}

fn auth_status_from(mut command: Command, quiet: bool) -> Result<AuthStatus> {
    let output = command
        .args(["auth", "status", "--json"])
        .stdout(Stdio::piped())
        .stderr(if quiet {
            Stdio::null()
        } else {
            Stdio::inherit()
        })
        .output()
        .context("failed to run Claude auth status")?;
    // Claude Code exits nonzero when logged out while still printing valid
    // JSON, so the payload decides; the exit code only matters when the
    // output is unusable.
    match serde_json::from_slice(&output.stdout) {
        Ok(status) => Ok(status),
        Err(_) if !output.status.success() => {
            bail!("`claude auth status --json` exited unsuccessfully")
        }
        Err(error) => {
            Err(error).context("Claude returned an invalid response from `auth status --json`")
        }
    }
}

/// Ask whether Claude sees a *stored* login for the given configuration
/// directory. Environment-token auth never influences the answer — even when
/// the user opted into CLAUDE_ACCOUNT_PRESERVE_AUTH_ENV=1 — because the
/// question is always about credential storage. `None` means the probe was
/// inconclusive (for example, a Claude build without `auth status`).
pub fn dir_sees_login(real_claude: &Path, config_dir: &Path) -> Option<bool> {
    let mut command = managed_command(real_claude, config_dir);
    for variable in AUTH_ENVIRONMENT {
        command.env_remove(variable);
    }
    match auth_status_from(command, true) {
        Ok(status) => Some(status.logged_in),
        Err(_) => None,
    }
}

/// Probe whether a brand-new, empty configuration directory already sees an
/// existing login. With per-profile credential storage this is always false;
/// `Some(true)` means this Claude Code build shares credentials across
/// configuration directories, so profiles cannot be isolated.
pub fn fresh_dir_sees_login(real_claude: &Path, paths: &AppPaths) -> Option<bool> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let probe_dir =
        paths
            .profiles_dir
            .join(format!(".isolation-probe.{}.{}", std::process::id(), nonce));
    if state::ensure_private_dir(&probe_dir).is_err() {
        return None;
    }
    let result = dir_sees_login(real_claude, &probe_dir);
    let _ = fs::remove_dir_all(&probe_dir);
    result
}

pub fn resolve_real_claude(
    configured: Option<&Path>,
    current_executable: &Path,
    paths: &AppPaths,
) -> Result<PathBuf> {
    if let Some(explicit) = env::var_os("CLAUDE_ACCOUNT_REAL_CLAUDE") {
        let explicit = PathBuf::from(explicit);
        validate_distinct_executable(&explicit, current_executable)?;
        return Ok(explicit);
    }

    if let Some(configured) = configured {
        if validate_distinct_executable(configured, current_executable).is_ok() {
            return Ok(configured.to_path_buf());
        }
    }

    let path = env::var_os("PATH").context("PATH is not set")?;
    for directory in env::split_paths(&path) {
        let candidate = if directory.as_os_str().is_empty() {
            env::current_dir()?.join("claude")
        } else {
            directory.join("claude")
        };
        if candidate == paths.shim {
            continue;
        }
        if validate_distinct_executable(&candidate, current_executable).is_ok() {
            return Ok(candidate);
        }
    }

    bail!(
        "could not find the real `claude` executable; pass it with \
         `claude-account install --real /path/to/claude`"
    )
}

pub fn validate_executable(path: &Path) -> Result<()> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("Claude executable does not exist: {}", path.display()))?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        bail!("path is not executable: {}", path.display());
    }
    Ok(())
}

fn validate_distinct_executable(candidate: &Path, current_executable: &Path) -> Result<()> {
    validate_executable(candidate)?;
    let candidate_canonical = fs::canonicalize(candidate)
        .with_context(|| format!("failed to resolve {}", candidate.display()))?;
    let current_canonical = fs::canonicalize(current_executable)
        .with_context(|| format!("failed to resolve {}", current_executable.display()))?;
    if candidate_canonical == current_canonical {
        bail!("candidate points back to the claude-account wrapper");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_dir_detection_ignores_trailing_slashes() {
        let home = Path::new("/home/user");
        assert!(is_default_claude_config_dir(
            Path::new("/home/user/.claude"),
            Some(home)
        ));
        assert!(is_default_claude_config_dir(
            Path::new("/home/user/.claude/"),
            Some(home)
        ));
        assert!(!is_default_claude_config_dir(
            Path::new("/home/user/.claude-work"),
            Some(home)
        ));
        assert!(!is_default_claude_config_dir(
            Path::new("/home/user/.claude"),
            None
        ));
    }
}
