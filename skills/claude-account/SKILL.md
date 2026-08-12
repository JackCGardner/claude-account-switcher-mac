---
name: claude-account
description: Drive the claude-account CLI, the Claude Code profile switcher for macOS/Linux. Use when the user wants to switch Claude Code accounts or subscriptions, add/adopt/list/remove profiles, set up or manage a shared workspace (one directory, several subscription logins), import existing CLAUDE_CONFIG_DIR folders, or troubleshoot the claude shim.
---

# claude-account — Claude Code profile switcher

`claude-account` installs a transparent `claude` shim. `claude account …`
manages profiles; every other `claude` invocation is forwarded to the real
Claude Code with `CLAUDE_CONFIG_DIR` pointed at the **active profile's**
directory (auth env vars like `ANTHROPIC_API_KEY` are stripped so the
profile's stored login always wins). It never reads, writes, or copies
credentials — Claude Code itself performs login/logout/refresh, and recent
builds key their credential storage (macOS keychain item) off the literal
`CLAUDE_CONFIG_DIR` string, so each directory is an isolated login.

## Mental model

- **Profile** — a name → one configuration directory → one login. Sessions,
  transcripts, auto-memory, settings, and history all live in that directory.
- **Adopted profile** — a pre-existing directory registered in place (never
  copied, never logged out or deleted by this tool).
- **Workspace** — one shared directory used by several **member profiles**.
  Everything is common except the login: each member is a private symlink to
  the directory, and because credentials are keyed by the literal path, each
  member holds its own subscription login. Switching members = switching
  which subscription pays, with identical sessions/memories. macOS only (on
  Linux credentials live inside the shared directory, so `join` refuses).
- Special case: a profile for `~/.claude` runs Claude with
  `CLAUDE_CONFIG_DIR` unset (an explicit `~/.claude` would select different
  credentials than plain `claude`). `~/.claude` can never back a workspace.

## Commands

```bash
# one-time setup (from a release download or repo build)
./claude-account install [--real /abs/path/to/claude]   # then add printed PATH line

claude account add NAME [--email E] [--sso] [--console] # new dir + official login
claude account adopt NAME DIR                           # register existing dir in place
claude account use NAME                                 # affects new claude processes only
claude account list [--status]                          # * = active; --status shows email/plan
claude account current                                  # active profile name (script-safe)
claude account remove NAME [--keep-login] [--force] [--purge --yes]

claude account workspace create WS --from-profile PROFILE  # profile's dir becomes shared storage
claude account workspace create WS                         # or fresh empty storage
claude account workspace join WS MEMBER [--email E]        # add 2nd subscription (browser login)
claude account workspace list                              # workspaces, members, emails
claude account workspace remove WS [--purge --yes]         # members must be removed first
```

## Common flows

**Import existing hand-rolled CLAUDE_CONFIG_DIR folders**

```bash
claude account adopt personal ~/.claude
claude account adopt work ~/.claude-work-4
claude account list --status     # verify each profile's email and plan
```

**Two subscriptions, one shared work environment** (the workspace flow)

```bash
claude account adopt work-a ~/.claude-work-4              # dir with latest sessions
claude account workspace create work --from-profile work-a
claude account workspace join work work-b                 # sign in the OTHER subscription
# daily use — hit a usage cap? switch and continue in the same sessions:
claude account use work-b
```

**Check what's what**: `claude account list --status` launches Claude once
per profile, so it takes a few seconds; `claude account workspace list` is
instant (uses recorded identities).

## Rules for the assistant

- Logins are interactive browser flows: run `add`/`join` in a way the user
  can complete, and tell them which account to sign in with. Never try to
  script or copy credentials, keychain items, or tokens.
- Never edit `~/.config/claude-account/state.json`, member symlinks under
  `…/claude-account/profiles/`, or a profile's `.claude.json` by hand; use
  the CLI. Moving/renaming a member symlink orphans that member's login.
- `use` only affects newly launched Claude processes; running sessions keep
  the account they started with.
- Removing an adopted profile or workspace member never deletes shared data:
  `remove` on a member logs out only that member and deletes its link
  (`--keep-login` keeps both so a later `join` with the same name reuses the
  login without a browser flow).
- `--purge` is only for directories claude-account created itself; it is
  refused for adopted dirs and workspace members by design — don't work
  around that with `rm`.

## Troubleshooting

- **"a brand-new configuration directory already sees an existing login"** —
  the Claude Code build shares credential storage (old macOS builds).
  `claude update`, then retry.
- **"Claude Code resolves the member link before choosing credential
  storage"** — this platform/build cannot give workspace members separate
  logins (always true on Linux). Use plain isolated profiles instead.
- **"unsupported state version 2"** from an older claude-account binary —
  workspaces are in use; upgrade the binary (`cargo build --locked --release
  && ./target/release/claude-account install`).
- **`claude account` says no active profile / not configured** — run
  `claude account use NAME`, or `claude-account install` if the real Claude
  path was never recorded.
- **Shim not first on PATH** (`type -a claude` shows the official binary
  first) — re-add the PATH line `install` printed, open a new terminal.
- **Wrong email shown inside Claude after switching members** — run
  `claude account use MEMBER` again (it rewrites the shared identity), or
  `claude auth login` once in that member if it was joined with a reused
  login.
