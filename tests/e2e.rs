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
