# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/JackCGardner/claude-account-switcher-mac/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/JackCGardner/claude-account-switcher-mac/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/hamzarehmandeveloper/claude-account/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/hamzarehmandeveloper/claude-account/releases/tag/v0.1.0
