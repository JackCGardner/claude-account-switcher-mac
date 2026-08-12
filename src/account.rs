use std::env;
use std::fs::{self, OpenOptions};
use std::os::unix::fs::{symlink, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};

use crate::claude_json;
use crate::paths::AppPaths;
use crate::process;
use crate::state::{self, Profile, StateLock};
use crate::workspace;

#[derive(Debug, Parser)]
#[command(
    name = "claude account",
    version,
    about = "Manage isolated Claude Code accounts"
)]
pub struct AccountCli {
    #[command(subcommand)]
    command: AccountCommand,
}

#[derive(Debug, Subcommand)]
enum AccountCommand {
    /// Create a profile and open Claude Code's normal login flow
    Add {
        /// Profile name, such as work or personal
        name: String,
        /// Pre-fill the email address in Claude's login flow
        #[arg(long)]
        email: Option<String>,
        /// Force SSO authentication
        #[arg(long)]
        sso: bool,
        /// Authenticate with Anthropic Console instead of a subscription
        #[arg(long)]
        console: bool,
    },
    /// Register an existing CLAUDE_CONFIG_DIR as a profile without copying or
    /// modifying it
    Adopt {
        /// Profile name, such as work or personal
        name: String,
        /// The existing Claude configuration directory
        directory: PathBuf,
    },
    /// Select the profile used by future Claude processes
    Use { name: String },
    /// List registered profiles
    List {
        /// Also query each profile's login state, email, and plan
        #[arg(long)]
        status: bool,
    },
    /// Print only the active profile name
    Current,
    /// Log out and unregister a profile
    Remove {
        name: String,
        /// Also delete settings, sessions, plugins, and history
        #[arg(long, requires = "yes")]
        purge: bool,
        /// Confirm permanent deletion with --purge
        #[arg(long)]
        yes: bool,
        /// Allow removing the active profile
        #[arg(long)]
        force: bool,
        /// Unregister without logging the profile out
        #[arg(long, conflicts_with = "purge")]
        keep_login: bool,
    },
    /// Install the transparent `claude` shim
    Install {
        /// Absolute path to the real Claude Code executable
        #[arg(long)]
        real: Option<PathBuf>,
    },
    /// Share one directory between several logins: sessions, memories, and
    /// settings are common, only the subscription differs per member
    Workspace {
        #[command(subcommand)]
        command: workspace::WorkspaceCommand,
    },
}

impl AccountCli {
    pub fn run(self, paths: &AppPaths) -> Result<()> {
        match self.command {
            AccountCommand::Add {
                name,
                email,
                sso,
                console,
            } => add(paths, &name, email.as_deref(), sso, console),
            AccountCommand::Adopt { name, directory } => adopt(paths, &name, &directory),
            AccountCommand::Use { name } => use_target(paths, &name),
            AccountCommand::List { status } => list(paths, status),
            AccountCommand::Current => current(paths),
            AccountCommand::Remove {
                name,
                purge,
                yes: _,
                force,
                keep_login,
            } => remove(paths, &name, purge, force, keep_login),
            AccountCommand::Install { real } => install(paths, real.as_deref()),
            AccountCommand::Workspace { command } => command.run(paths),
        }
    }
}

fn add(paths: &AppPaths, name: &str, email: Option<&str>, sso: bool, console: bool) -> Result<()> {
    validate_profile_name(name)?;
    let current_executable = env::current_exe().context("failed to locate this executable")?;
    let existing_state = {
        let _lock = StateLock::acquire(paths)?;
        let state = state::load(paths)?;
        if state.profiles.contains_key(name) {
            bail!("profile `{name}` already exists");
        }
        if state.workspaces.contains_key(name) {
            bail!(
                "a workspace named `{name}` already exists; workspace and profile names share \
                 one namespace"
            );
        }
        state
    };

    let real_claude = process::resolve_real_claude(
        existing_state.real_claude.as_deref(),
        &current_executable,
        paths,
    )?;

    match process::fresh_dir_sees_login(&real_claude, paths) {
        Some(true) => bail!(
            "cannot add a profile: a brand-new configuration directory already sees an existing \
             login, so this Claude Code build shares credentials across profiles (older macOS \
             builds keep a single keychain item for every CLAUDE_CONFIG_DIR). Update Claude Code \
             with `claude update` and retry"
        ),
        Some(false) => {}
        None => eprintln!(
            "warning: could not verify that Claude Code isolates credentials per profile; \
             continuing"
        ),
    }

    let profile_dir = paths.profile_dir(name);
    if let Ok(metadata) = fs::symlink_metadata(&profile_dir) {
        if metadata.file_type().is_symlink() {
            bail!(
                "{} is a leftover workspace member link; reuse it with `claude account \
                 workspace join`, or delete the link first",
                profile_dir.display()
            );
        }
    }
    state::ensure_private_dir(&profile_dir)?;

    println!("Logging in profile `{name}` using Claude Code...");
    let mut login = process::managed_command(&real_claude, &profile_dir);
    login.args(["auth", "login"]);
    if let Some(email) = email {
        login.args(["--email", email]);
    }
    if sso {
        login.arg("--sso");
    }
    if console {
        login.arg("--console");
    }
    let login_status = login.status().context("failed to start Claude login")?;
    if !login_status.success() {
        bail!(
            "Claude login failed for `{name}`; the profile directory was preserved so you can retry"
        );
    }

    let auth_status = process::auth_status(&real_claude, &profile_dir, false)
        .context("failed to verify Claude login")?;
    if !auth_status.logged_in {
        bail!("Claude did not report a valid login for profile `{name}`");
    }

    if let Some(true) = process::fresh_dir_sees_login(&real_claude, paths) {
        bail!(
            "Claude Code stored this login in shared credential storage instead of isolating it \
             per profile; it may have replaced another account's login. Undo it with \
             `CLAUDE_CONFIG_DIR='{}' claude auth logout`, update Claude Code with \
             `claude update`, and retry",
            profile_dir.display()
        );
    }

    claude_json::complete_onboarding(&profile_dir)?;

    let first_profile;
    {
        let _lock = StateLock::acquire(paths)?;
        let mut state = state::load(paths)?;
        if state.profiles.contains_key(name) {
            bail!("profile `{name}` was added by another process");
        }
        first_profile = state.profiles.is_empty();
        state.real_claude = Some(real_claude);
        state
            .profiles
            .insert(name.to_owned(), Profile::new(profile_dir));
        if first_profile {
            state.active = Some(name.to_owned());
        }
        state::save(paths, &state)?;
    }

    if first_profile {
        println!("Added `{name}` and made it active.");
    } else {
        println!("Added `{name}`. Activate it with `claude account use {name}`.");
    }
    Ok(())
}

pub(crate) fn adopt(paths: &AppPaths, name: &str, directory: &Path) -> Result<()> {
    validate_profile_name(name)?;
    let directory = normalize_adopted_directory(directory)?;
    let metadata = fs::metadata(&directory)
        .with_context(|| format!("cannot adopt {}", directory.display()))?;
    if !metadata.is_dir() {
        bail!("cannot adopt {}: not a directory", directory.display());
    }

    let current_executable = env::current_exe().context("failed to locate this executable")?;
    let configured = {
        let _lock = StateLock::acquire(paths)?;
        let state = state::load(paths)?;
        ensure_unregistered(&state, name, &directory)?;
        state.real_claude
    };
    let real_claude =
        process::resolve_real_claude(configured.as_deref(), &current_executable, paths)?;

    match process::fresh_dir_sees_login(&real_claude, paths) {
        Some(true) => bail!(
            "cannot adopt a profile: a brand-new configuration directory already sees an existing \
             login, so this Claude Code build shares credentials across profiles (older macOS \
             builds keep a single keychain item for every CLAUDE_CONFIG_DIR) and switching \
             between profiles cannot work. Update Claude Code with `claude update` and retry"
        ),
        Some(false) => {}
        None => eprintln!(
            "warning: could not verify that Claude Code isolates credentials per profile; \
             continuing"
        ),
    }

    match process::auth_status(&real_claude, &directory, true) {
        Ok(status) if status.logged_in => {}
        Ok(_) => eprintln!(
            "note: {} has no active login; activate the profile and run `claude auth login` \
             when ready",
            directory.display()
        ),
        Err(error) => eprintln!(
            "warning: could not check the login state of {}: {error:#}",
            directory.display()
        ),
    }

    let is_default = process::is_default_claude_config_dir(
        &directory,
        env::var_os("HOME").map(PathBuf::from).as_deref(),
    );

    let first_profile;
    {
        let _lock = StateLock::acquire(paths)?;
        let mut state = state::load(paths)?;
        ensure_unregistered(&state, name, &directory)?;
        first_profile = state.profiles.is_empty();
        state.real_claude = Some(real_claude);
        state
            .profiles
            .insert(name.to_owned(), Profile::new_adopted(directory.clone()));
        if first_profile {
            state.active = Some(name.to_owned());
        }
        state::save(paths, &state)?;
    }

    println!("Adopted `{name}` from {}.", directory.display());
    if is_default {
        println!(
            "This is Claude's default directory, so the profile runs Claude without \
             CLAUDE_CONFIG_DIR and shares the login of a plain `claude` command."
        );
    }
    if first_profile {
        println!("`{name}` is now active.");
    } else {
        println!("Activate it with `claude account use {name}`.");
    }
    Ok(())
}

fn ensure_unregistered(state: &state::State, name: &str, directory: &Path) -> Result<()> {
    if state.profiles.contains_key(name) {
        bail!("profile `{name}` already exists");
    }
    if state.workspaces.contains_key(name) {
        bail!("a workspace named `{name}` already exists; workspace and profile names share one namespace");
    }
    for (existing, profile) in &state.profiles {
        if profile.config_dir == directory {
            bail!(
                "{} is already registered as profile `{existing}`",
                directory.display()
            );
        }
    }
    for (existing, workspace) in &state.workspaces {
        if workspace.dir == directory {
            bail!(
                "{} is the storage of workspace `{existing}`; add a login to it with \
                 `claude account workspace join {existing} NAME`",
                directory.display()
            );
        }
    }
    Ok(())
}

/// Make the path absolute and lexically drop `.`/`..` segments and trailing
/// slashes without resolving symlinks: Claude Code derives its
/// credential-storage key from the literal path string, so the stored spelling
/// must stay stable, and two spellings of the same directory must normalize
/// identically.
fn normalize_adopted_directory(directory: &Path) -> Result<PathBuf> {
    let absolute = if directory.is_absolute() {
        directory.to_path_buf()
    } else {
        env::current_dir()
            .context("failed to resolve the current directory")?
            .join(directory)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    bail!(
                        "cannot adopt {}: path escapes the filesystem root",
                        directory.display()
                    );
                }
            }
            other => normalized.push(other),
        }
    }
    Ok(normalized)
}

/// `use NAME` where NAME is a workspace (target it, keeping or inferring its
/// selected member), a workspace member (select it and target its workspace),
/// or a standalone profile (target it directly).
pub(crate) fn use_target(paths: &AppPaths, name: &str) -> Result<()> {
    validate_profile_name(name)?;
    let _lock = StateLock::acquire(paths)?;
    let mut state = state::load(paths)?;

    if state.workspaces.contains_key(name) {
        let members: Vec<String> = state
            .profiles
            .iter()
            .filter(|(_, profile)| profile.workspace.as_deref() == Some(name))
            .map(|(member, _)| member.clone())
            .collect();
        if members.is_empty() {
            bail!(
                "workspace `{name}` has no members yet; add one with `claude account workspace \
                 join {name} PROFILE`"
            );
        }
        let selected = match state.workspaces[name].selected.clone() {
            Some(selected) if state.profiles.contains_key(&selected) => selected,
            _ if members.len() == 1 => members[0].clone(),
            _ => bail!(
                "workspace `{name}` has no selected member; pick one with `claude account use \
                 MEMBER` (members: {})",
                members.join(", ")
            ),
        };
        workspace::select_member(&mut state, name, &selected);
        state.active = Some(name.to_owned());
        state::save(paths, &state)?;
        println!(
            "Now targeting workspace `{name}` (member `{selected}`) for new Claude processes."
        );
        return Ok(());
    }

    let Some(profile) = state.profiles.get(name).cloned() else {
        bail!("`{name}` is not a registered profile or workspace");
    };
    if let Some(workspace_name) = profile.workspace.clone() {
        workspace::select_member(&mut state, &workspace_name, name);
        state.active = Some(workspace_name.clone());
        state::save(paths, &state)?;
        println!(
            "Selected `{name}` in workspace `{workspace_name}`; new Claude processes use it \
             (shared sessions and memories, separate login)."
        );
        if state.profiles[name].identity.is_none() {
            println!(
                "note: no recorded sign-in identity for `{name}` yet; if Claude shows another \
                 account's email, run `claude auth login` once."
            );
        }
        return Ok(());
    }

    state.active = Some(name.to_owned());
    state::save(paths, &state)?;
    println!("Now using `{name}` for new Claude processes.");
    Ok(())
}

fn list(paths: &AppPaths, with_status: bool) -> Result<()> {
    let state = state::load(paths)?;
    if state.profiles.is_empty() {
        println!("No profiles. Add one with `claude account add NAME`.");
        return Ok(());
    }
    let default_profile = state
        .active
        .as_deref()
        .and_then(|target| state.resolve_target(target).ok())
        .map(|(profile_name, _)| profile_name);
    for (name, profile) in &state.profiles {
        let marker = if default_profile.as_deref() == Some(name.as_str()) {
            "*"
        } else {
            " "
        };
        let mut line = format!("{marker} {name}");
        if profile.adopted {
            line.push_str(&format!("  ({})", profile.config_dir.display()));
        }
        if let Some(workspace_name) = &profile.workspace {
            line.push_str(&format!("  [workspace {workspace_name}]"));
        }
        if with_status {
            line.push_str(&format!("  {}", profile_status(&state, profile)));
        }
        println!("{line}");
    }
    Ok(())
}

fn profile_status(state: &state::State, profile: &Profile) -> String {
    let Some(real_claude) = state.real_claude.as_deref() else {
        return "[status unavailable: run `claude-account install`]".to_owned();
    };
    match process::auth_status(real_claude, &profile.config_dir, true) {
        Ok(status) if status.logged_in => {
            let email = status.email.unwrap_or_else(|| "logged in".to_owned());
            match status.subscription_type {
                Some(plan) => format!("{email} ({plan})"),
                None => email,
            }
        }
        Ok(_) => "logged out".to_owned(),
        Err(_) => "[status unavailable]".to_owned(),
    }
}

fn current(paths: &AppPaths) -> Result<()> {
    let state = state::load(paths)?;
    let target = state.active.as_deref().context("no active profile")?;
    let (profile_name, _) = state.resolve_target(target)?;
    println!("{profile_name}");
    Ok(())
}

pub(crate) fn remove(
    paths: &AppPaths,
    name: &str,
    purge: bool,
    force: bool,
    keep_login: bool,
) -> Result<()> {
    validate_profile_name(name)?;
    let (profile, real_claude, is_active, member_workspace) = {
        let _lock = StateLock::acquire(paths)?;
        let state = state::load(paths)?;
        let profile = state
            .profiles
            .get(name)
            .cloned()
            .with_context(|| format!("profile `{name}` does not exist"))?;
        let member_workspace = profile
            .workspace
            .as_ref()
            .and_then(|workspace_name| state.workspaces.get(workspace_name))
            .cloned();
        if purge && profile.workspace.is_some() {
            bail!(
                "`{name}` is a workspace member; members share the workspace directory, so \
                 removing one never deletes shared data. Remove it without --purge (this only \
                 logs out its login), or remove the whole workspace with `claude account \
                 workspace remove`"
            );
        }
        if purge && profile.adopted {
            bail!(
                "refusing to purge adopted directory {}; remove the profile without --purge and \
                 delete the directory yourself if that is what you want",
                profile.config_dir.display()
            );
        }
        let is_active = state.active.as_deref() == Some(name);
        if is_active && !force {
            bail!(
                "`{name}` is active; switch profiles first, or pass --force to leave no active profile"
            );
        }
        (
            profile,
            state.real_claude.clone(),
            is_active,
            member_workspace,
        )
    };
    let adopted = profile.adopted;
    let is_founding_member = member_workspace
        .as_ref()
        .is_some_and(|workspace| workspace.dir == profile.config_dir);

    if keep_login || adopted {
        if adopted && !keep_login {
            let is_default = process::is_default_claude_config_dir(
                &profile.config_dir,
                env::var_os("HOME").map(PathBuf::from).as_deref(),
            );
            if is_default {
                println!(
                    "Leaving the adopted profile `{name}` logged in. To also log it out, run \
                     `claude auth logout`."
                );
            } else {
                println!(
                    "Leaving the adopted profile `{name}` logged in. To also log it out, run \
                     `CLAUDE_CONFIG_DIR='{}' claude auth logout`.",
                    profile.config_dir.display()
                );
            }
        }
    } else {
        let real_claude = real_claude.context("real Claude executable is not configured")?;
        println!("Logging out profile `{name}`...");
        let logout_status = process::managed_command(&real_claude, &profile.config_dir)
            .args(["auth", "logout"])
            .status()
            .context("failed to start Claude logout")?;
        if !logout_status.success() {
            bail!("Claude logout failed; profile `{name}` was not removed");
        }
    }

    {
        let _lock = StateLock::acquire(paths)?;
        let mut state = state::load(paths)?;
        state.profiles.remove(name);
        if is_active && state.active.as_deref() == Some(name) {
            state.active = None;
        }
        if let Some(workspace_name) = &profile.workspace {
            let remaining: Vec<String> = state
                .profiles
                .iter()
                .filter(|(_, entry)| entry.workspace.as_deref() == Some(workspace_name.as_str()))
                .map(|(member, _)| member.clone())
                .collect();
            if let Some(entry) = state.workspaces.get_mut(workspace_name) {
                if entry.selected.as_deref() == Some(name) {
                    if remaining.len() == 1 {
                        entry.selected = Some(remaining[0].clone());
                        println!(
                            "Workspace `{workspace_name}` now selects its remaining member \
                             `{}`.",
                            remaining[0]
                        );
                    } else {
                        entry.selected = None;
                        if !remaining.is_empty() {
                            println!(
                                "Workspace `{workspace_name}` has no selected member; pick one \
                                 with `claude account use MEMBER` (members: {}).",
                                remaining.join(", ")
                            );
                        }
                    }
                }
            }
        }
        state::save(paths, &state)?;
    }

    if let Some(workspace_name) = &profile.workspace {
        if !is_founding_member {
            if keep_login {
                println!(
                    "Kept the member link {}; joining the workspace again with the same name \
                     will reuse its login.",
                    profile.config_dir.display()
                );
            } else if let Ok(metadata) = fs::symlink_metadata(&profile.config_dir) {
                if metadata.file_type().is_symlink() {
                    let _ = fs::remove_file(&profile.config_dir);
                }
            }
        }
        // Logging this member out may have cleared the shared identity
        // fields; restore the selected member's.
        let state = state::load(paths)?;
        workspace::write_selected_identity(&state, workspace_name);
    }

    if purge {
        let expected = paths.profile_dir(name);
        if profile.config_dir != expected {
            bail!(
                "refusing to purge unexpected directory {}; expected {}",
                profile.config_dir.display(),
                expected.display()
            );
        }
        let metadata = fs::symlink_metadata(&expected)
            .with_context(|| format!("failed to inspect {}", expected.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            bail!("refusing to purge a symlink or non-directory");
        }
        fs::remove_dir_all(&expected)
            .with_context(|| format!("failed to purge {}", expected.display()))?;
        println!("Removed `{name}` and permanently deleted its local data.");
    } else if let Some(workspace) = &member_workspace {
        println!(
            "Removed member `{name}`. The workspace directory {} was not touched.",
            workspace.dir.display()
        );
    } else if adopted {
        println!(
            "Removed `{name}`. The adopted directory {} was not touched.",
            profile.config_dir.display()
        );
    } else {
        println!(
            "Removed `{name}`. Its non-credential data remains at {}.",
            profile.config_dir.display()
        );
    }
    Ok(())
}

fn install(paths: &AppPaths, explicit_real: Option<&Path>) -> Result<()> {
    let current_executable = env::current_exe().context("failed to locate this executable")?;
    let configured = {
        let _lock = StateLock::acquire(paths)?;
        state::load(paths)?.real_claude
    };
    let real_claude = match explicit_real {
        Some(path) => {
            if !path.is_absolute() {
                bail!("--real must be an absolute path");
            }
            process::validate_executable(path)?;
            path.to_path_buf()
        }
        None => process::resolve_real_claude(configured.as_deref(), &current_executable, paths)?,
    };

    state::ensure_private_dir(&paths.data_dir)?;
    state::ensure_private_dir(&paths.shim_dir)?;
    let libexec_dir = paths
        .installed_executable
        .parent()
        .context("invalid installation path")?;
    state::ensure_private_dir(libexec_dir)?;

    let same_executable = fs::canonicalize(&current_executable).ok()
        == fs::canonicalize(&paths.installed_executable).ok();
    if !same_executable {
        let temporary = paths
            .installed_executable
            .with_extension(format!("tmp.{}", std::process::id()));
        let mut destination = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o755)
            .open(&temporary)
            .with_context(|| format!("failed to create {}", temporary.display()))?;
        let mut source = fs::File::open(&current_executable)
            .with_context(|| format!("failed to open {}", current_executable.display()))?;
        std::io::copy(&mut source, &mut destination).context("failed to install executable")?;
        destination
            .sync_all()
            .context("failed to sync executable")?;
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o755))?;
        fs::rename(&temporary, &paths.installed_executable)
            .context("failed to activate installed executable")?;
    }

    if let Ok(metadata) = fs::symlink_metadata(&paths.shim) {
        let points_to_us = metadata.file_type().is_symlink()
            && fs::canonicalize(&paths.shim).ok()
                == fs::canonicalize(&paths.installed_executable).ok();
        if !points_to_us {
            bail!(
                "refusing to replace existing non-managed path {}",
                paths.shim.display()
            );
        }
    }

    let temporary_shim = paths
        .shim
        .with_extension(format!("tmp.{}", std::process::id()));
    let _ = fs::remove_file(&temporary_shim);
    symlink(&paths.installed_executable, &temporary_shim)
        .context("failed to create Claude shim")?;
    fs::rename(&temporary_shim, &paths.shim).context("failed to activate Claude shim")?;

    {
        let _lock = StateLock::acquire(paths)?;
        let mut state = state::load(paths)?;
        state.real_claude = Some(real_claude.clone());
        state::save(paths, &state)?;
    }

    println!("Installed claude-account.");
    println!("Real Claude: {}", real_claude.display());
    println!("Shim: {}", paths.shim.display());
    println!();
    print_path_instructions(&paths.shim_dir);
    Ok(())
}

fn print_path_instructions(shim_dir: &Path) {
    let shell = env::var("SHELL").unwrap_or_default();
    let shell_name = Path::new(&shell)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    match shell_name {
        "fish" => {
            println!("Run this once so the shim comes first on PATH:");
            println!("fish_add_path --move --prepend {}", shim_dir.display());
        }
        shell_name => {
            let startup_file = if shell_name == "zsh" {
                "~/.zshrc"
            } else {
                "~/.bashrc"
            };
            println!("Add this line to {startup_file}, then open a new terminal:");
            println!("export PATH=\"{}:$PATH\"", shim_dir.display());
        }
    }
}

pub(crate) fn validate_profile_name(name: &str) -> Result<()> {
    validate_name("profile", name)
}

pub(crate) fn validate_name(kind: &str, name: &str) -> Result<()> {
    let mut characters = name.chars();
    let first = characters
        .next()
        .with_context(|| format!("{kind} name cannot be empty"))?;
    if !first.is_ascii_alphanumeric()
        || !characters.all(|character| {
            character.is_ascii_alphanumeric() || character == '-' || character == '_'
        })
        || name.len() > 32
    {
        bail!(
            "invalid {kind} name `{name}`; use 1-32 letters, numbers, hyphens, or underscores, \
             starting with a letter or number"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn install_fake_claude(path: &Path, script: &str) {
        fs::write(path, script).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// A fake Claude that stores credentials inside CLAUDE_CONFIG_DIR, the way
    /// real per-profile isolation behaves. Like real builds, it exits nonzero
    /// from `auth status --json` when logged out while still printing JSON.
    fn namespaced_fake_claude(log: &Path) -> String {
        format!(
            "#!/bin/sh\n\
             printf '%s|%s\\n' \"$CLAUDE_CONFIG_DIR\" \"$*\" >> '{}'\n\
             if [ \"$1 $2\" = \"auth login\" ]; then\n\
               touch \"$CLAUDE_CONFIG_DIR/.fake-credentials\"\n\
               exit 0\n\
             fi\n\
             if [ \"$1 $2 $3\" = \"auth status --json\" ]; then\n\
               if [ -f \"$CLAUDE_CONFIG_DIR/.fake-credentials\" ]; then\n\
                 printf '{{\"loggedIn\":true}}\\n'\n\
                 exit 0\n\
               fi\n\
               printf '{{\"loggedIn\":false}}\\n'\n\
               exit 1\n\
             fi\n\
             exit 0\n",
            log.display()
        )
    }

    /// A fake Claude that stores credentials in one shared location regardless
    /// of CLAUDE_CONFIG_DIR, the way old macOS builds used a single keychain
    /// item.
    fn shared_storage_fake_claude(shared_credentials: &Path) -> String {
        format!(
            "#!/bin/sh\n\
             if [ \"$1 $2\" = \"auth login\" ]; then\n\
               touch '{shared}'\n\
               exit 0\n\
             fi\n\
             if [ \"$1 $2 $3\" = \"auth status --json\" ]; then\n\
               if [ -f '{shared}' ]; then\n\
                 printf '{{\"loggedIn\":true}}\\n'\n\
               else\n\
                 printf '{{\"loggedIn\":false}}\\n'\n\
               fi\n\
               exit 0\n\
             fi\n\
             exit 0\n",
            shared = shared_credentials.display()
        )
    }

    fn configure_real_claude(paths: &AppPaths, fake_claude: &Path) {
        let _lock = StateLock::acquire(paths).unwrap();
        let mut initial = state::load(paths).unwrap();
        initial.real_claude = Some(fake_claude.to_path_buf());
        state::save(paths, &initial).unwrap();
    }

    #[test]
    fn profile_name_validation_blocks_path_traversal() {
        for invalid in ["", "../work", ".work", "work space", "work/personal"] {
            assert!(validate_profile_name(invalid).is_err(), "{invalid}");
        }
        for valid in ["work", "personal-2", "team_account"] {
            assert!(validate_profile_name(valid).is_ok(), "{valid}");
        }
    }

    #[test]
    fn add_uses_an_isolated_config_directory() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_roots(temp.path().join("config"), temp.path().join("data"));
        let fake_claude = temp.path().join("claude-real");
        let log = temp.path().join("calls.log");
        install_fake_claude(&fake_claude, &namespaced_fake_claude(&log));
        configure_real_claude(&paths, &fake_claude);

        add(&paths, "work", None, false, false).unwrap();
        let calls = fs::read_to_string(log).unwrap();
        let expected = paths.profile_dir("work").display().to_string();
        assert!(calls.contains(&format!("{expected}|auth login")));
        assert!(calls.contains(&format!("{expected}|auth status --json")));
        let claude_config: Value = serde_json::from_slice(
            &fs::read(paths.profile_dir("work").join(".claude.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(claude_config["hasCompletedOnboarding"], true);
        assert_eq!(state::load(&paths).unwrap().active.as_deref(), Some("work"));
    }

    #[test]
    fn add_rejects_successful_command_that_reports_logged_out() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_roots(temp.path().join("config"), temp.path().join("data"));
        let fake_claude = temp.path().join("claude-real");
        install_fake_claude(
            &fake_claude,
            "#!/bin/sh\n\
             if [ \"$1 $2 $3\" = \"auth status --json\" ]; then\n\
               printf '{\"loggedIn\":false}\\n'\n\
             fi\n\
             exit 0\n",
        );
        configure_real_claude(&paths, &fake_claude);

        let error = add(&paths, "work", None, false, false).unwrap_err();
        assert!(error.to_string().contains("did not report a valid login"));
        assert!(!state::load(&paths).unwrap().profiles.contains_key("work"));
        assert!(!paths.profile_dir("work").join(".claude.json").exists());
    }

    #[test]
    fn add_aborts_before_login_when_credential_storage_is_shared() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_roots(temp.path().join("config"), temp.path().join("data"));
        let fake_claude = temp.path().join("claude-real");
        let shared = temp.path().join("shared-credentials");
        install_fake_claude(&fake_claude, &shared_storage_fake_claude(&shared));
        configure_real_claude(&paths, &fake_claude);
        fs::write(&shared, b"an existing login").unwrap();

        let error = add(&paths, "work", None, false, false).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("shares credentials across profiles"),
            "{error:#}"
        );
        assert!(!paths.profile_dir("work").exists());
        assert!(!state::load(&paths).unwrap().profiles.contains_key("work"));
    }

    #[test]
    fn add_aborts_when_login_lands_in_shared_storage() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_roots(temp.path().join("config"), temp.path().join("data"));
        let fake_claude = temp.path().join("claude-real");
        let shared = temp.path().join("shared-credentials");
        install_fake_claude(&fake_claude, &shared_storage_fake_claude(&shared));
        configure_real_claude(&paths, &fake_claude);

        let error = add(&paths, "work", None, false, false).unwrap_err();
        assert!(
            error.to_string().contains("shared credential storage"),
            "{error:#}"
        );
        assert!(!state::load(&paths).unwrap().profiles.contains_key("work"));
    }

    #[test]
    fn adopt_registers_a_directory_without_modifying_it() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_roots(temp.path().join("config"), temp.path().join("data"));
        let fake_claude = temp.path().join("claude-real");
        let log = temp.path().join("calls.log");
        install_fake_claude(&fake_claude, &namespaced_fake_claude(&log));
        configure_real_claude(&paths, &fake_claude);

        let external = temp.path().join("external-claude-dir");
        fs::create_dir_all(&external).unwrap();
        fs::write(external.join(".fake-credentials"), b"logged in").unwrap();
        fs::write(external.join("settings.json"), b"{}").unwrap();

        adopt(&paths, "work", &external).unwrap();

        let state = state::load(&paths).unwrap();
        assert_eq!(state.active.as_deref(), Some("work"));
        assert_eq!(state.profiles["work"].config_dir, external);
        assert!(!external.join(".claude.json").exists());

        let error = adopt(&paths, "work-again", &external).unwrap_err();
        assert!(
            error.to_string().contains("already registered"),
            "{error:#}"
        );
        let dotted = temp
            .path()
            .join("nested")
            .join("..")
            .join("external-claude-dir");
        let error = adopt(&paths, "work-dotted", &dotted).unwrap_err();
        assert!(
            error.to_string().contains("already registered"),
            "a `..` spelling of a registered directory must be detected: {error:#}"
        );
        let error = adopt(&paths, "work", temp.path()).unwrap_err();
        assert!(error.to_string().contains("already exists"), "{error:#}");
    }

    #[test]
    fn adopt_normalizes_dot_and_parent_segments() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("external")).unwrap();
        let with_slash = format!("{}/external/", temp.path().display());
        assert_eq!(
            normalize_adopted_directory(Path::new(&with_slash)).unwrap(),
            temp.path().join("external")
        );
        let with_parent = format!("{}/nested/../external", temp.path().display());
        assert_eq!(
            normalize_adopted_directory(Path::new(&with_parent)).unwrap(),
            temp.path().join("external")
        );
        assert!(normalize_adopted_directory(Path::new("/..")).is_err());
    }

    #[test]
    fn adopt_aborts_when_credential_storage_is_shared() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_roots(temp.path().join("config"), temp.path().join("data"));
        let fake_claude = temp.path().join("claude-real");
        let shared = temp.path().join("shared-credentials");
        install_fake_claude(&fake_claude, &shared_storage_fake_claude(&shared));
        configure_real_claude(&paths, &fake_claude);
        fs::write(&shared, b"an existing login").unwrap();

        let external = temp.path().join("external-claude-dir");
        fs::create_dir_all(&external).unwrap();
        let error = adopt(&paths, "work", &external).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("shares credentials across profiles"),
            "{error:#}"
        );
        assert!(!state::load(&paths).unwrap().profiles.contains_key("work"));
    }

    #[test]
    fn remove_rejects_purge_combined_with_keep_login() {
        let result = AccountCli::try_parse_from([
            "claude account",
            "remove",
            "work",
            "--purge",
            "--yes",
            "--keep-login",
        ]);
        assert!(result.is_err());
    }

    #[test]
    fn remove_keeps_adopted_directories_logged_in() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_roots(temp.path().join("config"), temp.path().join("data"));
        let fake_claude = temp.path().join("claude-real");
        let log = temp.path().join("calls.log");
        install_fake_claude(&fake_claude, &namespaced_fake_claude(&log));
        configure_real_claude(&paths, &fake_claude);

        let external = temp.path().join("external-claude-dir");
        fs::create_dir_all(&external).unwrap();
        adopt(&paths, "work", &external).unwrap();

        remove(&paths, "work", false, true, false).unwrap();
        assert!(external.is_dir());
        assert!(!state::load(&paths).unwrap().profiles.contains_key("work"));
        let calls = fs::read_to_string(&log).unwrap();
        assert!(
            !calls.contains("auth logout"),
            "adopted profile must not be logged out: {calls}"
        );
    }

    #[test]
    fn remove_with_keep_login_skips_logout() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_roots(temp.path().join("config"), temp.path().join("data"));
        let fake_claude = temp.path().join("claude-real");
        let log = temp.path().join("calls.log");
        install_fake_claude(&fake_claude, &namespaced_fake_claude(&log));
        configure_real_claude(&paths, &fake_claude);

        add(&paths, "work", None, false, false).unwrap();
        remove(&paths, "work", false, true, true).unwrap();
        assert!(!state::load(&paths).unwrap().profiles.contains_key("work"));
        let calls = fs::read_to_string(&log).unwrap();
        assert!(!calls.contains("auth logout"), "{calls}");
    }

    #[test]
    fn remove_refuses_to_purge_adopted_directories() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_roots(temp.path().join("config"), temp.path().join("data"));
        let fake_claude = temp.path().join("claude-real");
        let log = temp.path().join("calls.log");
        install_fake_claude(&fake_claude, &namespaced_fake_claude(&log));
        configure_real_claude(&paths, &fake_claude);

        let external = temp.path().join("external-claude-dir");
        fs::create_dir_all(&external).unwrap();
        adopt(&paths, "work", &external).unwrap();

        let error = remove(&paths, "work", true, true, false).unwrap_err();
        assert!(error.to_string().contains("refusing to purge"), "{error:#}");
        assert!(external.is_dir());
        assert!(
            state::load(&paths).unwrap().profiles.contains_key("work"),
            "profile must remain registered when purge is refused"
        );
    }
}
