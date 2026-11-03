# Multiple Claude / Codex accounts

Several accounts of the same agent can be used side by side. Each account is its own folder with its own login;
a role (or a role in one room) picks which one to use. aaf never reads, copies or moves credentials.

| Agent | Variable aaf sets when starting the agent | Folder contents |
|---|---|---|
| claude | `CLAUDE_CONFIG_DIR` | settings, history and the login (macOS: Keychain, kept per folder) |
| codex | `CODEX_HOME` | `config.toml`, `auth.json`, sessions |

This is the same approach used by other account switchers (`silo`, `claude-acct`, `codex-profiles`): one documented
environment variable per account, no credential swapping, so two accounts can run at the same time.

## Use it

1. Agents page → **Accounts** → **＋ Add account** (agent, id, optional label). The folder is created under
   `<instance>/accounts/<agent>-<id>` (0700). Non-credential settings are copied from the default account
   (Claude `settings.json` and `CLAUDE.md`, Codex `config.toml`); untick to skip.
2. Log in once from a terminal with the command shown, for example
   `CODEX_HOME='…/accounts/codex-work' codex login`. aaf cannot do this for you.
3. Pick the account in the role editor (**Account**), or per room in the room's role dialog (overrides the role).
   Saving checks that the account exists, belongs to that agent and is logged in, and that the model and reasoning
   effort exist for it, without sending a prompt.

`default` is the login the agent uses on its own (no variable set). Ids are unique across agents (`work-claude`,
`work-codex`). Choosing a different account starts a new native session; aaf's own context ledger carries the
conversation over.

## What the page shows

Login state (via `claude auth status` / `codex login status`), plan, a masked email, which roles and rooms use the
account, and the remaining 5-hour / weekly quota. Codex comes from `codex app-server`; Claude comes from Claude
Code's own `claude -p "/usage"` (a local slash command: no turn, no tokens, aaf never touches the login). A Claude
account logged in with an OAuth token (`CLAUDE_CODE_OAUTH_TOKEN`) only gets a cost summary from `/usage`, so no
quota windows are shown for it.

There is no automatic switch when an account runs out; you choose the account. (Several account tools state that
rotating accounts to get around usage limits may violate the provider's terms.)

## Files

- `deploy/accounts.json`: `{"accounts": {"<id>": {"agent", "label", "dir"}}}`, written by the page.
- `roles.json`: `"account": "<id>"` on a role, or inside a room override (`"default"` = default login in that room).
- Removing an account only removes the entry. Its folder and login stay on disk; a role or room still using it blocks the removal.
