# Security policy

## Supported versions

Security fixes are provided for the latest released version.

## Reporting a vulnerability

Please use GitHub's private **Report a vulnerability** feature on the
repository's Security tab. Do not open a public issue for a suspected
credential exposure, path traversal, arbitrary command execution, or unsafe
deletion vulnerability.

Include:

- The affected version and operating system (Linux distribution or macOS version)
- Reproduction steps using placeholder credentials
- The expected and observed behavior
- Any proposed fix, if available

Never include real Claude credentials, access tokens, refresh tokens, API keys,
or the contents of `.credentials.json`.

## Security model

claude-account does not parse or copy Claude credentials. It creates an
isolated `CLAUDE_CONFIG_DIR` and delegates authentication to the official
Claude Code executable. Local profile directories are owner-only (`0700`) and
the registry is owner-readable and owner-writable (`0600`).

The program runs with the invoking user's permissions. Anyone who can modify
that user's configuration or executable search path is already within the same
local trust boundary.
