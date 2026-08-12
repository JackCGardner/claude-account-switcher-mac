# claude-account

[![CI](https://github.com/JackCGardner/claude-account-switcher-mac/actions/workflows/ci.yml/badge.svg)](https://github.com/JackCGardner/claude-account-switcher-mac/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/JackCGardner/claude-account-switcher-mac)](https://github.com/JackCGardner/claude-account-switcher-mac/releases)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

A profile switcher for Claude Code on Linux and macOS. It gives Claude Code an
isolated `CLAUDE_CONFIG_DIR` for each account and transparently forwards normal
commands to the official Claude executable.

```bash
claude account add work
claude account add personal
claude account adopt movo ~/.claude-work
claude account workspace create work --from-profile movo
claude account workspace join work movo-2
claude account use work
claude account run personal          # parallel session, no switching
claude account map ~/Dev/movo work   # bare `claude` in there uses work
claude account usage
claude account watch work            # auto-rotate before hitting limits
claude account dashboard
claude account list
claude account current
claude account remove personal

claude
claude "fix this bug in main.py"
```

Claude Code itself performs login, logout, credential storage, and token
refresh. `claude-account` never writes, copies, or moves credential contents;
by default it never reads them either. The only exception is the opt-in
`--live` flag on `usage`/`watch`/`dashboard`, which reads each login's token
from the keychain — in memory, read-only — to query Anthropic's usage API.

> [!IMPORTANT]
> This is an independent community project, forked from
> [hamzarehmandeveloper/claude-account][upstream]. It is not made, endorsed, or
> supported by Anthropic. Claude and Claude Code are products of Anthropic.

## Requirements

- Linux or macOS
- A working Claude Code installation. On macOS this must be a recent build
  (verified with 2.1.220 and 2.1.228): older builds kept a single shared
  keychain item for every configuration directory, which makes per-profile
  isolation impossible. `claude account add` checks for this and refuses to
  continue on affected builds — see
  [How credentials are isolated](#how-credentials-are-isolated).
- Rust 1.85 or later to build from source

## Install a release

Download the archive for your platform and its `.sha256` file from the
[latest release][releases]:

- `claude-account-v0.3.0-x86_64-unknown-linux-gnu.tar.gz` — Linux
- `claude-account-v0.3.0-aarch64-apple-darwin.tar.gz` — macOS on Apple Silicon
- `claude-account-v0.3.0-x86_64-apple-darwin.tar.gz` — macOS on Intel

Then verify and install it:

```bash
# Linux
sha256sum --check claude-account-v0.3.0-x86_64-unknown-linux-gnu.tar.gz.sha256
tar -xzf claude-account-v0.3.0-x86_64-unknown-linux-gnu.tar.gz

# macOS (Apple Silicon)
shasum -a 256 --check claude-account-v0.3.0-aarch64-apple-darwin.tar.gz.sha256
tar -xzf claude-account-v0.3.0-aarch64-apple-darwin.tar.gz

./claude-account install
```

> [!NOTE]
> On macOS, archives downloaded with a browser carry the quarantine attribute
> and Gatekeeper will block the binary. Either download with `curl -LO` or
> clear it with `xattr -d com.apple.quarantine ./claude-account`.

If the real Claude executable cannot be found automatically on `PATH`, point
the installer at it explicitly:

```bash
./claude-account install --real /absolute/path/to/claude
```

The installer prints one line to put the shim first on `PATH`, matched to your
shell (`~/.bashrc` for bash, `~/.zshrc` for zsh — the macOS default — or a
`fish_add_path` command for fish). Apply it and open a new terminal. The shim
lives in its own directory; it does not replace the official Claude executable.

Confirm the installation:

```bash
type -a claude
claude account list
```

The claude-account shim should appear before the official Claude executable.

## Build from source

```bash
git clone https://github.com/JackCGardner/claude-account-switcher-mac.git
cd claude-account-switcher-mac
cargo build --locked --release
./target/release/claude-account install
```

## Commands

### Add an account

```bash
claude account add work
claude account add personal --email you@example.com
claude account add company --sso
claude account add api-billing --console
```

This opens Claude Code's official login flow. The first profile becomes active.
Adding another profile does not switch the active profile. The command also
completes Claude Code's local onboarding state, so the next `claude` launch
uses the saved login without asking you to authenticate again.

Before and after the login, `add` verifies that a brand-new configuration
directory does not see any existing login. If it does, your Claude Code build
stores credentials in one shared location instead of isolating them per
profile, and the command aborts with instructions rather than risk overwriting
another account's login.

### Adopt an existing configuration directory

If you already switch accounts by hand with `CLAUDE_CONFIG_DIR`, register those
directories as profiles without copying or modifying anything:

```bash
claude account adopt movo ~/.claude-work
claude account adopt personal ~/.claude
```

The directory is used in place: its settings, sessions, and login stay exactly
where they are, and the profile keeps using the same stored credentials that
your manual `CLAUDE_CONFIG_DIR=...` invocations used.

Adopting `~/.claude` (Claude's default directory) is special: the profile runs
Claude with `CLAUDE_CONFIG_DIR` *unset*, because Claude Code derives its
credential-storage key from the variable and an explicit `~/.claude` would
select different credentials than a plain `claude` command.

### Share one directory between several subscriptions (workspaces)

If you have two subscriptions but want them to feel like one account — same
sessions, transcripts, memories, settings, and history, with only the login
differing — put them in a shared workspace:

```bash
claude account adopt movo ~/.claude-work           # existing dir, first login
claude account workspace create work --from-profile movo
claude account workspace join work movo-2          # log in the 2nd subscription
claude account use movo-2                          # switch subscription, keep everything
claude account workspace list
```

`workspace create --from-profile` uses the profile's directory as the shared
storage, in place and without copying; that profile keeps its login and
becomes the first member. `workspace join` adds a member: it creates a
private symlink to the workspace directory, runs Claude Code's official login
through it, and registers the member as a normal profile. Because recent
Claude Code builds derive their credential-storage key from the literal
`CLAUDE_CONFIG_DIR` string, each member link selects its own keychain login
while every file Claude reads or writes lands in the one shared directory.
Switching members with `claude account use` therefore changes nothing except
which subscription pays for the tokens; resumed sessions carry on seamlessly.

`account use` also keeps the account identity that Claude Code records in the
shared `.claude.json` (`oauthAccount`/`userID`) in step with the active
member, so `/status` and friends show the account whose login is actually in
use. Running sessions started under the other member keep working; they were
started with their own credentials.

Like `add`, joining is protected by empirical probes: it aborts when a
brand-new link to the workspace can already see the members' logins. That is
always the case on Linux, where credentials live in a plaintext file inside
the (shared) directory — workspaces are effectively a macOS feature until
Linux builds gain per-directory external credential storage. Claude's default
`~/.claude` directory cannot become a workspace either, because Claude Code
keeps its top-level state at `~/.claude.json` when run without
`CLAUDE_CONFIG_DIR`, which members would not share.

`account remove MEMBER` logs out only that member's login and deletes its
link; the shared directory is never touched. `account workspace remove NAME`
unregisters the workspace once its linked members are gone and detaches the
founding profile back into a standalone one.

### Switch accounts

```bash
claude account use movo-2     # a workspace member: select it, target its workspace
claude account use work       # a workspace: target it (keeps its selection)
claude account use personal   # a standalone profile
```

There is no single global "active profile"; there is a **default target** for
bare `claude`, and each workspace tracks its own **selected member**. Using a
member selects it inside its workspace and makes the workspace the default
target, so later rotations (manual or from `watch`) apply without another
`use`. Switching affects newly launched Claude processes; existing sessions
keep the account they started with. `current` prints the profile a bare
`claude` would resolve to right now.

### Run a profile once, without switching anything

```bash
claude account run personal
claude account run work -- --model opus "review this diff"
```

`run` launches Claude as the named profile or workspace (through its selected
member) in this terminal only — ideal for a personal session alongside your
work workspace. The `CLAUDE_ACCOUNT_PROFILE` environment variable does the
same for any launch, and the wrapper re-exports it holding the resolved
profile so nested `claude` invocations stay on the login their session
started with.

### Bind directories to targets

```bash
claude account map ~/Development/movo work
claude account map ~/Personal personal
claude account map        # list bindings
claude account unmap ~/Personal
```

Bare `claude` inside a bound directory (or any subdirectory — the deepest
binding wins) targets the bound profile or workspace automatically.
Precedence: `CLAUDE_ACCOUNT_PROFILE`, then bindings, then the default target.

### Usage, the dashboard, and automatic rotation

```bash
claude account usage             # per-login 5h / 7d / per-model weekly windows
claude account dashboard         # full-screen, auto-refreshing comparison
claude account watch work        # rotate the workspace before hitting a limit
claude account watch work --threshold 85 --models fable --strategy best --once
```

Usage data is cache-first: Claude Code stores its last usage snapshot inside
the profile's `.claude.json`, tagged with the account it belongs to; sessions
refresh it while they run, an idle login's windows only decay, and the
watcher remembers each member's freshest numbers. `--live` (opt-in, macOS)
instead queries Anthropic's usage API with each login's keychain token —
read-only, and the only place this tool ever reads a credential.

`watch` checks the workspace's gating windows — the 5-hour and 7-day windows
always, plus per-model weekly windows per `--models` (default `all`) — and
when any reaches `--threshold` (default 90%) it rotates the workspace's
selected member and sends a desktop notification. `--strategy` picks the
replacement: `consume-first` (default — soonest weekly reset, so no quota is
wasted), `best` (most headroom), or `next-available`. Anti-flap hysteresis
requires candidates to sit 10 points below the threshold, and rotations are
at least 10 minutes apart unless the selected member is hard-limited.
Settings persist per workspace, so a bare `claude account watch work` reuses
them. Running sessions are never touched: the rotated-away session keeps
working until you relaunch it — `claude --resume` / `claude -c` continues the
same transcript on the fresh subscription.

### Inspect profiles

```bash
claude account list
claude account list --status
claude account current
```

`list` marks the active profile with `*` and shows the directory of adopted
profiles. `--status` additionally asks Claude Code for each profile's login
state, email, and plan (it launches Claude once per profile, so it takes a
moment). `current` prints only the profile name, making it safe to use in
scripts.

### Remove an account

```bash
claude account remove personal
```

For profiles created with `add`, this runs Claude Code's official
`auth logout` inside the profile and unregisters it. Settings and session
history are preserved, allowing the same profile name to reuse them later.
Pass `--keep-login` to unregister without logging out.

Adopted profiles are never logged out: `remove` only unregisters them and
leaves the directory untouched, since the directory predates claude-account
and may still be used outside it. The command prints the manual
`claude auth logout` invocation if you do want to log it out.

To delete all local data belonging to a profile created with `add`:

```bash
claude account remove personal --purge --yes
```

Removing the active profile is refused unless `--force` is supplied.
`--purge` permanently deletes that profile's settings, sessions, plugins, and
history in addition to its stored login. Purging is refused for adopted
directories.

### Get help

```bash
claude account --help
claude account add --help
claude account remove --help
```

All non-account commands and flags are passed unchanged to the official Claude
executable:

```bash
claude
claude -p "explain this project"
claude --model opus
claude auth status --text
```

## How credentials are isolated

Each profile gets its own `CLAUDE_CONFIG_DIR`, and Claude Code scopes its
credential storage to that directory:

- **Linux:** credentials live in `.credentials.json` inside the configuration
  directory, so separate directories are fully separate logins.
- **macOS:** credentials live in the login keychain. Recent Claude Code builds
  derive the keychain service name from the configuration directory —
  `Claude Code-credentials-<first 8 hex chars of sha256(path)>`, or plain
  `Claude Code-credentials` when `CLAUDE_CONFIG_DIR` is unset — so each
  profile gets its own keychain item. You can inspect a profile's item with:

  ```bash
  security find-generic-password \
    -s "Claude Code-credentials-$(printf %s "$HOME/.claude-work" | shasum -a 256 | cut -c1-8)"
  ```

Older macOS builds of Claude Code used one shared `Claude Code-credentials`
item for every configuration directory. On such builds, profile switching
cannot work — a login in one profile silently replaces the others.
`claude account add` detects this by probing whether a brand-new directory
already sees a login, and aborts before and after the login step if isolation
is broken.

claude-account itself never reads, writes, or copies credential contents on
either platform; it only selects which storage Claude Code uses.

## Storage

By default:

```text
~/.config/claude-account/state.json
~/.local/share/claude-account/profiles/<name>/
~/.local/share/claude-account/workspaces/<name>/
~/.local/share/claude-account/bin/claude
~/.local/share/claude-account/libexec/claude-account
```

The same layout is used on Linux and macOS, and the standard
`XDG_CONFIG_HOME` and `XDG_DATA_HOME` variables are respected on both.
`CLAUDE_ACCOUNT_HOME` can place all application data under one absolute
directory, which is especially useful for tests. Workspace members appear
under `profiles/<name>` as symlinks to their workspace directory; the member's
login is keyed to the symlink's own path, so moving or renaming the link
orphans that login.

The state file contains profile names, directory paths, directory bindings,
the real Claude executable path, per-workspace watch settings, and — for
workspace members — the account-identity metadata Claude Code shows for the
login (email and account ids) plus the last usage percentages the watcher
observed. It never contains access or refresh tokens.

## Authentication environment variables

`CLAUDE_ACCOUNT_PROFILE` pins a launch to a profile or workspace name,
overriding directory bindings and the default target; the wrapper re-exports
it to the child holding the resolved profile name so nested `claude`
invocations stay on their session's login.

To guarantee that the selected profile is actually used, the wrapper removes
these variables from the child Claude process:

- `ANTHROPIC_API_KEY`
- `ANTHROPIC_AUTH_TOKEN`
- `CLAUDE_CODE_OAUTH_TOKEN`

Any of these would silently override the profile's stored login — and a
leftover `CLAUDE_CODE_OAUTH_TOKEN` can even cause Claude Code to delete the
stored credential on exit. Set `CLAUDE_ACCOUNT_PRESERVE_AUTH_ENV=1` if you
intentionally want those variables to override profile authentication.

The wrapper also always removes `CLAUDE_SECURESTORAGE_CONFIG_DIR`, an
undocumented Claude Code variable that re-keys credential storage away from
the selected profile directory.

## Development

```bash
cargo fmt --check
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
```

The same checks run in CI on Ubuntu and macOS. See
[CONTRIBUTING.md](CONTRIBUTING.md) for the contribution workflow and
[SECURITY.md](SECURITY.md) for private vulnerability reporting.

## License

Released under the [MIT License](LICENSE).

[releases]: https://github.com/JackCGardner/claude-account-switcher-mac/releases
[upstream]: https://github.com/hamzarehmandeveloper/claude-account
