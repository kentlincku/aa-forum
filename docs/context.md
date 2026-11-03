# Durable role × room context

AAF retains its own context in the instance database while continuing to use native ACP sessions. This is **deterministic source preservation and a budgeted index, not semantic LLM compaction**. There are no extra model or summarizer calls.

## Agent usage

During a role turn, `AAF_ROLE`, `AAF_ROOM`, `AAF_TOKEN`, and `AAF_URL` are supplied by AAF:

```sh
aaf context read
aaf context read --after 20 --limit 20
aaf context read --source 12 --offset 0
aaf context read --raw --source 12 --offset 0
```

The first form returns logical source IDs, kinds, bounded excerpts, `next_after`, checkpoint versions/cursors, and a `diagnostic` object (fault/inflight/pending/recovery instructions). Completed responses supersede their own token chunks in this projection; raw records are never deleted. An unfinished stream is one readable `assistant_partial` aggregate with `source_ids`. `--raw` (HTTP `raw=true`) lists/retrieves immutable raw observations instead, with the same lane authorization. Partial aggregates are live projections: finish paging before the next turn, or use their raw source IDs for stable retrieval. Pre-attempt legacy chunks cannot safely be matched to final replies and remain a separate aggregate. Follow `next_after` until null to enumerate the complete history; do not assume that the recent index contains every requirement. Use `--source ID` to read full source JSON in UTF-8-safe chunks, following `next_offset` until null. `text` is a fragment of the source JSON, not necessarily a complete JSON document until all pages are concatenated.

Explicit lane selection: `aaf context read --room 7 --role writer`. A role token can only read its own role's sources in a room where it is currently a member. Role tokens are role-scoped, not per-process/room credentials: the same role can explicitly select another room where it is also a member; contexts are never implicitly combined. Other roles and nonmember rooms are denied. Owners can explicitly inspect a lane in an existing, nondeleted room. Humans without owner authority cannot read role-private context. Source IDs are also checked against the requested lane.

HTTP equivalent:

```
GET /api/v1/context/rooms/{room}/roles/{role}/sources?after=0&limit=20
GET /api/v1/context/rooms/{room}/roles/{role}/sources?source={id}&offset=0
Authorization: Bearer <token>
```

Limits: source list `limit` is 1–100, cursors nonnegative, rooms positive. Invalid offsets, malformed queries, and cross-lane source IDs fail rather than silently falling back. CLI rejects malformed response JSON. Raw tool evidence is excluded from room SSE, including owner SSE, and is available only through explicit authorized source retrieval.

## What survives

- Delivered requirements, decisions, next actions, and artifact references in input messages. Edited delivery text is an additional source revision; unchanged retries deduplicate.
- Sanitized observable assistant message chunks (including partial crash suffixes), final responses, and non-success/error results. Private thought chunks are not copied into this feature's canonical source ledger.
- Adapter-supplied tool IDs, status, input, output, content, and locations, when supplied. An absent result is marked as a coverage gap. A reported result does not prove the external effect was committed.
- Immutable checkpoint versions with source cursors; the last successful injection cursor/version and recovery-pending state are committed transactionally with the checkpoint. Source responses remain recorded even if checkpoint indexing fails.
- Pending and accepted steering messages use the same source ledger; a steering acknowledgment has its own checkpoint status and is not falsely marked as a recovery injection.

The original event log is persisted before indexing. Recovery indexes missing observable event suffixes, in bounded batches, instead of assuming the last checkpoint is the entire history. Source records are not deleted on an engine/session switch. The feature does not retroactively import old delivered chat history from before it was enabled, nor private adapter memory; already-persisted observable runtime events may be indexed on recovery. There is no source retention/deletion setting in this version.

## When a package is injected

- Actual ACP `session/new`, including fallback after failed/unsupported native load.
- Between-turn session rotation after the configured observed-text threshold.
- Recovery after a non-success prompt, or any ordinary/recovery attempt whose checkpoint did not commit. A durable inflight marker is written with inputs **before** execution. Unknown-effect faults and interrupted attempts block automated admission across parking/restarts until owner recovery.

Ordinary successful turns and successful native loads with no pending recovery do not re-inject checkpoints. A successful native load alone cannot erase an uncommitted recovery marker. Role identity and assigned skills still use their existing providers. A short retrieval-discovery line is added to the ordinary preamble; it is not a summary.

The package includes a safety/coverage header and budgeted JSON-quoted early/recent conversation excerpts, including unfinished `assistant_partial` responses. Tool events do not consume those excerpt slots: when bytes remain, a separate single-line index references at most eight recent tool source IDs. Omitted requirements and tool evidence remain available by source ID. Raw tool input/output is **not** automatically inserted. AAF never executes stored commands. The current request takes priority: use carried evidence for recall and fetch only missing information when that request permits tools. No-tools recall does not require history retrieval. Read back unknown effects and inspect dirty/uncommitted artifacts before repeating actions or claiming publication.

## Configuration

Set in the instance `.aaf.env` or environment:

| Key | Default | Meaning |
|---|---:|---|
| `AAF_CONTEXT_ENABLED` | `1` | `0` disables this feature's source recording/recovery; historical retrieval remains available. No new raw ToolEvidence events or continuity recorder/fault semantics are attached; legacy runtime stream/status events still operate. Broadcast lag does not block a successful legacy reply/ack. Only 0/1 accepted. |
| `AAF_CONTEXT_BYTES` | `16384` | Continuation byte ceiling. 0 disables automatic packages, not recording/retrieval. |
| `AAF_CONTEXT_TOKENS` | `8192` | Conservative token-budget proxy using UTF-8 bytes as an upper bound, not a vendor tokenizer. 0 disables packages. Effective limit is min(bytes,tokens). |
| `AAF_CONTEXT_ROTATE_BYTES` | `0` | Opt-in observed input/final-response UTF-8 byte proxy before between-turn rotation. 0 disables rotation. Rotation is also off when automatic packages are off. |
| `AAF_CONTEXT_ROTATE_PCT` | `auto` | Replace the session between turns once the agent's own `usage_update` (ACP: tokens in context / window size) reaches this percent. `auto` = Claude 60, Codex 70, others 65 (by `agentInfo.name`); `off` disables; or a fixed 1–99. Agents that never report usage are unaffected; use the byte proxy for them. |
| `AAF_CONTEXT_PAGE_BYTES` | `16384` | Full-source page bytes; 4–1048576. UTF-8 boundaries are preserved. |

Nonzero effective package budget must be 1024–1048576 so the complete safety header fits. Invalid configuration fails closed. The budget covers the continuation package, **not** identity/skills, newly delivered messages, the existing native transcript, or a model's hidden context. The rotation proxy counts delivered input body text and final response text once per checkpoint, excluding the injected continuation and JSON escaping/metadata. It intentionally excludes tool payloads, streamed duplicates, skills/preamble, private adapter messages, and failed-checkpoint output. It is **not** actual token use, total prompt size, or provider capacity telemetry. Byte-proxy rotation is off by default because it is not capacity; agent-reported usage (`AAF_CONTEXT_ROTATE_PCT`) is the default trigger.

### Handoff before rotation

When rotation is due and the lane is idle, the dispatcher first asks the old session for a handoff (one extra turn, no tools, under 600 words, 180 s timeout). A usable reply is stored as a `handoff` source and placed first in the next continuation, capped at half the remaining package budget so verbatim excerpts still fit; it is never posted to the room. If the agent cannot answer (quota exhausted, provider error, non-`end_turn`, empty, timeout), no handoff is stored and the new session gets the ordinary ledger excerpts. Each attempt publishes `runtime.context.handoff` with `written`, `reason`, `used`, `size`.

## Failure and lifecycle handling

Roster/engine changes stop dispatch to affected lanes and drain current turns/steering. Scheduling and replacement are serialized through a lifecycle lock. The old executor is stopped and its final event queue drained before the forwarder is aborted. If `Driver::stop` returns an error, switch/close fails, ownership and the live recorder remain registered, and no replacement is admitted. Even a refused stop drains final evidence. ACP serializes group signalling and reaping under its owner/child locks, retains the unreaped leader until a bounded OS process-table census confirms no live group members, and only then waits/reaps. EPERM is not absence: a live or unreadable group returns unverified and retains the connection and registry. Darwin zombie-only groups are independently recognized as quiescent; no signal is sent using an already-reaped child handle. An in-flight completion cannot overwrite a newly selected executor's state.

Storage/recorder failures leave delivery unacknowledged. An observation/publication failure or broadcast lag immediately latches the shared lane fault, including while idle; persistence is attempted, but a failed persistence write cannot clear the in-memory admission block. Both new prompts and steering drain pending recorder work before executing. Steering, like a normal prompt, drains again before checkpoint completion and delivery acknowledgment. An uncommitted attempt independently blocks admission across restart even when writing its fault message fails. Automatic parking, roster replacement and service restart do not authorize retries. Other lanes still launch after started/prepare/rotation errors, and the chat job runs parking/reaping even if dispatch reports an error. `once()` waits for all launched lanes before returning an error. `tick()` reports synchronous preparation errors; asynchronous failures are reported through durable diagnostics/events.

### Owner recovery runbook (explicit retry authorization)

1. Inspect `aaf context read --room 7 --role writer` using an owner session credential in `AAF_TOKEN` (or the authenticated source GET). `diagnostic.fault` is the stored failure, `inflight` is an uncheckpointed attempt, `pending` means a package is still owed. `blocked` means admission is unsafe; while a turn is actually running, inflight is expected, not proof of a fault. The existing room lanes API shows busy state. `runtime.context.failed` is also emitted for post-execution failures; there is no new fault UI in this version.
2. Repair the storage/config/event error and **read back the affected external state** (files, API receipt, task status) before authorizing queued work. An interrupted operation is not a rollback. Never replay a stored command merely because the checkpoint failed.
3. Wait for a busy turn or explicitly stop it: `POST /api/v1/chat/rooms/7/lanes/writer/cancel`. Close returns HTTP 409 while busy. A process whose stop is unverified retains ownership and blocks replacement; resolve that process before retrying close.
4. Explicitly recover with owner authentication:
   ```sh
   # OWNER_CURL_CONFIG is a pre-existing private (0600) curl config containing
   # the owner Authorization header; do not paste credentials into commands/logs.
   curl --fail-with-body --config "$OWNER_CURL_CONFIG" -X POST \
     "$AAF_URL/api/v1/chat/rooms/7/lanes/writer/close?forget=false"
   aaf context read --room 7 --role writer
   ```
   A successful close returns `{"closed":true}`, clears the durable fault/uncommitted-attempt admission block, and **preserves pending recovery and all evidence**. It authorizes the queued delivery to run again; the next successful checkpoint clears pending. A role, lead or other non-owner gets 403 and cannot clear the fault. If a stopped recorder reports a historical drain error, that close returns an error and leaves the fault; after repairing/inspecting the evidence, a second explicit close of the now-stopped lane completes recovery. Do not treat the first error as success.

There is no separate `context recover` CLI command: the exact close API above is the explicit recovery operation. Disabling context is an explicit legacy escape hatch, not an automatic recovery path; it bypasses these protections and must not be used to infer external effects are safe.

### Credential minimization

Before publication to `events` and insertion into the source ledger, the recorder replaces known lane credential values (including the injected `AAF_TOKEN`), common nested credential fields, bearer/basic authorization, URL userinfo, PEM private keys, key/token assignment patterns and common credential prefixes with `[REDACTED]`. Input/final output copies use the same policy. No credential files are read by this mechanism or the tests; tests use fake values only. This is best-effort minimization, not a universal secret classifier: arbitrary obfuscation/encoding, secrets split across notifications, unknown unlabelled values, existing event/chat rows, and adapter-private logs are outside its guarantees. Existing rows are not retroactively scrubbed. Do not request an agent to dump its environment or credentials.

Durable continuity requires `room_lanes=true`. An unscoped/mixed-room lane fails closed rather than mixing different rooms. Native ACP continuity is the supported production path; command drivers have no native conversation memory, so keeping a long-running conversational session remains the adapter's responsibility.

## Verification boundaries

Fake ACP, real SQLite, real chat inbox, authenticated HTTP handlers, and the compiled CLI are tested locally, including switch/load/fallback, busy drain, event suffixes, malformed data, permissions, paging/budgets, and failure commit ordering. Separate real Hermes/Codex acceptance, native failure injection and independent review are recorded in [context-acceptance.md](context-acceptance.md). Deployment and other operating systems are not claimed.

Native Darwin regression coverage includes an inert same-group child, an unrelated-group control, denied `killpg` via a scoped DYLD interposer, retained registry/connection after repeated refusal, native zombie-group EPERM, and no signal after reaping. Run `python3 crates/features/context/tests/fixtures/native_faults/run_probes.py --output <scratch-directory>` after loading the Rust toolchain. The runner executes the otherwise-ignored native probes; ordinary CI also runs recorder/steering and owned-group tests. This confirms the owned ACP process-group contract, not descendants that escape the group, inner agent/tool authorization, or Windows/Linux behavior. Unsupported non-Unix tree stops fail closed. The legacy command-driver wrapper reports unverified group cleanup explicitly but is not the ACP ownership/replacement contract.
