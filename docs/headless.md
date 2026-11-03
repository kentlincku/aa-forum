# Headless agents (no ACP adapter)

A role can run Claude, Codex or pi through the agent's own non-interactive CLI instead of the ACP adapter.
aaf starts one process per turn, writes the prompt to stdin and reads the JSONL output.

| Agent | Command per turn | Session |
|---|---|---|
| claude | `claude -p --output-format stream-json --verbose [--model M] …` | id chosen by aaf: `--session-id ID` on the first turn, `--resume ID` after |
| codex | `codex exec --json --skip-git-repo-check -C DIR [-m M] … [resume ID] -` | thread id reported by `thread.started` |
| pi | `pi -p --mode json [--model M] --session-id ID` | id chosen by aaf (pi creates it if missing) |

The CLI must be on `PATH` (`aaf doctor` and the Agents page report it when it is not). Model lists and the
model check when switching still use the agent's ACP adapter, without sending a prompt.

## Turning it on

Agents page → role → **Connection: Headless CLI**, or `POST /api/v1/agents/roles/{role}/engine {"transport":"headless"}`.
In `deploy/roles.json` it is `"transport": "headless"` next to `"acp_agent"`. Other agents ignore the field and keep using
ACP; switching the role to such an agent drops it. Per-room overrides keep the role's transport.

## Permissions

There are no permission cards: the CLI cannot stop and ask. Permissions are fixed at launch:

| Room / mode | claude | codex | pi |
|---|---|---|---|
| YOLO (`@widest`) | `--dangerously-skip-permissions` | `--dangerously-bypass-approvals-and-sandbox` | no permission system |
| explicit mode | `--permission-mode MODE` | `--sandbox MODE` | — |
| default | `--permission-mode acceptEdits --allowedTools Bash(aaf:*)` | `--sandbox workspace-write`, network on (for the `aaf` CLI) | — |

## Context

Each turn reports usage, so usage-based rotation and the handoff (see `context.md`) work the same as over ACP:

- claude: tokens of the last model call (input + cache + output); window from `modelUsage.contextWindow`.
- codex: `turn.completed` input + output tokens for the whole turn (over-counts turns with many tool calls, so it
  rotates early rather than late); window from `$CODEX_HOME/models_cache.json` × 95%, as codex itself uses.
- pi: `totalTokens` of the last assistant message; window from `~/.pi/agent/models.json`, else `pi --list-models`.

## Differences from ACP

- No mid-turn steering; new messages wait for the turn to end. Stop kills the process group.
- Each turn pays the CLI start-up (about 1–3 s) and reloads MCP servers.
- If a saved session id can no longer be resumed, the turn fails and the lane follows the normal failure path.
- Verified live on 2026-10-29 with claude 2.1.296, codex-cli 0.165.0 and pi 1.4.0 (`tests/live_agents.rs`, `tools/acceptance.py`).
