use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

fn run_with_env(
    program: &Path,
    account_home: &Path,
    arguments: &[&str],
    extra_env: &[(&str, &str)],
) -> Output {
    let mut command = Command::new(program);
    command
        .env("CLAUDE_ACCOUNT_HOME", account_home)
        .env("ANTHROPIC_API_KEY", "must-not-leak")
        .env("CLAUDE_SECURESTORAGE_CONFIG_DIR", "must-not-leak-either")
        .args(arguments);
    for (key, value) in extra_env {
        command.env(key, value);
    }
    let output = command.output().unwrap();

    assert!(
        output.status.success(),
        "command failed: {}\nstdout:\n{}\nstderr:\n{}",
        arguments.join(" "),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn run(program: &Path, account_home: &Path, arguments: &[&str]) -> Output {
    run_with_env(program, account_home, arguments, &[])
}

/// A fake Claude executable that mirrors how real builds isolate credentials:
/// login state lives inside the effective configuration directory
/// (CLAUDE_CONFIG_DIR, or ~/.claude when the variable is unset).
fn write_fake_claude(fake_claude: &Path, calls_log: &Path) {
    fs::write(
        fake_claude,
        format!(
            "#!/bin/sh\n\
             dir=\"${{CLAUDE_CONFIG_DIR:-$HOME/.claude}}\"\n\
             printf '%s|%s%s|%s\\n' \"$CLAUDE_CONFIG_DIR\" \"$ANTHROPIC_API_KEY\" \"$CLAUDE_SECURESTORAGE_CONFIG_DIR\" \"$*\" >> '{log}'\n\
             if [ \"$1 $2\" = \"auth login\" ]; then\n\
               touch \"$dir/.fake-credentials\"\n\
               exit 0\n\
             fi\n\
             if [ \"$1 $2 $3\" = \"auth status --json\" ]; then\n\
               if [ -f \"$dir/.fake-credentials\" ]; then\n\
                 printf '{{\"loggedIn\":true}}\\n'\n\
                 exit 0\n\
               fi\n\
               printf '{{\"loggedIn\":false}}\\n'\n\
               exit 1\n\
             fi\n\
             if [ \"$1\" = \"auth\" ]; then exit 0; fi\n\
             printf 'forwarded:%s|config:%s\\n' \"$*\" \"$CLAUDE_CONFIG_DIR\"\n",
            log = calls_log.display()
        ),
    )
    .unwrap();
    fs::set_permissions(fake_claude, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn complete_profile_lifecycle() {
    let temp = tempfile::tempdir().unwrap();
    let account_home = temp.path().join("account-home");
    let fake_claude = temp.path().join("real-claude");
    let calls = temp.path().join("calls.log");
    let binary = Path::new(env!("CARGO_BIN_EXE_claude-account"));
    write_fake_claude(&fake_claude, &calls);

    run(
        binary,
        &account_home,
        &["install", "--real", fake_claude.to_str().unwrap()],
    );
    let shim = account_home.join("bin/claude");
    assert!(shim.is_symlink());

    let first_add = run(&shim, &account_home, &["account", "add", "work"]);
    assert!(String::from_utf8_lossy(&first_add.stdout).contains("made it active"));
    let work_config: serde_json::Value =
        serde_json::from_slice(&fs::read(account_home.join("profiles/work/.claude.json")).unwrap())
            .unwrap();
    assert_eq!(work_config["hasCompletedOnboarding"], true);

    run(&shim, &account_home, &["account", "add", "personal"]);

    let profiles = run(&shim, &account_home, &["account", "list"]);
    let profiles = String::from_utf8(profiles.stdout).unwrap();
    assert!(profiles.contains("* work"));
    assert!(profiles.contains("  personal"));

    let current = run(&shim, &account_home, &["account", "current"]);
    assert_eq!(String::from_utf8(current.stdout).unwrap().trim(), "work");

    run(&shim, &account_home, &["account", "use", "personal"]);
    let current = run(&shim, &account_home, &["account", "current"]);
    assert_eq!(
        String::from_utf8(current.stdout).unwrap().trim(),
        "personal"
    );

    let forwarded = run(&shim, &account_home, &["fix this bug", "--model", "sonnet"]);
    let forwarded = String::from_utf8(forwarded.stdout).unwrap();
    assert!(forwarded.contains("forwarded:fix this bug --model sonnet"));
    assert!(forwarded.contains(account_home.join("profiles/personal").to_str().unwrap()));

    // `run` launches a specific profile once without changing the default
    // target, and CLAUDE_ACCOUNT_PROFILE overrides resolution per launch.
    let ran = run(
        &shim,
        &account_home,
        &["account", "run", "work", "ping from run"],
    );
    let ran = String::from_utf8(ran.stdout).unwrap();
    assert!(ran.contains("forwarded:ping from run"));
    assert!(ran.contains(account_home.join("profiles/work").to_str().unwrap()));
    let overridden = run_with_env(
        &shim,
        &account_home,
        &["ping via env"],
        &[("CLAUDE_ACCOUNT_PROFILE", "work")],
    );
    let overridden = String::from_utf8(overridden.stdout).unwrap();
    assert!(overridden.contains(account_home.join("profiles/work").to_str().unwrap()));
    let still_personal = run(&shim, &account_home, &["account", "current"]);
    assert_eq!(
        String::from_utf8(still_personal.stdout).unwrap().trim(),
        "personal"
    );

    run(&shim, &account_home, &["account", "remove", "work"]);
    assert!(account_home.join("profiles/work").is_dir());

    run(
        &shim,
        &account_home,
        &[
            "account", "remove", "personal", "--force", "--purge", "--yes",
        ],
    );
    assert!(!account_home.join("profiles/personal").exists());

    // Adopt a pre-existing configuration directory in place.
    let external = temp.path().join("external-claude");
    fs::create_dir_all(&external).unwrap();
    fs::write(external.join(".fake-credentials"), b"live").unwrap();
    let adopted = run(
        &shim,
        &account_home,
        &["account", "adopt", "movo", external.to_str().unwrap()],
    );
    assert!(String::from_utf8_lossy(&adopted.stdout).contains("Adopted `movo`"));

    let listed = run(&shim, &account_home, &["account", "list"]);
    let listed = String::from_utf8(listed.stdout).unwrap();
    assert!(listed.contains("* movo"));
    assert!(listed.contains(external.to_str().unwrap()));

    let forwarded_adopted = run(&shim, &account_home, &["hello from movo"]);
    let forwarded_adopted = String::from_utf8(forwarded_adopted.stdout).unwrap();
    assert!(forwarded_adopted.contains(&format!("config:{}", external.display())));

    // Removing an adopted profile unregisters it but never logs it out.
    run(
        &shim,
        &account_home,
        &["account", "remove", "movo", "--force"],
    );
    assert!(external.join(".fake-credentials").exists());

    let logged_calls = fs::read_to_string(calls).unwrap();
    assert!(logged_calls.contains("profiles/work||auth login"));
    assert!(logged_calls.contains("profiles/personal||auth login"));
    assert!(logged_calls.contains("profiles/work||auth logout"));
    assert!(logged_calls.contains("profiles/personal||fix this bug --model sonnet"));
    assert!(
        !logged_calls.contains(&format!("{}||auth logout", external.display())),
        "adopted directory must never be logged out:\n{logged_calls}"
    );
    assert!(
        !logged_calls.contains("must-not-leak"),
        "auth environment variable leaked to Claude"
    );
    assert!(
        !logged_calls.contains("must-not-leak-either"),
        "storage override environment variable leaked to Claude"
    );
}

/// A fake Claude that keys credentials off the literal CLAUDE_CONFIG_DIR
/// string, the way recent macOS builds derive the keychain item, while file
/// I/O follows symlinks like real filesystem access.
fn write_keychain_fake_claude(fake_claude: &Path, control: &Path) {
    fs::create_dir_all(control).unwrap();
    fs::write(
        fake_claude,
        format!(
            "#!/bin/sh\n\
             dir=\"${{CLAUDE_CONFIG_DIR:-$HOME/.claude}}\"\n\
             key=$(printf %s \"$CLAUDE_CONFIG_DIR\" | cksum | cut -d' ' -f1)\n\
             cred='{control}/cred-'$key\n\
             if [ \"$1 $2\" = \"auth login\" ]; then\n\
               cat '{control}/next-email' > \"$cred\"\n\
               email=$(cat \"$cred\")\n\
               printf '{{\"oauthAccount\":{{\"emailAddress\":\"%s\"}}}}' \"$email\" > \"$dir/.claude.json\"\n\
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
             if [ \"$1\" = \"auth\" ]; then exit 0; fi\n\
             printf 'forwarded:%s|config:%s\\n' \"$*\" \"$CLAUDE_CONFIG_DIR\"\n",
            control = control.display()
        ),
    )
    .unwrap();
    fs::set_permissions(fake_claude, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn workspace_lifecycle() {
    let temp = tempfile::tempdir().unwrap();
    let account_home = temp.path().join("account-home");
    let control = temp.path().join("control");
    let fake_claude = temp.path().join("real-claude");
    let binary = Path::new(env!("CARGO_BIN_EXE_claude-account"));
    write_keychain_fake_claude(&fake_claude, &control);

    run(
        binary,
        &account_home,
        &["install", "--real", fake_claude.to_str().unwrap()],
    );
    let shim = account_home.join("bin/claude");

    // An existing, logged-in directory becomes the workspace storage.
    let external = temp.path().join("external-claude");
    fs::create_dir_all(&external).unwrap();
    fs::write(control.join("next-email"), "jack@work.example").unwrap();
    let login = Command::new(&fake_claude)
        .env("CLAUDE_CONFIG_DIR", &external)
        .args(["auth", "login"])
        .output()
        .unwrap();
    assert!(login.status.success());

    run(
        &shim,
        &account_home,
        &["account", "adopt", "jack", external.to_str().unwrap()],
    );
    run(
        &shim,
        &account_home,
        &[
            "account",
            "workspace",
            "create",
            "work",
            "--from-profile",
            "jack",
        ],
    );

    // A second subscription joins the same directory through a member link.
    fs::write(control.join("next-email"), "jackg@work.example").unwrap();
    let joined = run(
        &shim,
        &account_home,
        &["account", "workspace", "join", "work", "jackg"],
    );
    assert!(String::from_utf8_lossy(&joined.stdout).contains("Joined workspace"));

    let link = account_home.join("profiles/jackg");
    assert!(link.is_symlink());
    assert_eq!(fs::read_link(&link).unwrap(), external);
    let cred_count = || {
        fs::read_dir(&control)
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
    };
    assert_eq!(cred_count(), 2, "each member keeps its own login");

    // Switching members swaps the shared identity and runs Claude through the
    // member link, which selects the member's own credentials.
    run(&shim, &account_home, &["account", "use", "jackg"]);
    let claude_json: serde_json::Value =
        serde_json::from_slice(&fs::read(external.join(".claude.json")).unwrap()).unwrap();
    assert_eq!(
        claude_json["oauthAccount"]["emailAddress"],
        "jackg@work.example"
    );
    let forwarded = run(&shim, &account_home, &["hello from the workspace"]);
    let forwarded = String::from_utf8(forwarded.stdout).unwrap();
    assert!(
        forwarded.contains(&format!("config:{}", link.display())),
        "Claude must run through the member link:\n{forwarded}"
    );

    let listing = run(&shim, &account_home, &["account", "workspace", "list"]);
    let listing = String::from_utf8(listing.stdout).unwrap();
    assert!(listing.contains("work"));
    assert!(listing.contains("jack@work.example"));
    assert!(listing.contains("jackg@work.example"));

    // Removing a member logs out only that member; the workspace remains.
    run(&shim, &account_home, &["account", "use", "jack"]);
    let restored: serde_json::Value =
        serde_json::from_slice(&fs::read(external.join(".claude.json")).unwrap()).unwrap();
    assert_eq!(
        restored["oauthAccount"]["emailAddress"],
        "jack@work.example"
    );
    run(&shim, &account_home, &["account", "remove", "jackg"]);
    assert!(!link.exists());
    assert_eq!(cred_count(), 1, "the founder's login must survive");

    // Dissolving the workspace detaches the founding profile.
    run(
        &shim,
        &account_home,
        &["account", "workspace", "remove", "work"],
    );
    let profiles = run(&shim, &account_home, &["account", "list"]);
    let profiles = String::from_utf8(profiles.stdout).unwrap();
    assert!(profiles.contains("* jack"));
    assert!(!profiles.contains("[workspace"));
    assert!(external.join(".claude.json").exists());
}

#[test]
fn adopting_the_default_claude_directory_unsets_the_config_override() {
    let temp = tempfile::tempdir().unwrap();
    let account_home = temp.path().join("account-home");
    let home = temp.path().join("home");
    let default_dir = home.join(".claude");
    fs::create_dir_all(&default_dir).unwrap();
    fs::write(default_dir.join(".fake-credentials"), b"live").unwrap();
    let fake_claude = temp.path().join("real-claude");
    let calls = temp.path().join("calls.log");
    let binary = Path::new(env!("CARGO_BIN_EXE_claude-account"));
    write_fake_claude(&fake_claude, &calls);
    let home_env = [("HOME", home.to_str().unwrap())];

    run_with_env(
        binary,
        &account_home,
        &["install", "--real", fake_claude.to_str().unwrap()],
        &home_env,
    );
    let shim = account_home.join("bin/claude");
    run_with_env(
        &shim,
        &account_home,
        &[
            "account",
            "adopt",
            "personal",
            default_dir.to_str().unwrap(),
        ],
        &home_env,
    );

    let forwarded = run_with_env(&shim, &account_home, &["hi"], &home_env);
    let forwarded = String::from_utf8(forwarded.stdout).unwrap();
    assert!(
        forwarded.lines().any(|line| line == "forwarded:hi|config:"),
        "expected Claude to run without CLAUDE_CONFIG_DIR for the default directory:\n{forwarded}"
    );
}
