# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.0] - 2026-08-11

### Added

- Shared workspaces: one configuration directory used by several member
  profiles, so sessions, transcripts, memories, settings, and history are
  common while each member keeps its own login. `account workspace create
  NAME --from-profile PROFILE` turns an existing profile's directory into the
  workspace storage in place; `account workspace join NAME MEMBER` links a new
  member to it and opens Claude's login flow for another subscription.
  Switching subscriptions becomes `claude account use MEMBER` — no more
  copying configuration directories around. Members run Claude through their
  own symlink to the directory: Claude Code keys credential storage off the
  literal `CLAUDE_CONFIG_DIR` string (verified on 2.1.228), so each link is a
  separate login while all file I/O lands in the shared directory.
- `account use` keeps the account identity (`oauthAccount`/`userID`) in the
  shared `.claude.json` in step with the member being activated, so Claude
  displays the account that matches the login actually in use.
- `account workspace list` shows each workspace's directory, members, and
  member emails; `account list` tags member profiles with their workspace.
- Empirical workspace guardrails: `join` aborts when a brand-new link to the
  workspace can already see existing members' logins — which catches Linux
  (plaintext credentials live inside the shared directory) and any build that
  canonicalizes the path before keying credential storage — and re-checks
  after the login landed.
- The state file is saved as version 2 once workspaces are used, so older
  claude-account binaries refuse it instead of silently dropping workspace
  fields; without workspaces it stays at version 1.
- A Claude Code skill (`skills/claude-account/SKILL.md`) that teaches Claude
  how to drive this CLI.
- `account run NAME [ARGS...]` launches Claude once as a profile or workspace
  without changing the default target, and the `CLAUDE_ACCOUNT_PROFILE`
  environment variable pins any launch the same way (re-exported holding the
  resolved profile so nested invocations stay on their session's login).
- `account map DIRECTORY TARGET` binds a directory tree to a profile or
  workspace: bare `claude` inside it targets that automatically (deepest
  binding wins); `map` lists bindings, `unmap` removes one, and removals
  clean up bindings that pointed at the removed name.
- `account usage [--live]` shows each login's 5-hour, 7-day, and per-model
  weekly windows with reset countdowns, grouped by workspace. Data is
  cache-first from Claude's own `cachedUsageUtilization` snapshots (account-
  UUID-tagged, with passed resets decayed to zero); `--live` opts into
  querying Anthropic's usage API with each login's keychain token —
  read-only, and the only credential read in the tool.
- `account watch WORKSPACE` rotates the workspace's selected member before a
  usage window hits its limit: gating windows are OR'd (5h, 7d, per-model
  weeklies via `--models`, default `all`), `--threshold` defaults to 90%, and
  `--strategy` picks the replacement (`consume-first` by default — the member
  whose weekly window resets soonest — or `best` / `next-available`), with
  hysteresis, a rotation cooldown that a hard 100% limit overrides, adaptive
  polling, persisted per-workspace settings, `--once`, and a desktop
  notification on rotation. Decision logic ported from claude-swap (MIT).
  The watcher records each member's freshest observed windows so idle
  members keep known, decaying numbers in cached mode.
- `account dashboard` renders a full-screen auto-refreshing comparison of
  every login — workspaces grouped with members and watch status, standalone
  profiles separate, usage bars per window — with no TUI dependency.

### Changed

- The single global active profile is gone: each workspace tracks its own
  **selected member** (what `use MEMBER` and `watch` rotate), and the
  `active` name is now a default launch target that can be a workspace —
  resolving through its selection — or a standalone profile. Profile and
  workspace names share one namespace, `current` prints the resolved
  profile, and `list` marks the resolved default.
- `account remove` on a workspace member logs out only that member's login,
  deletes its member link, and never touches the shared directory.
  `--keep-login` keeps the link so a later `workspace join` with the same
  name reuses the login without a new browser flow. Purging is refused for
  workspace members, and `workspace remove --purge` only ever deletes
  directories that claude-account created itself. Removing the selected
  member hands the selection to the only remaining member.
- Writes to a profile's `.claude.json` (onboarding, identity swaps) now take
  Claude Code's own `.claude.json.lock` mkdir-mutex, so they cannot interleave
  with a running session's config writes; stale locks are taken over and a
  live holder only delays the (still atomic) write briefly.

### Fixed

Findings from a four-way adversarial validation pass (code review, sandboxed
functional testing, security review, docs audit):

- `add` and `workspace join` re-check the shared profile/workspace namespace
  after the interactive login, so a workspace created during the login window
  can no longer silently shadow the just-logged-in profile.
- The watch candidate ceiling is clamped (`max(threshold − 10, threshold/2)`),
  so thresholds below the hysteresis margin can still rotate to a genuinely
  idle member instead of reporting all members saturated forever.
- `remove`'s post-logout identity restore now runs under the state lock, so a
  concurrent `use`/watch rotation can no longer be overwritten with a stale
  identity.
- Removing the last member of the default-target workspace requires
  `--force` (mirroring the standalone active-profile gate) and leaving a
  workspace memberless prints how to re-join or remove it.
- `current` resolves exactly like a bare `claude` launch — environment
  override and directory bindings included — instead of only the default
  target.
- The RFC 3339 parser rejects calendar-invalid dates (Feb 30, Apr 31) instead
  of rolling them forward; `--interval` is bounded to 15–86400 seconds.
- Hardening: tokens containing curl-config-breaking characters are refused;
  stale token-bearing curl config files from killed fetches are swept and a
  drop guard removes them on panic; the state lock refuses symlinks
  (O_NOFOLLOW); managed directories are created with 0700 atomically at mkdir
  time; the `.claude.json.lock` path is built without lossy UTF-8 conversion.

## [0.2.0] - 2026-07-31

### Added

- Native macOS support alongside Linux: CI runs on Ubuntu and macOS, and
  releases ship `aarch64-apple-darwin` and `x86_64-apple-darwin` archives.
- Credential-isolation guardrails in `account add`: before and after the login
  the tool probes whether a brand-new configuration directory sees an existing
  login, and aborts on Claude Code builds that share credential storage across
  profiles (older macOS builds with a single keychain item).
- `account adopt NAME DIRECTORY` registers an existing `CLAUDE_CONFIG_DIR` as
  a profile in place, without copying or modifying it. Adopting Claude's
  default `~/.claude` directory runs Claude with `CLAUDE_CONFIG_DIR` unset so
  it keeps using the same credentials as a plain `claude` command.
- `account list --status` shows each profile's login state, email, and plan.
- `account remove --keep-login` unregisters a profile without logging it out.
- The installer's PATH instruction now matches the user's shell
  (bash, zsh, or fish).

### Changed

- `account remove` never logs out adopted profiles; it unregisters them,
  leaves the directory untouched, and prints the manual logout command.
  Purging is refused for adopted directories.
- Managed Claude processes always run with `CLAUDE_SECURESTORAGE_CONFIG_DIR`
  removed so an inherited storage override cannot re-key credentials away
  from the selected profile.

## [0.1.1] - 2026-07-30

### Fixed

- Complete Claude Code onboarding after a verified `account add` login so the
  first normal `claude` launch does not ask the user to authenticate again.
- Require `auth status --json` to explicitly report `loggedIn: true` before a
  profile is registered.

## [0.1.0] - 2026-07-30

### Added

- Linux-only Claude Code profile isolation through `CLAUDE_CONFIG_DIR`.
- `add`, `use`, `list`, `current`, and `remove` account commands.
- Transparent forwarding of normal Claude Code commands and arguments.
- Official Claude Code login, status verification, and logout integration.
- Atomic state writes, process locking, strict filesystem permissions, and
  profile-name validation.
- Safe profile removal with separate unregister and permanent purge modes.
- Non-invasive shim installation that preserves the official Claude launcher.
- Unit and end-to-end lifecycle tests.

[Unreleased]: https://github.com/JackCGardner/claude-account-switcher-mac/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/JackCGardner/claude-account-switcher-mac/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/JackCGardner/claude-account-switcher-mac/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/hamzarehmandeveloper/claude-account/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/hamzarehmandeveloper/claude-account/releases/tag/v0.1.0
