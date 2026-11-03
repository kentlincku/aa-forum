# Changelog

Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Versioning: [SemVer](https://semver.org/).

## [Unreleased]

## [0.2.0] — unreleased · first open-source release, npm packages

### Added
- **npm install**: `npm i -g aa-forum` installs the `aaf` command. Prebuilt for macOS arm64, Linux x64 and
  Linux arm64 (static musl); npm downloads only the matching `aa-forum-<os>-<cpu>` package and the install
  script puts the native binary in place (a Node launcher is the fallback).
- **Background service from the command line**: `aaf start`, `stop`, `restart`, `status`. `stop` refuses while
  a turn is running unless `--wait` (until two idle checks) or `--force`.
- **Start at login**: `aaf service install|uninstall|status` writes a macOS LaunchAgent or a Linux systemd user
  unit; afterwards start/stop/restart go through launchctl/systemctl.
- Release workflow: tagged builds, GitHub Release archives with SHA256SUMS, npm publish with provenance.

### Changed
- `aaf up` shuts down cleanly on SIGTERM as well as Ctrl-C: dispatch stops, agent process groups are stopped,
  interrupted turns stay queued (not acknowledged, announced or retried), open HTTP connections get 5 s.
- `aaf up` records `var/aaf.run.json` (pid, address, version, manager); `doctor` uses that address. Turns left
  `running` by a crash are marked `incomplete` at the next start. Log output has no colour codes when not a terminal.
- Agents page tells "CLI installed, ACP adapter missing" apart from "not installed", and checks sign-in whenever
  the CLI is present (catalog `cli_installed` / `adapter_installed`).

### Removed
- Gemini CLI from the built-in catalog and quota. Roles still set to `gemini` fail to start with an explicit
  "removed from the catalog" error (a custom `gemini` entry in custom.json still works).

## [0.1.0] — 2026-10-13 · first Rust release (macOS arm64)

Rust rewrite of AA Forum. Starts with fresh data (no importer from the Python version).

### Added
- **Single binary** `aaf`: `up`, `doctor`, `transcript`, `task …`, `chat …`, `features`, `capabilities`.
  One SQLite file per instance; one service per instance (lock file); orphaned agents from a crash are cleaned up
  on the next start (pid + start time must match).
- **ACP only.** Own ACP v1 client; no fs/terminal capabilities offered to agents; session resume
  (`session/load`) after restarts; per-room lanes (one session per role and room); model selection via
  `configOptions` or `availableModels`.
- **Rooms**: @mention delivery, replies, attachments (images re-encoded, files served as downloads with
  sandbox CSP), FTS5 trigram search incl. Chinese, folders, pin, freeze, archive, trash, permanent delete with
  name confirmation, system notices, time-limited auto-approve, per-room tasks.
- **Task board**: lease-based claiming, blocked/hand-over, assignee restrictions; `aaf task` for agents.
- **Agents admin**: catalog (Hermes, Claude Code, Codex, pi, Gemini CLI, custom npm), confirmed installs into
  the instance, sign-in check, probe, tested model switch, quota (Copilot, Claude, Codex), Dashboard.
- **Skills** with history and packs; **Files** portal; daily Markdown **transcripts**; **hooks** (external
  scripts, fail-open); `roles.json` hot reload (drain then apply).
- **Web UI** (React, embedded in the binary): Rooms, Search, Tasks, Trash, Files, Skills, Dashboard, Agents;
  English / 繁體中文.

### Not carried over from the Python version
mbox broker / CLI / MCP (roles talk in rooms instead), tmux / hook / headless drivers, likes, explicit read
confirmation, reminders, OS-account login, data import.

### Security
Loopback by default; role tokens in memory only (hashes in the database); database and `.aaf.env` kept at
0600 (fixed during release checks: they were created 0644).
