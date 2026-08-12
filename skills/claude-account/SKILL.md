---
name: claude-account
description: Drive the claude-account CLI, the Claude Code profile switcher for macOS/Linux. Use when the user wants to switch Claude Code accounts or subscriptions, add/adopt/list/remove profiles, set up or manage a shared workspace (one directory, several subscription logins), run parallel sessions on different accounts, map directories to accounts, check usage windows or the dashboard, set up automatic subscription rotation (watch), import existing CLAUDE_CONFIG_DIR folders, or troubleshoot the claude shim.
---

# claude-account — Claude Code profile switcher

`claude-account` installs a transparent `claude` shim. `claude account …`
manages profiles; every other `claude` invocation is forwarded to the real
Claude Code with `CLAUDE_CONFIG_DIR` pointed at the resolved profile's
directory (auth env vars like `ANTHROPIC_API_KEY` are stripped so the
profile's stored login always wins). It never writes or copies credentials —
Claude Code performs login/logout/refresh itself, and recent builds key
credential storage (the macOS keychain item) off the literal
`CLAUDE_CONFIG_DIR` string, so each directory is an isolated login.

## Mental model

- **Profile** — a name → one configuration directory → one login. Sessions,
  transcripts, auto-memory, settings, and history live in that directory.
- **Adopted profile** — a pre-existing directory registered in place (never
  copied; never logged out or deleted by this tool).
- **Workspace** — one shared directory used by several **member profiles**:
  everything is common except the login. Each member is a private symlink to
  the directory; credentials key off the literal path, so each member holds
  its own subscription. Each workspace has a **selected member** — what
  launches resolve to and what `watch` rotates. macOS only (on Linux
  credentials live inside the shared directory, so `join` refuses).
- **Default target** — what bare `claude` runs: a standalone profile, or a
  workspace (which resolves through its selected member). There is no global
  "active profile". Resolution order: `CLAUDE_ACCOUNT_PROFILE` env var →
  directory binding (`map`) → default target (`use`).
- Special case: a profile for `~/.claude` runs Claude with
  `CLAUDE_CONFIG_DIR` unset, and `~/.claude` can never back a workspace.

## Commands

```bash
./claude-account install [--real /abs/path/to/claude]   # then add printed PATH line

claude account add NAME [--email E] [--sso] [--console] # new dir + official login
claude account adopt NAME DIR                           # register existing dir in place
claude account use NAME              # member: select it in its workspace; workspace or
                                     # standalone profile: make it the default target
claude account run NAME [-- ARGS]    # one launch as NAME, nothing switched
claude account map DIR TARGET        # bare `claude` under DIR targets TARGET
claude account map                   # list bindings   (unmap DIR removes one)
claude account list [--status]       # * = what bare `claude` resolves to now
claude account current               # resolved profile name (script-safe)
claude account usage [--live]        # 5h / 7d / per-model weekly windows per login
claude account dashboard [--live] [--once] [--interval N]
claude account watch WS [--threshold 90] [--strategy consume-first|best|next-available]
                        [--models all|none|fable,opus] [--live|--cached]
                        [--interval 60] [--once]        # settings persist per workspace
claude account remove NAME [--keep-login] [--force] [--purge --yes]

claude account workspace create WS --from-profile PROFILE  # dir becomes shared storage
claude account workspace join WS MEMBER [--email E]        # add another subscription
claude account workspace list
claude account workspace remove WS [--purge --yes]
```

## Common flows

**Import existing hand-rolled CLAUDE_CONFIG_DIR folders**

```bash
claude account adopt personal ~/.claude
claude account adopt work ~/.claude-work-4
claude account list --status
```

**Two subscriptions, one shared work environment, auto-rotated**

```bash
claude account adopt work-a ~/.claude-work-4              # dir with the sessions
claude account workspace create work --from-profile work-a
claude account workspace join work work-b                 # sign in the OTHER subscription
claude account watch work                                 # rotate before hitting limits
```

When `watch` rotates, running sessions keep working on their old login; to
move one over, relaunch it with `claude --resume` / `claude -c` — same
transcript, fresh subscription, because the workspace shares everything.

**Parallel personal + work sessions (no switching)**

```bash
claude account map ~/Development/movo work   # work terminals need no commands
claude account map ~/Personal personal
claude account run personal                  # or explicitly, from anywhere
```

## Rules for the assistant

- Logins are interactive browser flows: run `add`/`join` so the user can
  complete them, and say which account to sign in with. Never script or copy
  credentials, keychain items, or tokens.
- `--live` on usage/watch/dashboard is the single sanctioned credential READ
  (in-memory, to query the usage API); default is cached and reads nothing.
- Never edit `~/.config/claude-account/state.json`, member symlinks under
  `…/claude-account/profiles/`, or a profile's `.claude.json` by hand; use
  the CLI. Moving or renaming a member symlink orphans that member's login.
- `use`/`watch` rotations affect newly launched Claude processes; running
  sessions keep the login they started with (`CLAUDE_ACCOUNT_PROFILE` is
  pinned into them for nested calls).
- Removing an adopted profile or workspace member never deletes shared data;
  `--purge` is only for directories claude-account created itself and is
  refused elsewhere by design — don't work around that with `rm`.

## Troubleshooting

- **"a brand-new configuration directory already sees an existing login"** —
  the Claude Code build shares credential storage (old macOS builds).
  `claude update`, then retry.
- **"Claude Code resolves the member link before choosing credential
  storage"** — this platform/build cannot give workspace members separate
  logins (always true on Linux). Use plain isolated profiles instead.
- **"workspace `X` has no selected member"** — pick one:
  `claude account use MEMBER`.
- **`usage` shows "no usage data" for an idle member** — run one session as
  that member, run `watch`, or use `--live` once; observations are then
  remembered and decay correctly.
- **"unsupported state version 2"** from an older claude-account binary —
  upgrade the binary (`cargo build --locked --release && ./target/release/claude-account install`).
- **Shim not first on PATH** (`type -a claude` shows the official binary
  first) — re-add the PATH line `install` printed, open a new terminal.
- **Wrong email shown inside Claude after switching members** — run
  `claude account use MEMBER` again (it rewrites the shared identity), or
  `claude auth login` once in that member if it was joined with a reused
  login.
