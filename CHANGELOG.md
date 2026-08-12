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

### Changed

- `account remove` on a workspace member logs out only that member's login,
  deletes its member link, and never touches the shared directory.
  `--keep-login` keeps the link so a later `workspace join` with the same
  name reuses the login without a new browser flow. Purging is refused for
  workspace members, and `workspace remove --purge` only ever deletes
  directories that claude-account created itself.

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
