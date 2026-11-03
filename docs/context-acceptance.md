# Role-context continuity acceptance

## Scope and implementation

This change keeps native ACP sessions and adds AAF-owned role × room sources, logical source projections, versioned checkpoints, durable attempt/fault admission, authenticated retrieval, and bounded continuation packages. It adds no summarizer/model calls. It is deterministic preservation/retrieval, not semantic LLM compaction. Automatic byte-proxy rotation is off by default.

Verification below is macOS-only; other operating systems are not claimed.

## Automated gates

- Full `tools/ci-local.sh`: exit 0; web TypeScript/i18n, fmt, clippy `-D warnings`, workspace tests.
- Workspace test output: 305 passed, 0 failed, 4 ignored executions. These are execution totals, not unique scenarios: preserved reviewer harnesses include existing suites.
- Three ignored executions require the native Darwin fault injector and are exercised separately. The remaining ignored historical rotation observation preserves its old assertions; the replacement normative test verifies the approved accounting behavior.
- Runtime + context suites repeated 20 times: every invocation exited 0.
- Recorder-fault regression cases: 15/15 repeated runs passed, including no-injection controls.
- Repository-native fault-injection runner: 18/18 runs passed, covering real ACP stop refusal, connection/registry retention, unrelated-group controls, recorder admission, steering commit ordering and steering byte accounting.

Native runner (Darwin; requires clang):

```sh
python3 crates/features/context/tests/fixtures/native_faults/run_probes.py \
  --output "$TMPDIR/aaf-context-native-check" --repeat 3
```

Use an owned scratch output directory; never inject the library into unrelated processes or model clients. The tests launch inert local fixtures, not model requests.

## Real agents: nine-turn scenario

The tested binary was built from the final source. Adapter/model identities came from ACP responses and fixture-native records, not model self-description:

- Hermes ACP: Copilot `gpt-6-astra`.
- Codex ACP 2.2.2: `gpt-6-luna` (medium).

The fixture used its own owner account, isolated agent state, a conversation-only random decision/canary, and a separate real tool-generated artifact nonce. Source facts were not copied into the new user requests.

| Turn | Scenario | Observed result |
|---|---|---|
| 1 | Seed decision, create artifact, initialize one receipt, leave work pending | Real tool execution; artifact and receipt independently read back |
| 2 | Same native session, no-tools recall | Exact facts and pending state; no history/file retrieval |
| 3 | Hermes → Codex, same role/room | Exact facts and pending state without tools or re-explanation |
| 4 | Ordinary Codex followup | Exact recall; continuation not reinjected |
| 5 | Execute only the unfinished step | Artifact becomes DONE; initialization not repeated; one finalized receipt |
| 6 | Restart the fixture and successfully load Codex session | Exact facts and DONE state without tools; no reinjection |
| 7 | Real `aaf context read --source … --offset 0` | Agent saves the actual API response; original decision/canary independently verified |
| 8 | Stop fixture, substitute a missing native session ID, restart | Actual load error followed by session/new; exact facts and latest DONE state restored without tools |
| 9 | Codex → Hermes | Exact facts and DONE state without tools; effects unchanged |

All nine original deliveries completed, with final role replies and matching `end_turn` checkpoints. The actual ACP prompt continuation-marker sequence was:

```
0, 0, 1, 0, 0, 0, 0, 1, 1
```

The final effect ledger contained exactly `initialized`, then `finalized`; the artifact retained its original nonce. The final context state had `pending=0` and no fault. No-tool controls checked fixture-native Codex tool-call records as well as ACP updates, because adapter notification coverage is incomplete. Completed fixture process groups were checked independently; no live members remained.

## Token observation, not a general cost forecast

A feature-disabled control used the same final binary, engines/models, sandbox configuration and short scenario, with fresh random facts of the same shape. It correctly returned UNKNOWN after switching engines. This is one short matched observation, not a benchmark population or a claim about subscription quota.

At the first Codex prompt after switching:

| Metric | Context disabled | Context enabled |
|---|---:|---:|
| AAF prompt UTF-8 bytes | 1,312 | 3,459 |
| Uncached input tokens reported by Codex | 3,725 | 4,339 |
| Cached-read tokens | 11,008 | 11,008 |
| Output tokens | 23 | 84 |

The observed added input was 614 tokens, with a 2,147-byte continuation package. The enabled ordinary followup returned to 1,312 prompt bytes and contained no continuation marker. Output differs because the control answered UNKNOWN while the enabled run answered the facts.

This does not imply zero subsequent native-history input or a fixed per-task percentage. Native sessions may resend/cache prior context; retrieval/tool loops consume tokens. Long histories, repeated failures, different tokenizers, provider billing and subscription accounting were not benchmarked. Hermes prompt usage was treated as session-cumulative and Codex as per-prompt, with raw usage retained separately; context-size estimates and bytes were not reported as billed tokens.

## Failures found and corrected

Independent counterexamples and real fixtures caught bugs not covered by the initial passing suite: ordinary-turn recovery markers, fault loss on parking, cross-lane starvation, disabled-feature lag semantics, credential persistence, idle-recorder admission, steering barriers/accounting, real ACP group-stop failure, unconditional history retrieval, token-chunk crowding, and tool records displacing later completion state. Failed fixture evidence was preserved rather than relabeled as a pass; final live acceptance used a fresh fixture.

## Remaining boundaries

- An independent review reran the recorder-fault cases (15/15), the native runner (18/18) and targeted regressions and found no reproducible blocking security or logic issue.
- No Windows/Linux execution or compilation.
- No semantic summary extraction, exact vendor-capacity enforcement, automatic filesystem verification, or exactly-once external-effect guarantee. Explicit recovery still requires operator readback of uncertain effects.
- Credential minimization is best effort, not arbitrary secret detection or retroactive scrubbing.
- Native group-stop verification covers the owned process group; descendants that deliberately escape it are outside that guarantee. Legacy CommandDriver stop handling was not upgraded to ACP connection/registry retention semantics.
- Source retention is unbounded; long-lived event indexing, storage growth and retrieval latency have not been load-tested. Existing pre-feature delivered-chat history is not automatically imported.
- No new context-management UI; authenticated API/CLI and the owner recovery runbook are documented in `context.md`.
