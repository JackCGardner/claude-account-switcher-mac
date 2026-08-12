use std::env;
use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use clap::Subcommand;

use crate::account::{validate_name, validate_profile_name};
use crate::claude_json;
use crate::paths::AppPaths;
use crate::process;
use crate::state::{self, Profile, StateLock, Workspace};

#[derive(Debug, Subcommand)]
pub enum WorkspaceCommand {
    /// Create a shared workspace: one directory whose sessions, memories,
    /// settings, and history are used by every member profile, while each
    /// member keeps its own separate login
    Create {
        /// Workspace name, such as work
        name: String,
        /// Use an existing profile's directory as the workspace storage, in
        /// place and without copying; the profile becomes the first member
        #[arg(long)]
        from_profile: Option<String>,
    },
    /// Add a member profile to a workspace and open Claude Code's login flow
    /// for it (sign in with a different subscription here)
    Join {
        /// The workspace to join
        workspace: String,
        /// Name for the new member profile
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
    /// List workspaces and their member profiles
    List,
    /// Unregister a workspace once its member links have been removed
    Remove {
        name: String,
        /// Also delete the workspace directory (only for workspaces created
        /// without --from-profile)
        #[arg(long, requires = "yes")]
        purge: bool,
        /// Confirm permanent deletion with --purge
        #[arg(long)]
        yes: bool,
    },
}

impl WorkspaceCommand {
    pub fn run(self, paths: &AppPaths) -> Result<()> {
        match self {
            Self::Create { name, from_profile } => create(
                paths,
                &name,
                from_profile.as_deref(),
                env::var_os("HOME").map(PathBuf::from).as_deref(),
            ),
            Self::Join {
                workspace,
                name,
                email,
                sso,
                console,
            } => join(paths, &workspace, &name, email.as_deref(), sso, console),
            Self::List => list(paths),
            Self::Remove {
                name,
                purge,
                yes: _,
            } => remove(paths, &name, purge),
        }
    }
}

fn create(
    paths: &AppPaths,
    name: &str,
    from_profile: Option<&str>,
    home: Option<&Path>,
) -> Result<()> {
    validate_name("workspace", name)?;
    let _lock = StateLock::acquire(paths)?;
    let mut state = state::load(paths)?;
    if state.workspaces.contains_key(name) {
        bail!("workspace `{name}` already exists");
    }

    match from_profile {
        Some(profile_name) => {
            let profile = state
                .profiles
                .get(profile_name)
                .with_context(|| format!("profile `{profile_name}` does not exist"))?;
            if let Some(existing) = &profile.workspace {
                bail!("profile `{profile_name}` already belongs to workspace `{existing}`");
            }
            let directory = profile.config_dir.clone();
            if process::is_default_claude_config_dir(&directory, home) {
                bail!(
                    "cannot use Claude's default ~/.claude directory as a workspace: Claude Code \
                     keeps its top-level state in ~/.claude.json when that directory is used \
                     without CLAUDE_CONFIG_DIR, so members would not share it. Use a non-default \
                     directory instead"
                );
            }
            for (existing, workspace) in &state.workspaces {
                if workspace.dir == directory {
                    bail!(
                        "{} is already the storage of workspace `{existing}`",
                        directory.display()
                    );
                }
            }

            let identity = claude_json::read_identity(&directory).ok().flatten();
            let entry = state
                .profiles
                .get_mut(profile_name)
                .expect("profile existence checked above");
            entry.workspace = Some(name.to_owned());
            if identity.is_some() {
                entry.identity = identity;
            }
            state
                .workspaces
                .insert(name.to_owned(), Workspace::new(directory.clone(), true));
            state::save(paths, &state)?;

            println!("Created workspace `{name}` around {}.", directory.display());
            println!(
                "`{profile_name}` is its first member. Add another subscription with \
                 `claude account workspace join {name} PROFILE`."
            );
        }
        None => {
            let directory = paths.workspace_dir(name);
            state::ensure_private_dir(&directory)?;
            state
                .workspaces
                .insert(name.to_owned(), Workspace::new(directory.clone(), false));
            state::save(paths, &state)?;

            println!(
                "Created empty workspace `{name}` at {}.",
                directory.display()
            );
            println!("Add its first login with `claude account workspace join {name} PROFILE`.");
        }
    }
    Ok(())
}

fn join(
    paths: &AppPaths,
    workspace_name: &str,
    name: &str,
    email: Option<&str>,
    sso: bool,
    console: bool,
) -> Result<()> {
    validate_profile_name(name)?;
    let current_executable = env::current_exe().context("failed to locate this executable")?;

    let (workspace, configured, member_dirs) = {
        let _lock = StateLock::acquire(paths)?;
        let state = state::load(paths)?;
        let workspace = state
            .workspaces
            .get(workspace_name)
            .cloned()
            .with_context(|| {
                format!(
                    "workspace `{workspace_name}` does not exist; create it with \
                     `claude account workspace create {workspace_name} --from-profile PROFILE`"
                )
            })?;
        if state.profiles.contains_key(name) {
            bail!("profile `{name}` already exists");
        }
        let member_dirs: Vec<PathBuf> = state
            .profiles
            .values()
            .filter(|profile| profile.workspace.as_deref() == Some(workspace_name))
            .map(|profile| profile.config_dir.clone())
            .collect();
        (workspace, state.real_claude.clone(), member_dirs)
    };

    let metadata = fs::metadata(&workspace.dir)
        .with_context(|| format!("workspace directory {} is missing", workspace.dir.display()))?;
    if !metadata.is_dir() {
        bail!(
            "workspace storage {} is not a directory",
            workspace.dir.display()
        );
    }

    let real_claude =
        process::resolve_real_claude(configured.as_deref(), &current_executable, paths)?;

    match process::fresh_dir_sees_login(&real_claude, paths) {
        Some(true) => bail!(
            "cannot join: a brand-new configuration directory already sees an existing login, so \
             this Claude Code build shares credentials across profiles. Update Claude Code with \
             `claude update` and retry"
        ),
        Some(false) => {}
        None => eprintln!(
            "warning: could not verify that Claude Code isolates credentials per profile; \
             continuing"
        ),
    }

    // Workspace members rely on Claude Code deriving credential storage from
    // the literal CLAUDE_CONFIG_DIR string: a new link to the workspace must
    // NOT see the logins of existing members. On Linux, credentials live in a
    // file inside the (shared) directory itself, so this probe fails there by
    // design and members cannot hold separate logins.
    let any_member_logged_in = member_dirs.iter().any(|dir| {
        process::auth_status(&real_claude, dir, true)
            .map(|status| status.logged_in)
            .unwrap_or(false)
    });
    if any_member_logged_in {
        match symlink_sees_workspace_login(&real_claude, paths, &workspace.dir) {
            Some(true) => bail!(
                "cannot join: Claude Code resolves the member link before choosing credential \
                 storage, so all members of this workspace would share one login (on Linux, \
                 credentials always live inside the shared directory). Separate logins per \
                 member are not possible on this platform or Claude Code build"
            ),
            Some(false) => {}
            None => eprintln!(
                "warning: could not verify that workspace members get separate credential \
                 storage; continuing"
            ),
        }
    }

    // The login below overwrites the account identity in the shared
    // .claude.json, so record the current identity for the member it belongs
    // to (the active one) first.
    {
        let _lock = StateLock::acquire(paths)?;
        let mut state = state::load(paths)?;
        refresh_active_member_identity(&mut state);
        state::save(paths, &state)?;
    }

    let member_path = paths.profile_dir(name);
    let mut created_link = false;
    match fs::symlink_metadata(&member_path) {
        Ok(metadata) => {
            let points_to_workspace = metadata.file_type().is_symlink()
                && fs::read_link(&member_path)
                    .map(|target| target == workspace.dir)
                    .unwrap_or(false);
            if !points_to_workspace {
                bail!(
                    "{} already exists and is not a link to this workspace; remove it first",
                    member_path.display()
                );
            }
            println!(
                "Reusing the existing member link {}.",
                member_path.display()
            );
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {
            state::ensure_private_dir(&paths.profiles_dir)?;
            symlink(&workspace.dir, &member_path).with_context(|| {
                format!(
                    "failed to link {} to {}",
                    member_path.display(),
                    workspace.dir.display()
                )
            })?;
            created_link = true;
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to inspect {}", member_path.display()));
        }
    }

    // A fresh login was performed through the member link, or an existing one
    // is reused. On failure before a login exists, remove the link we made so
    // the join can be retried cleanly.
    let already_logged_in = process::auth_status(&real_claude, &member_path, true)
        .map(|status| status.logged_in)
        .unwrap_or(false);
    let mut captured_identity = None;
    if already_logged_in {
        println!("`{name}` already has a login; skipping Claude's login flow.");
        println!(
            "note: its sign-in identity will be recorded at the next `claude auth login` in \
             this member."
        );
    } else {
        let login_result = (|| -> Result<()> {
            println!("Logging in member `{name}` using Claude Code...");
            println!("(Use the other subscription's account in this login.)");
            let mut login = process::managed_command(&real_claude, &member_path);
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
                bail!("Claude login failed for member `{name}`");
            }
            let auth_status = process::auth_status(&real_claude, &member_path, false)
                .context("failed to verify Claude login")?;
            if !auth_status.logged_in {
                bail!("Claude did not report a valid login for member `{name}`");
            }
            Ok(())
        })();
        if let Err(error) = login_result {
            if created_link {
                let _ = fs::remove_file(&member_path);
            }
            return Err(error);
        }
        captured_identity = claude_json::read_identity(&workspace.dir).ok().flatten();
    }

    // From here on the login exists, so failures keep the link in place and
    // print how to undo the login by hand.
    if let Some(true) = process::fresh_dir_sees_login(&real_claude, paths) {
        bail!(
            "Claude Code stored this login in shared credential storage instead of isolating it \
             per profile; it may have replaced another account's login. Undo it with \
             `CLAUDE_CONFIG_DIR='{}' claude auth logout`, update Claude Code with `claude \
             update`, and retry",
            member_path.display()
        );
    }
    if let Some(true) = symlink_sees_workspace_login(&real_claude, paths, &workspace.dir) {
        bail!(
            "Claude Code stored this login where every workspace member link can see it, so \
             members cannot hold separate logins on this build. Undo it with \
             `CLAUDE_CONFIG_DIR='{}' claude auth logout`",
            member_path.display()
        );
    }

    claude_json::complete_onboarding(&workspace.dir)?;

    let first_profile;
    {
        let _lock = StateLock::acquire(paths)?;
        let mut state = state::load(paths)?;
        if state.profiles.contains_key(name) {
            bail!("profile `{name}` was added by another process");
        }
        if !state.workspaces.contains_key(workspace_name) {
            bail!("workspace `{workspace_name}` was removed by another process");
        }
        first_profile = state.profiles.is_empty();
        state.real_claude = Some(real_claude);
        state.profiles.insert(
            name.to_owned(),
            Profile::new_member(
                member_path.clone(),
                workspace_name.to_owned(),
                captured_identity,
            ),
        );
        if first_profile {
            state.active = Some(name.to_owned());
        } else {
            // The login rewrote the shared identity; put the active member's
            // own identity back so running sessions stay coherent.
            write_active_member_identity(&state);
        }
        state::save(paths, &state)?;
    }

    println!(
        "Joined workspace `{workspace_name}` as `{name}`: sessions, memories, and settings are \
         shared; the login is separate."
    );
    if first_profile {
        println!("`{name}` is now active.");
    } else {
        println!("Activate it with `claude account use {name}`.");
    }
    Ok(())
}

fn list(paths: &AppPaths) -> Result<()> {
    let state = state::load(paths)?;
    if state.workspaces.is_empty() {
        println!(
            "No workspaces. Create one with `claude account workspace create NAME \
             --from-profile PROFILE`."
        );
        return Ok(());
    }
    for (name, workspace) in &state.workspaces {
        println!("{name}  ({})", workspace.dir.display());
        let mut any_member = false;
        for (profile_name, profile) in &state.profiles {
            if profile.workspace.as_deref() != Some(name.as_str()) {
                continue;
            }
            any_member = true;
            let marker = if state.active.as_deref() == Some(profile_name) {
                "*"
            } else {
                " "
            };
            match profile
                .identity
                .as_ref()
                .and_then(claude_json::identity_email)
            {
                Some(email) => println!("  {marker} {profile_name}  {email}"),
                None => println!("  {marker} {profile_name}"),
            }
        }
        if !any_member {
            println!("    (no members yet — `claude account workspace join {name} PROFILE`)");
        }
    }
    Ok(())
}

fn remove(paths: &AppPaths, name: &str, purge: bool) -> Result<()> {
    validate_name("workspace", name)?;
    let _lock = StateLock::acquire(paths)?;
    let mut state = state::load(paths)?;
    let workspace = state
        .workspaces
        .get(name)
        .cloned()
        .with_context(|| format!("workspace `{name}` does not exist"))?;

    let mut linked_members = Vec::new();
    let mut founding = None;
    for (profile_name, profile) in &state.profiles {
        if profile.workspace.as_deref() != Some(name) {
            continue;
        }
        if profile.config_dir == workspace.dir {
            founding = Some(profile_name.clone());
        } else {
            linked_members.push(profile_name.clone());
        }
    }
    if !linked_members.is_empty() {
        bail!(
            "workspace `{name}` still has member profiles: {}. Remove them first with \
             `claude account remove NAME`",
            linked_members.join(", ")
        );
    }
    if purge {
        if workspace.external {
            bail!(
                "refusing to delete {}: the directory pre-existed the workspace; remove the \
                 workspace without --purge and delete the directory yourself if that is what \
                 you want",
                workspace.dir.display()
            );
        }
        let expected = paths.workspace_dir(name);
        if workspace.dir != expected {
            bail!(
                "refusing to purge unexpected directory {}; expected {}",
                workspace.dir.display(),
                expected.display()
            );
        }
    }

    if let Some(founding_name) = &founding {
        let profile = state
            .profiles
            .get_mut(founding_name)
            .expect("membership checked above");
        profile.workspace = None;
        profile.identity = None;
    }
    state.workspaces.remove(name);
    state::save(paths, &state)?;

    if let Some(founding_name) = &founding {
        println!(
            "Detached `{founding_name}`; it keeps {} as a standalone profile.",
            workspace.dir.display()
        );
    }
    if purge {
        let metadata = fs::symlink_metadata(&workspace.dir)
            .with_context(|| format!("failed to inspect {}", workspace.dir.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            bail!("refusing to purge a symlink or non-directory");
        }
        fs::remove_dir_all(&workspace.dir)
            .with_context(|| format!("failed to purge {}", workspace.dir.display()))?;
        println!("Removed workspace `{name}` and permanently deleted its directory.");
    } else {
        println!(
            "Removed workspace `{name}`. Its directory {} was not touched.",
            workspace.dir.display()
        );
    }
    Ok(())
}

/// Empirically check whether a brand-new link to the workspace directory sees
/// the logins of existing members. With literal-path credential keying (recent
/// macOS Claude Code builds) this is false; `Some(true)` means credentials
/// resolve through the link (Linux plaintext storage, or a build that
/// canonicalizes the path), so members cannot hold separate logins.
fn symlink_sees_workspace_login(
    real_claude: &Path,
    paths: &AppPaths,
    workspace_dir: &Path,
) -> Option<bool> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let link =
        paths
            .profiles_dir
            .join(format!(".workspace-probe.{}.{}", std::process::id(), nonce));
    state::ensure_private_dir(&paths.profiles_dir).ok()?;
    symlink(workspace_dir, &link).ok()?;
    let result = process::dir_sees_login(real_claude, &link);
    let _ = fs::remove_file(&link);
    result
}

/// Re-read the shared `.claude.json` identity and store it on the currently
/// active profile when that profile is a workspace member. The file always
/// carries the identity of whichever member logged in last — normally the
/// active one — so this picks up manual `claude auth login` runs too.
pub fn refresh_active_member_identity(state: &mut state::State) {
    let Some(active) = state.active.clone() else {
        return;
    };
    let Some(profile) = state.profiles.get(&active) else {
        return;
    };
    let Some(workspace_name) = profile.workspace.clone() else {
        return;
    };
    let Some(workspace) = state.workspaces.get(&workspace_name) else {
        return;
    };
    let workspace_dir = workspace.dir.clone();
    match claude_json::read_identity(&workspace_dir) {
        Ok(Some(identity)) => {
            state
                .profiles
                .get_mut(&active)
                .expect("active profile fetched above")
                .identity = Some(identity);
        }
        Ok(None) => {}
        Err(error) => eprintln!(
            "warning: could not read the account identity in {}: {error:#}",
            workspace_dir.display()
        ),
    }
}

/// Write the active member's recorded identity into its workspace's shared
/// `.claude.json`, so Claude displays the account that matches the login the
/// member actually uses. Does nothing when the active profile is not a
/// workspace member or has no recorded identity.
pub fn write_active_member_identity(state: &state::State) {
    let Some(active) = state.active.as_deref() else {
        return;
    };
    let Some(profile) = state.profiles.get(active) else {
        return;
    };
    let Some(workspace_name) = profile.workspace.as_deref() else {
        return;
    };
    let Some(workspace) = state.workspaces.get(workspace_name) else {
        return;
    };
    let Some(identity) = profile.identity.as_ref() else {
        return;
    };
    if let Err(error) = claude_json::write_identity(&workspace.dir, identity) {
        eprintln!(
            "warning: could not restore `{active}`'s account identity in {}: {error:#}",
            workspace.dir.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account;
    use serde_json::Value;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    /// A fake Claude that mimics the macOS keychain: credentials are keyed by
    /// the literal CLAUDE_CONFIG_DIR string, while `.claude.json` is written
    /// through the (possibly symlinked) directory, like real file I/O.
    fn keychain_fake_claude(control_dir: &Path) -> String {
        format!(
            "#!/bin/sh\n\
             key=$(printf %s \"$CLAUDE_CONFIG_DIR\" | cksum | cut -d' ' -f1)\n\
             cred='{control}/cred-'$key\n\
             if [ \"$1 $2\" = \"auth login\" ]; then\n\
               cat '{control}/next-email' > \"$cred\"\n\
               email=$(cat \"$cred\")\n\
               printf '{{\"oauthAccount\":{{\"emailAddress\":\"%s\"}},\"userID\":\"uid-%s\"}}' \"$email\" \"$email\" > \"$CLAUDE_CONFIG_DIR/.claude.json\"\n\
               exit 0\n\
             fi\n\
             if [ \"$1 $2\" = \"auth logout\" ]; then\n\
               rm -f \"$cred\"\n\
               exit 0\n\
             fi\n\
             if [ \"$1 $2 $3\" = \"auth status --json\" ]; then\n\
               if [ -f \"$cred\" ]; then\n\
                 printf '{{\"loggedIn\":true,\"email\":\"%s\"}}\\n' \"$(cat \"$cred\")\"\n\
                 exit 0\n\
               fi\n\
               printf '{{\"loggedIn\":false}}\\n'\n\
               exit 1\n\
             fi\n\
             exit 0\n",
            control = control_dir.display()
        )
    }

    /// A fake Claude that stores credentials inside the resolved directory,
    /// the way Linux plaintext storage (or a canonicalizing build) behaves.
    fn resolving_fake_claude() -> &'static str {
        "#!/bin/sh\n\
         if [ \"$1 $2\" = \"auth login\" ]; then\n\
           touch \"$CLAUDE_CONFIG_DIR/.fake-credentials\"\n\
           exit 0\n\
         fi\n\
         if [ \"$1 $2 $3\" = \"auth status --json\" ]; then\n\
           if [ -f \"$CLAUDE_CONFIG_DIR/.fake-credentials\" ]; then\n\
             printf '{\"loggedIn\":true}\\n'\n\
             exit 0\n\
           fi\n\
           printf '{\"loggedIn\":false}\\n'\n\
           exit 1\n\
         fi\n\
         exit 0\n"
    }

    struct Fixture {
        _temp: tempfile::TempDir,
        paths: AppPaths,
        control: PathBuf,
        external: PathBuf,
    }

    fn install_script(path: &Path, script: &str) {
        fs::write(path, script).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn configure_real_claude(paths: &AppPaths, fake_claude: &Path) {
        let _lock = StateLock::acquire(paths).unwrap();
        let mut state = state::load(paths).unwrap();
        state.real_claude = Some(fake_claude.to_path_buf());
        state::save(paths, &state).unwrap();
    }

    fn fake_login(fake_claude: &Path, config_dir: &Path, control: &Path, email: &str) {
        fs::write(control.join("next-email"), email).unwrap();
        let status = Command::new(fake_claude)
            .env("CLAUDE_CONFIG_DIR", config_dir)
            .args(["auth", "login"])
            .status()
            .unwrap();
        assert!(status.success());
    }

    fn shared_claude_json(fixture: &Fixture) -> Value {
        serde_json::from_slice(&fs::read(fixture.external.join(".claude.json")).unwrap()).unwrap()
    }

    fn cred_count(fixture: &Fixture) -> usize {
        fs::read_dir(&fixture.control)
            .unwrap()
            .filter(|entry| {
                entry
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("cred-")
            })
            .count()
    }

    /// An adopted, logged-in profile `founder` whose directory became
    /// workspace `work`.
    fn keychain_fixture() -> Fixture {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_roots(temp.path().join("config"), temp.path().join("data"));
        let control = temp.path().join("control");
        fs::create_dir_all(&control).unwrap();
        let fake_claude = temp.path().join("claude-real");
        install_script(&fake_claude, &keychain_fake_claude(&control));
        configure_real_claude(&paths, &fake_claude);

        let external = temp.path().join("external-claude-dir");
        fs::create_dir_all(&external).unwrap();
        fake_login(&fake_claude, &external, &control, "founder@example.com");
        account::adopt(&paths, "founder", &external).unwrap();
        create(&paths, "work", Some("founder"), None).unwrap();

        Fixture {
            _temp: temp,
            paths,
            control,
            external,
        }
    }

    #[test]
    fn create_from_profile_registers_membership_and_identity() {
        let fixture = keychain_fixture();
        let state = state::load(&fixture.paths).unwrap();
        assert_eq!(state.workspaces["work"].dir, fixture.external);
        assert!(state.workspaces["work"].external);
        assert_eq!(state.profiles["founder"].workspace.as_deref(), Some("work"));
        assert_eq!(
            claude_json::identity_email(state.profiles["founder"].identity.as_ref().unwrap()),
            Some("founder@example.com")
        );

        let error = create(&fixture.paths, "again", Some("founder"), None).unwrap_err();
        assert!(
            error.to_string().contains("already belongs to workspace"),
            "{error:#}"
        );
    }

    #[test]
    fn create_refuses_the_default_claude_directory() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_roots(temp.path().join("config"), temp.path().join("data"));
        let control = temp.path().join("control");
        fs::create_dir_all(&control).unwrap();
        let fake_claude = temp.path().join("claude-real");
        install_script(&fake_claude, &keychain_fake_claude(&control));
        configure_real_claude(&paths, &fake_claude);

        let home = temp.path().join("home");
        let default_dir = home.join(".claude");
        fs::create_dir_all(&default_dir).unwrap();
        account::adopt(&paths, "personal", &default_dir).unwrap();

        let error = create(&paths, "shared", Some("personal"), Some(&home)).unwrap_err();
        assert!(error.to_string().contains("default"), "{error:#}");
        assert!(state::load(&paths).unwrap().workspaces.is_empty());
    }

    #[test]
    fn join_creates_a_member_link_with_its_own_login() {
        let fixture = keychain_fixture();
        fs::write(fixture.control.join("next-email"), "second@example.com").unwrap();
        join(&fixture.paths, "work", "second", None, false, false).unwrap();

        let member_path = fixture.paths.profile_dir("second");
        assert!(fs::symlink_metadata(&member_path)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read_link(&member_path).unwrap(), fixture.external);
        assert_eq!(cred_count(&fixture), 2, "each member holds its own login");

        let state = state::load(&fixture.paths).unwrap();
        assert_eq!(state.active.as_deref(), Some("founder"));
        assert_eq!(state.profiles["second"].workspace.as_deref(), Some("work"));
        assert_eq!(
            claude_json::identity_email(state.profiles["second"].identity.as_ref().unwrap()),
            Some("second@example.com")
        );

        // The active member's identity was restored after the new login.
        assert_eq!(
            shared_claude_json(&fixture)["oauthAccount"]["emailAddress"],
            "founder@example.com"
        );
        assert_eq!(shared_claude_json(&fixture)["hasCompletedOnboarding"], true);
    }

    #[test]
    fn use_swaps_the_shared_identity_between_members() {
        let fixture = keychain_fixture();
        fs::write(fixture.control.join("next-email"), "second@example.com").unwrap();
        join(&fixture.paths, "work", "second", None, false, false).unwrap();

        account::use_profile(&fixture.paths, "second").unwrap();
        assert_eq!(
            shared_claude_json(&fixture)["oauthAccount"]["emailAddress"],
            "second@example.com"
        );
        assert_eq!(
            shared_claude_json(&fixture)["userID"],
            "uid-second@example.com"
        );

        account::use_profile(&fixture.paths, "founder").unwrap();
        assert_eq!(
            shared_claude_json(&fixture)["oauthAccount"]["emailAddress"],
            "founder@example.com"
        );
    }

    #[test]
    fn join_aborts_before_login_when_credentials_resolve_through_links() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_roots(temp.path().join("config"), temp.path().join("data"));
        let fake_claude = temp.path().join("claude-real");
        install_script(&fake_claude, resolving_fake_claude());
        configure_real_claude(&paths, &fake_claude);

        let external = temp.path().join("external-claude-dir");
        fs::create_dir_all(&external).unwrap();
        fs::write(external.join(".fake-credentials"), b"live").unwrap();
        account::adopt(&paths, "founder", &external).unwrap();
        create(&paths, "work", Some("founder"), None).unwrap();

        let error = join(&paths, "work", "second", None, false, false).unwrap_err();
        assert!(
            error.to_string().contains("resolves the member link"),
            "{error:#}"
        );
        assert!(!state::load(&paths).unwrap().profiles.contains_key("second"));
        assert!(
            fs::symlink_metadata(paths.profile_dir("second")).is_err(),
            "no member link may be left behind"
        );
    }

    #[test]
    fn remove_member_logs_out_only_that_member_and_deletes_the_link() {
        let fixture = keychain_fixture();
        fs::write(fixture.control.join("next-email"), "second@example.com").unwrap();
        join(&fixture.paths, "work", "second", None, false, false).unwrap();
        assert_eq!(cred_count(&fixture), 2);

        account::remove(&fixture.paths, "second", false, false, false).unwrap();

        assert_eq!(
            cred_count(&fixture),
            1,
            "only the member's login is removed"
        );
        assert!(fs::symlink_metadata(fixture.paths.profile_dir("second")).is_err());
        let state = state::load(&fixture.paths).unwrap();
        assert!(!state.profiles.contains_key("second"));
        assert!(state.workspaces.contains_key("work"));
        assert!(fixture.external.is_dir());
        assert_eq!(
            shared_claude_json(&fixture)["oauthAccount"]["emailAddress"],
            "founder@example.com"
        );
    }

    #[test]
    fn purge_is_refused_for_workspace_members() {
        let fixture = keychain_fixture();
        fs::write(fixture.control.join("next-email"), "second@example.com").unwrap();
        join(&fixture.paths, "work", "second", None, false, false).unwrap();

        let error = account::remove(&fixture.paths, "second", true, false, false).unwrap_err();
        assert!(error.to_string().contains("workspace"), "{error:#}");
        assert!(state::load(&fixture.paths)
            .unwrap()
            .profiles
            .contains_key("second"));
    }

    #[test]
    fn workspace_remove_requires_members_to_be_removed_first() {
        let fixture = keychain_fixture();
        fs::write(fixture.control.join("next-email"), "second@example.com").unwrap();
        join(&fixture.paths, "work", "second", None, false, false).unwrap();

        let error = remove(&fixture.paths, "work", false).unwrap_err();
        assert!(
            error.to_string().contains("still has member profiles"),
            "{error:#}"
        );

        account::remove(&fixture.paths, "second", false, false, false).unwrap();
        remove(&fixture.paths, "work", false).unwrap();

        let state = state::load(&fixture.paths).unwrap();
        assert!(state.workspaces.is_empty());
        assert!(state.profiles["founder"].workspace.is_none());
        assert!(fixture.external.is_dir(), "external directory is preserved");
        // With no workspace left, the state file drops back to version 1.
        let raw: Value =
            serde_json::from_slice(&fs::read(&fixture.paths.state_file).unwrap()).unwrap();
        assert_eq!(raw["version"], 1);
    }

    #[test]
    fn adopt_refuses_directories_registered_as_workspaces() {
        let fixture = keychain_fixture();
        let error = account::adopt(&fixture.paths, "duplicate", &fixture.external).unwrap_err();
        assert!(
            error.to_string().contains("already registered"),
            "{error:#}"
        );
    }
}
