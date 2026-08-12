use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::paths::AppPaths;

const LOCK_EX: i32 = 2;

unsafe extern "C" {
    fn flock(fd: i32, operation: i32) -> i32;
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default = "state_version")]
    pub version: u32,
    #[serde(default)]
    pub active: Option<String>,
    #[serde(default)]
    pub real_claude: Option<PathBuf>,
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
    /// Shared workspaces: one directory used by several member profiles, each
    /// with its own login. Absent entirely in version-1 state files.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub workspaces: BTreeMap<String, Workspace>,
}

impl State {
    /// Workspaces are the only feature an older binary would silently drop on
    /// save, so their presence bumps the persisted version.
    fn uses_workspaces(&self) -> bool {
        !self.workspaces.is_empty()
            || self
                .profiles
                .values()
                .any(|profile| profile.workspace.is_some() || profile.identity.is_some())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub dir: PathBuf,
    pub created_at: u64,
    /// The directory pre-existed as a profile's own directory (created with
    /// `--from-profile`); it is never deleted by workspace removal.
    #[serde(default)]
    pub external: bool,
}

impl Workspace {
    pub fn new(dir: PathBuf, external: bool) -> Self {
        Self {
            dir,
            created_at: unix_now(),
            external,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub config_dir: PathBuf,
    pub created_at: u64,
    /// The directory pre-existed claude-account and is only registered, not
    /// managed; 0.1.1 state files deserialize as managed.
    #[serde(default)]
    pub adopted: bool,
    /// Name of the workspace this profile is a member of, when any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// The `oauthAccount`/`userID` identity metadata Claude Code stored in the
    /// shared `.claude.json` for this member's login. Never contains tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<Value>,
}

impl Profile {
    pub fn new(config_dir: PathBuf) -> Self {
        Self::create(config_dir, false)
    }

    pub fn new_adopted(config_dir: PathBuf) -> Self {
        Self::create(config_dir, true)
    }

    pub fn new_member(config_dir: PathBuf, workspace: String, identity: Option<Value>) -> Self {
        let mut profile = Self::create(config_dir, false);
        profile.workspace = Some(workspace);
        profile.identity = identity;
        profile
    }

    fn create(config_dir: PathBuf, adopted: bool) -> Self {
        Self {
            config_dir,
            created_at: unix_now(),
            adopted,
            workspace: None,
            identity: None,
        }
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn state_version() -> u32 {
    1
}

const MAX_STATE_VERSION: u32 = 2;

pub struct StateLock {
    _file: File,
}

impl StateLock {
    pub fn acquire(paths: &AppPaths) -> Result<Self> {
        ensure_private_dir(&paths.config_dir)?;
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .mode(0o600)
            .open(&paths.lock_file)
            .with_context(|| format!("failed to open {}", paths.lock_file.display()))?;

        loop {
            let result = unsafe { flock(file.as_raw_fd(), LOCK_EX) };
            if result == 0 {
                break;
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error).context("failed to lock profile state");
            }
        }

        Ok(Self { _file: file })
    }
}

pub fn load(paths: &AppPaths) -> Result<State> {
    match File::open(&paths.state_file) {
        Ok(file) => {
            let state: State = serde_json::from_reader(file)
                .with_context(|| format!("failed to parse {}", paths.state_file.display()))?;
            if !(1..=MAX_STATE_VERSION).contains(&state.version) {
                anyhow::bail!(
                    "unsupported state version {} (created by a newer claude-account?)",
                    state.version
                );
            }
            Ok(state)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(State {
            version: state_version(),
            ..State::default()
        }),
        Err(error) => {
            Err(error).with_context(|| format!("failed to read {}", paths.state_file.display()))
        }
    }
}

pub fn save(paths: &AppPaths, state: &State) -> Result<()> {
    ensure_private_dir(&paths.config_dir)?;
    // Persist the lowest version that can represent the state, so an older
    // binary keeps working until workspaces are actually used and refuses the
    // file (instead of silently dropping fields) afterwards.
    let mut snapshot = state.clone();
    snapshot.version = if snapshot.uses_workspaces() {
        MAX_STATE_VERSION
    } else {
        1
    };
    let temporary = temporary_state_path(&paths.state_file);
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)
            .with_context(|| format!("failed to create {}", temporary.display()))?;
        serde_json::to_writer_pretty(&mut file, &snapshot).context("failed to serialize state")?;
        file.write_all(b"\n")
            .context("failed to finish state file")?;
        file.sync_all().context("failed to sync state file")?;
        fs::rename(&temporary, &paths.state_file).with_context(|| {
            format!(
                "failed to replace {} with {}",
                paths.state_file.display(),
                temporary.display()
            )
        })?;
        fs::set_permissions(&paths.state_file, fs::Permissions::from_mode(0o600))
            .context("failed to protect state file")?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

pub fn ensure_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)
        .with_context(|| format!("failed to create directory {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("failed to protect directory {}", path.display()))?;
    Ok(())
}

fn temporary_state_path(state_file: &Path) -> PathBuf {
    let filename = state_file
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("state.json");
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    state_file.with_file_name(format!("{filename}.tmp.{}.{}", std::process::id(), nonce))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_from_0_1_1_state_files_load_as_managed() {
        let profile: Profile = serde_json::from_str(
            r#"{"config_dir":"/home/user/.local/share/claude-account/profiles/work","created_at":1}"#,
        )
        .unwrap();
        assert!(!profile.adopted);
    }

    #[test]
    fn state_round_trip_preserves_profiles() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_roots(temp.path().join("config"), temp.path().join("data"));
        let mut state = State {
            version: 1,
            ..State::default()
        };
        state.active = Some("work".to_owned());
        state
            .profiles
            .insert("work".to_owned(), Profile::new(paths.profile_dir("work")));

        let _lock = StateLock::acquire(&paths).unwrap();
        save(&paths, &state).unwrap();
        let loaded = load(&paths).unwrap();

        assert_eq!(loaded.active.as_deref(), Some("work"));
        assert!(loaded.profiles.contains_key("work"));
    }

    #[test]
    fn workspaces_bump_the_saved_state_version() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_roots(temp.path().join("config"), temp.path().join("data"));
        let mut state = State {
            version: 1,
            ..State::default()
        };
        state
            .profiles
            .insert("work".to_owned(), Profile::new(paths.profile_dir("work")));

        let _lock = StateLock::acquire(&paths).unwrap();
        save(&paths, &state).unwrap();
        let plain: Value = serde_json::from_slice(&fs::read(&paths.state_file).unwrap()).unwrap();
        assert_eq!(plain["version"], 1);
        assert!(plain.get("workspaces").is_none());

        state.workspaces.insert(
            "shared".to_owned(),
            Workspace::new(temp.path().join("shared"), true),
        );
        save(&paths, &state).unwrap();
        let versioned: Value =
            serde_json::from_slice(&fs::read(&paths.state_file).unwrap()).unwrap();
        assert_eq!(versioned["version"], 2);
        let loaded = load(&paths).unwrap();
        assert_eq!(loaded.workspaces["shared"].dir, temp.path().join("shared"));
        assert!(loaded.workspaces["shared"].external);
    }

    #[test]
    fn future_state_versions_are_refused() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_roots(temp.path().join("config"), temp.path().join("data"));
        ensure_private_dir(&paths.config_dir).unwrap();
        fs::write(&paths.state_file, r#"{"version":3}"#).unwrap();
        let error = load(&paths).unwrap_err();
        assert!(
            error.to_string().contains("unsupported state version 3"),
            "{error:#}"
        );
    }
}
