# Testing Strategy

## Goals

Testing must prove five things independently: canonical behavior, recovery, effect safety, authority isolation, and the Performance Contract. Provider availability and network timing are not substitutes for deterministic local evidence.

Tests target deep-module interfaces. Internal helpers are tested directly only when they implement a dense pure algorithm; otherwise behavior is asserted through the owning module so refactors do not multiply shallow tests.

## Test Layers

### Pure and Property Tests

Cover canonical identifiers and state transitions, configuration merging, context accounting, token-class normalization, price calculations, Usage Window membership, Task graph invariants, capability subset rules, Read Set validation, deterministic tool identities, and event serialization.

Property tests must include:

- Event replay is deterministic for any valid complete-transaction prefix and fails closed on sequence, transaction, framing, or transition tampering.
- Config resolution is deterministic and respects built-in < user < project < CLI precedence.
- Delegation never creates a capability outside the parent snapshot.
- One writable worktree never has two simultaneous Workspace Leases.
- Usage Rollups equal aggregation over their source Usage Records.
- Half-open and cross-midnight Usage Windows assign an attempt at most once per named window; overlapping windows remain independent.
- Usage Window tests cover both occurrences of a repeated DST hour, the absence of instants in a skipped hour, versioned time-zone rule provenance, and rejection when Windows `local` cannot resolve to an IANA zone.
- Compaction never changes the Event Ledger or Tool Ledger.
- Append-only memory supersession leaves one deterministic active lineage.

### Golden Protocol Tests

Store redacted request/event fixtures for OpenAI Responses, OpenAI Chat Completions, and Anthropic Messages. Golden tests exercise canonical translation, streaming text, reasoning data, tool calls, usage, cache fields, service tier, incomplete responses, provider errors, and unknown fields.

Raw provider events remain fixture evidence; assertions target canonical Items, Events, and Usage Records. A protocol fixture update requires a reviewed explanation of the upstream change.

### Module Contract Tests

Each deep module is tested through its Interface:

- Runtime Kernel: Turn admission, cancellation, budgets, recovery, and terminal outcomes
- Ledger Store: append, sync boundaries, replay, migration, backup, compare-and-swap, and corrupt-tail handling
- Provider Runtime: capability freeze, dialect/transport fallback, epoch changes, interruption classification, explicit recovery, and usage normalization
- Tool Runtime: approvals, idempotency, ambiguous outcomes, sandbox policy, and MCP isolation
- Agent Team Runtime: Task ownership, Runtime-issued Agent Sessions, Delegation, messaging, Blocked resolution, failure propagation, Completion Capsules, and transaction recovery
- Workspace Coordinator: leases, worktree allocation, Read Set revalidation, merge, and conflict reporting
- Context Engine: pressure thresholds, artifact offload, folds, checkpoints, rebase, memory retrieval, and stale-result rejection
- Config Runtime: schema coverage, drafts, effective provenance, atomic writes, and application timing

True external dependencies use mock adapters. Windows facilities use focused integration tests around audited wrappers rather than exposing platform details through every module.

The current Phase 1 spine and first Phase 2 Tool policy slice cover a strict
subset of these end-state contracts:
versioned Config TOML, schema/path/type validation, precedence/provenance,
dry-run drafts, revision conflicts, atomic writes, backup repair, last-valid
external-edit behavior, symlink rejection, and immutable Runtime epochs;
canonical ID and Item bounds;
Ledger append/sync/replay, single-writer exclusion, expected-Head conflict,
torn-tail read-only inspection and writer repair, checksum/length/schema
tampering, and symlink rejection; deterministic Provider success and malformed
output; bounded generic SSE framing for LF, CRLF, lone CR, fragmented UTF-8,
comments, multiline data, byte and per-event data-line limits, poisoning, and
incomplete streams; a
strict OpenAI Responses event subset with streamed text and function-call
assembly, complete/failed/incomplete/error terminals, optional cache/reasoning
usage and service tier, unknown-field tolerance, transition and sequence
rejection, event/output/argument byte and nesting bounds, and redacted Debug
output; Runtime
admission resume, prepared-output reconciliation, idempotent
acknowledgement, and blocked replay; and cross-process headless CLI output,
status, resume, and reconcile behavior. The standalone durable Agent Team
adapter additionally covers synchronous receipt-before-publish ordering,
planning failure atomicity, exclusive writer ownership, every Team Event kind
through a complete lifecycle and restart, old-session rejection, torn-tail
complete-prefix recovery, and checksum/schema/kind failure. Runtime Kernel
integration tests cover dedicated writer ownership, Kernel-owned root admission
and duplicate rejection, per-open complete non-terminal Session rebind after replay,
Active/Dormant/Blocked recovery, terminal exclusion, stale-session rejection,
the absence of an ID-to-session conversion, same-transaction operation identity,
pending-operation admission blocking, explicit durable acknowledgement,
idempotent duplicate acknowledgement, and operation status after restart. A
private Team Ledger crash harness additionally covers eight write/flush/sync
error points for command frames, eight for acknowledgement frames, and six
authenticated child-process termination points from before the frame write
through a synced frame before Fold publication. I/O errors poison the live
writer; process termination leaves no writer to continue. Both paths forbid
blind retry and require reopen, yielding either a complete prefix known not to
contain the command or a complete transaction whose missing caller
acknowledgement is exposed as pending and blocks later Team commands. The core
Tool Runtime additionally covers canonical argument hashing, stable identity
deduplication across restart, changed-meaning rejection, Agent-session-bound
approval requests, exact approval/resource binding, independent filesystem,
process, and network Capability denial, grant expiry, raw-argument and external-
reason exclusion from the Tool Ledger and Debug output, and explicit ambiguous-
effect reconciliation. Fault injection proves that a prepared-effect append
failure invokes no executor,
while an outcome append failure after one invocation poisons the writer and
reopens in a reconciliation-required state. A same-binary child-process matrix
also terminates success, failure, and ambiguous executions at six
representative points from executor return through outcome-frame sync. Reopen
must expose either the complete terminal outcome or reconciliation-required,
must never return the same identity to approval, and must retain one external
effect marker across all 18 cases. The configured Responses, Chat
Completions, and Messages adapters now bind typed, frozen Provider Profile,
dialect, and route metadata to origin-scoped credential lookup. Live-provider
validation, non-Windows credential backends,
configurable proxy policy, broader TLS platform evidence, broader canonical
Runtime Items, reasoning/refusal/annotation and other unimplemented Responses
event kinds, reconnect/retry fixtures, MCP adapters, richer TUI/App Server Tool
presentation, and cross-process cuts for the remaining Tool, Runtime, Provider,
delivery, and product acknowledgement boundaries remain pending. The fixed CLI
path now flushes the Team receipt and exact approval
event before acknowledgement.
Migration/backup remains in the candidate storage harness rather than these
provisional product adapters.

Product integration tests now drive a private `local.echo` adapter through the
real Kernel approval/effect seam. They cover successful child output and digest
replay, known pre-spawn and nonzero-exit failures, timeout and output-overflow
reconciliation without replay, deadline-controlled blocked stdin and output
floods, parent environment and working-directory exclusion, exact process/
argument/resource rejection, raw-resource exclusion from the Tool Ledger, and
Unix process-group descendant termination. The Windows job also runs a
platform-only descendant-denial test; kill-on-close and process-memory-limit
evidence on a real Windows runner/Target remains a separate gate.

Product-driver tests cover explicit grant, denial with zero executor calls,
an interrupted approval reopened from the three durable Ledgers before exactly
one effect, fail-closed partial sidecars, ambiguous-effect reconciliation, and
process death after a durable Tool success without repeating the effect. A
presentation test leaves Product output unacknowledged after a broken writer.
A binary test exercises `headless --tool local.echo`, verifies the Team receipt
and final stdout, then reopens the same three Ledgers for another Turn without
presenting the already-acknowledged Team receipt again; Runtime returns to
`ready` after both Turns.

Another same-binary test kills the product after the executor observes a
durably prepared effect. Restarted `tool status` exposes the call as
reconciliation-required, headless execution remains blocked, and explicit
observed-failure reconciliation permits a later Turn. A companion case records
an observed-success digest, proves conflicting repeat reconciliation remains
idempotent, and never invokes the effect again. Missing state inspection and
reconciliation create no Ledgers; incomplete sidecars fail closed.

The core Tool Runtime additionally launches an authenticated copy of its own
test binary for each success, failure, and ambiguous executor result. The
supervisor terminates it after executor return, after the frame length header,
mid-frame, immediately before the commit byte, after flush, or after sync.
Complete frames replay terminally; absent or truncated frames recover as
reconciliation-required and block new effects until explicit reconciliation.
Every case verifies one external effect, stable repeat identity, idempotent
reconciliation, clean second reopen, and exclusion of raw arguments, outputs,
and executor reasons from the Tool Ledger. This is deterministic process-death
coverage, not exhaustive byte-offset, real power-loss, or Windows
directory-entry durability evidence.

The first fixture Provider/Tool tracer bullet decodes and normalizes one
Responses function call, requires a current Session and exact Tool authority,
durably crosses approval and `EffectPrepared`, invokes one injected executor,
continues the Provider once, and replays the acknowledged canonical output plus
two Usage Records. Companion tests prove stale Sessions invoke no Provider,
ambiguous effects never reach continuation, non-UTF-8 Tool output is blocked,
and process death after a durable Tool success cannot repeat the effect.
Migration tests replay a historical schema-1 Ledger before appending current
events, decode historical schema-2 and schema-3 Provider Epoch shapes
separately, and round-trip current Provider Profile, dialect, Config Usage
Window, Usage Attempt, Price Schedule, selected output-token data, reasoning
effort, service tier, and distinct template-mirror pricing provenance while
rejecting fingerprint, outcome, timestamp, and transition tampering. A schema-5
Config Epoch without the optional token field, a schema-6 Config Epoch without
request-policy fields, and a schema-7 Config Epoch without template-mirror tags
remain replayable under current Runtime Event schema 11; schema 8 Ledgers remain
compatible before the schema-9 Model-selection event, and schema-9 Ledgers
replay with legacy untyped block origin before schema-10 cancellation events.
Schema-10 blocks replay without a retry stage and therefore remain
non-retryable; schema-11 round trips the stage and retry-request event.

Product integration tests also run the configured Responses, Chat Completions,
and Messages adapters against concrete loopback HTTP tracers. They resolve and
freeze the fixture Provider Profile through Config Runtime, then validate the
selected POST route, model, input or messages, streaming flags, and synthetic
credential header; stream fragmented SSE fixtures through the matching core
decoder and Runtime; and retain fixed redacted failure categories. Responses
coverage includes canonical replay, HTTP 503, timeout, upstream private-body
exclusion, unsafe endpoints, local trusted/untrusted TLS roots, and status
policy. Chat coverage includes exact frozen dialect selection, canonical text
and usage, missing credential or explicit dialect before network access, HTTP
503, wrong content type, malformed SSE, and one exact two-request Tool protocol:
advertised `local_echo`, streamed call, canonical `local.echo` mapping,
correlated assistant/Tool messages, final text, and two Usage Records.
DeepSeek Chat coverage additionally binds the exact template and route, uses
Bearer authorization, sends `max_tokens` with non-thinking mode, omits the
Beta-only Tool `strict` flag, preserves top-level cache-hit usage, retains the
same frozen policy through one continuation, and rejects unsupported reasoning,
service tier, a limit above 384K, or a serialized request above 128 KiB before
network I/O. Responses continuation coverage also rejects a mismatched frozen
dialect and a second Tool call at the adapter boundary.
DeepSeek Responses coverage admits V4 Flash only, validates the exact
`/responses` request and bounded reasoning stream without projecting raw
reasoning text, rejects service tier or unsupported effort before network I/O,
resolves a Pro Responses preference to Chat before admission, and proves one
stateless Tool continuation with the effective Responses dialect frozen in the
Provider Epoch.
OpenCode Go Chat coverage binds the exact template, release-catalog model, and
Chat dialect before credential lookup; asserts the Bearer-authenticated
`/chat/completions` request and `max_completion_tokens`; normalizes the shared
bounded Chat fixture; reconstructs the frozen Provider Epoch; and proves unknown
models or unsupported reasoning/service-tier policy cause no network request.
The public `headless --preset` test proves an exact OpenCode Go Chat preset
reaches the credential boundary before Turn admission.
Messages coverage binds the exact DeepSeek template and frozen Messages route,
uses a sensitive `x-api-key` plus pinned compatibility-version header without
an `Authorization` header, disables unsupported thinking, and sends the frozen
selected-Preset output-token limit or a bounded 4096 fallback. Responses and
Chat request fixtures assert their dialect-specific fields, and all three Tool
continuation fixtures assert the initial and continuation requests retain one
frozen output limit. OpenAI Responses and Chat fixtures additionally assert
exact reasoning and service-tier fields on initial requests and continuations;
DeepSeek Chat and Messages prove either unsupported policy fails before network
I/O, while DeepSeek Responses maps supported effort and rejects tier. Coverage
also includes canonical text/usage, missing credential
and unsupported-template rejection before network access, HTTP 503, wrong
content type, provider error SSE redaction, and one exact `tool_use`/
`tool_result` continuation with two Usage Records.
All three HTTP dialects also have one-connection interruption fixtures. They
prove an early EOF before any decoded event reports `BeforeFirstEvent`, an EOF
after one event reports `AfterFirstEvent`, malformed semantics remain
`InvalidResponse`, and no path reconnects or retries. Core redaction tests cover
the separate `BeforeResponse` stage and prove diagnostics retain only the stage
and bounded byte count.

Provider cancellation tests first persist a blocked Turn, completed failed
Usage/cost evaluation, and immutable Config/Provider Epochs. Core replay proves
one schema-10 `TurnCancelled` returns the Runtime to ready, repeating it is a
no-op, and the next Turn invokes the Provider only once. Product and public CLI
tests prove exact recovered Agent ownership, strict no-create/no-repair opens,
Team/Tool byte identity, restart recovery, and a successful next headless Turn.
Negative tests preserve bytes while rejecting missing state, prepared output,
Tool denial or reconciliation, and the corresponding delivery/tool recovery
still succeeds afterward.
Provider retry tests persist failed Usage/cost evidence, expose retryability only
for `BeforeResponse` and `BeforeFirstEvent`, append one explicit schema-11 retry
request, recover it as `resume-required`, reuse the frozen Provider Epoch, and
record a new Attempt. Core tests reject partial-stream, malformed, post-Tool
continuation, duplicate, and stale-session requests without changing the
relevant Ledger bytes. Product and public CLI tests prove recovered Agent
authority, successful replay, a second early failure requiring a second request,
strict missing/incomplete-state handling, and Team/Tool byte identity. These
tests do not claim that retry is free of remote work, usage, or billing.
Windows-only tests exercise Credential Manager bind, replace, resolve, and
forget. This does not cover live credentials, proxy authentication,
automatic retry policy or partial-stream reconnect, live Providers, or broader Tool presentation.

The first Usage projection suite durably records Provider request and
continuation attempts, closes interrupted attempts only on explicit resume,
preserves frozen requested reasoning effort and service tier separately from
observed metadata,
and rebuilds cached Turn, Thread, Agent, Team, rolling, and named-window
rollups. A deterministic exhaustive small-input test compares cached totals to
source attempts across exact, estimated, unknown, and failed combinations;
overflow remains unknown instead of wrapping. Window tests cover half-open and
cross-midnight membership, both repeated DST instants, skipped local hours,
concrete `local` IANA resolution, pinned rule-set provenance, duplicate-window
rejection, and changed definitions under one name. Product tests verify Agent
and Team scope after replay, and the `stats` JSON excludes user input.
Price Schedule tests cover schema ownership and nested routes, zero-valued fixed
integer rates, provider/pricing provenance, half-open effective intervals,
selector-overlap rejection without Config mutation, exact token-class arithmetic,
missing evidence, inconsistent accounting, checked overflow, and historical
schedule immutability. Runtime tests require Usage kind 12 before cost kind 13 in
one transaction, rebuild the same frozen estimate after reopen, reject a tampered
amount against the Config Epoch evidence, and retain schema 1-4 replay. Product
tests prove resolved Config schedules reach admission, `stats` emits the frozen
version/currency and per-currency rollup without user text, and the terminal-neutral
statusline distinguishes known, mixed-unknown, estimated, and overflow cost facts.
Stats compatibility tests keep the original complete JSON snapshot unchanged.
Report tests cover summary-only output, bounded first and continuation pages,
cursor-checksum corruption rejection, requested-instant mismatch, page-size limits,
and stale-revision rejection after another Usage Attempt is appended; no page
silently combines attempts from different Ledger heads.

### Crash and Recovery Tests

Crash injection runs at every Durability Boundary and representative byte offsets around storage commits. Restart must produce one of two explicit outcomes: the effect is known and not repeated, or the effect is ambiguous and blocked for reconciliation.

Required scenarios include:

- Process exit before and after approval durability
- Exit during streaming batch append
- Exit before, during, and after a tool side effect
- Truncated or checksum-invalid Ledger tail
- Checkpoint compare-and-swap conflict
- Skill content changed before resume
- Provider reconnect after partial output or tool call
- Schema migration interrupted before replacement
- SQLite WAL VFS write, short-write, and sync errors followed by a no-fault reopen, integrity check, and complete-prefix classification

### Security and Isolation Tests

- Credentials never appear in TOML, Ledger records, checkpoints, logs, bundles, or command history.
- Changing Provider Origin cannot silently reuse a credential or catalog price.
- Remote origins require HTTPS; plain HTTP succeeds only for a loopback host with that profile's explicit insecure-loopback opt-in.
- Tool filesystem, process, and network authority are tested independently.
- Child processes remain inside Job Object lifetime and resource policy.
- MCP outputs and discovery metadata cannot create instructions, endpoints, approvals, or capabilities.
- Delegated Agents cannot access parent-only credentials, worktrees, tools, or Approval Grants.
- Agent IDs alone cannot authorize commands; Agent Sessions cannot cross Team Runtime or recovery boundaries.
- Diagnostic Bundle redaction is tested with adversarial paths, headers, tokens, and tool output.

### Fuzzing

Fuzz parsers and state machines that accept untrusted or crash-sensitive input:

- SSE, WebSocket, Chat Completions, Responses, and Messages events
- MCP messages and elicitation payloads
- Config TOML and schema migrations
- Event Ledger framing and recovery scans
- Tool-call arguments and streamed JSON
- Terminal input and width calculations
- Memory and checkpoint import formats

Fuzz failures become minimized regression fixtures.

### UI and Command Tests

Current pure tests freeze the five root Command Paths, nested Config routes,
token-prefix/fuzzy resolution, raw query limits, schema-to-editor-route
completeness, and credential binding-status-only readback. Terminal-neutral
presentation tests cover bounded Slash results, configured-preset and release
catalog search, known primary-dialect compatibility, explicit unknown live
availability, product-adapter gating, recovery/blocker projection, Config repair
redaction, and a pure in-memory subprocess smoke with no filesystem path input. Config
editor tests route `/config pro url` to one concrete Profile field, exercise
staged preview/reset/commit, retain an invalid draft for correction, reject
generic credential mutation and read-back, and prove a stale revision cannot
overwrite the winning commit. Controller tests cover Config Center and focused
editor navigation, explicit dirty-draft discard, credential-safe screens, and
failure-preserving commits. Object lifecycle tests freeze typed nested add/remove
routes, schema-driven multi-field Profile/Preset/Usage-Window creation, whole
target-layer deletion, reference-safe failure, backup creation, and Controller
create/delete projection. Terminal-neutral golden tests freeze exact 40x12,
80x24, and 160x50 status rows, deterministic hidden-segment order, layout height,
and grapheme-safe CJK/emoji/combining-mark text fitting. The subprocess smoke
emits the same three layouts without accepting a filesystem path. These tests do
not by themselves constitute an ANSI/VT backend, live terminal input, or ConPTY
terminal claim. Product terminal tests now additionally freeze Direct VT stale-cell
clearing, wide-cell geometry, zero-byte identical frames, resize clearing,
controller input mapping, blocking event delivery, 512x256/131,072-cell viewport
and 256-byte Slash-query bounds, alternate-screen/raw-mode restoration,
read-only missing-Ledger inspection, and non-terminal rejection before state
creation or stdout output. Provider URL input is separately bounded to 512
bytes before Draft growth; Provider Profile and credential-reference IDs are
bounded to 64 bytes. The first rendered mutation tests drive
`/config statusline preset` through menu selection, dry-run preview, user-scope
commit, disk reopen, explicit discard, validation recovery, and a competing
revision winner. A separate `/config statusline expansion` test proves the same
schema-metadata-driven choice interaction selects `compact`, previews, commits,
and survives Config Runtime reopen. Provider URL tests drive a field command into an existing
Profile selection, render the focused target, replace/reset bounded text,
recover from invalid URL validation, commit and reopen, retain a losing CAS
Draft, and explicitly discard it without overwriting the winner. Two earlier
full-loop tests map real Crossterm key events through VT rendering and reopen the
resulting Config files. A third full-loop test drives `/config provider add`
through the ID prompt, release-template choice, Tab field movement, opaque
credential reference, preview, commit, and reopen. It asserts the credential
reference is absent from terminal bytes. A recovery test retains an invalid ID,
blocks dirty quit, rejects a stale CAS preview, explicitly discards, and
preserves the winning Config. Controller tests also prove non-Provider `add`
routes do not enter a partial rendered prompt and that an existing Profile's
credential reference can be replaced without serialized readback or generic raw
credential mutation. A real-key loop now opens a Provider wizard, maps F5 to an
injected connection tester, renders success, and proves the Config bytes,
credential reference, and Ledger remain untouched. Recovery tests retain a valid
dirty Draft across a retryable failure, reset the observation after the next
edit, and reject a stale revision before invoking the tester. These tests assert that dirty
Escape/quit is blocked, a failed preview
is rendered without ending the loop, and a no-change commit creates no file.
Another real-key loop carries `/config provider remove` through the
section-filtered object selector and exact deletion confirmation, commits, and
reopens Config to prove the target is absent. Focused tests freeze the
confirmation layout and Enter/Escape mapping; cancellation preserves the
object, while a competing revision keeps the confirmation live, blocks dirty
quit, and requires explicit discard. A short-viewport regression keeps the
selected object visible before destructive activation. Core Config tests
continue to freeze target-layer and dangling-reference deletion rules.
A real-key loop now carries `/config model add` through its bounded ID prompt and
all nine fields, including reasoning effort, service tier, maximum output tokens,
context mode, favorite, and fallback list. It previews, commits, reopens, and
checks every optional value. A recovery test leaves the Draft live when model is
missing, repairs it, then proves a competing revision cannot be overwritten,
dirty quit is blocked, and explicit discard preserves the winner. Schema tests
pin every Model Preset interaction and parseable choice. These manual editor
tests do not prove live catalog refresh.
A real-key loop now opens `/model`, filters configured Presets, moves to the
Favorites group, opens the selected detail, redraws after resize, and proves
Config bytes and the absent Ledger remain unchanged. Projection tests separately
freeze explicit unknown Recent and live availability, release compatibility and
provenance detail, and credential-reference redaction. A second real-key flow
opens configured detail, stages `fast`, replaces it with `careful`, renders the
pending next-Turn ID, and proves Config plus Team and Tool Ledgers are unchanged
while the Runtime Ledger gains the selection Event. A third real-key flow opens
a compatible release detail, enters a bounded Preset ID, previews and commits
the prefilled user-scope Draft, manually refreshes, reopens the configured
Preset, and stages it for the current Agent. Core, CLI, and App Server tests pin
dry-run/no-write, commit/reopen, incompatible Profile, duplicate ID, unknown
catalog key, read-only scope, revision conflict, capacity recovery, credential
redaction, and zero Runtime/Team/Tool Ledger creation. Missing current-Agent
state still creates no files; incompatible release detail cannot start a Draft.
Core recovery tests prove stale Session rejection, replacement, shared read-only
inspection, and restart persistence. Product tests prove Config drift makes no
Provider call or Ledger write, then exact admission consumes one selection and
freezes the expected Provider Epoch. Headless integration proves automatic
pending-ID resolution plus credential-preflight and explicit-ID-conflict
failure preserve all three Ledger byte streams. These tests do not prove
Provider-backed catalog discovery, automatic starter updates, project/new-Agent
defaults, context-mode execution, or fallback execution.
A real-key loop now carries `/config stats-window add` through its bounded ID
prompt and start/end/days/time-zone fields, previews, commits, and reopens the
resolved window. Focused tests prove all four schema fields expose 512-byte text
interactions, TOML weekday arrays remain visibly buffered and dirty while
incomplete, invalid arrays cannot move focus, and dirty quit requires explicit
discard. Further tests repair an invalid equal-time window, then prove a stale
revision cannot overwrite the winner and leaves the Draft live. Existing-object
tests carry a Usage Window field command through the section selector, commit,
and reopen. These tests do not exercise the separate manual snapshot refresh.
A real-key loop now opens `/stats` over two Agent-scoped Turns with pinned named
Usage Windows and complete token/cache records. It selects the second durable
attempt, opens provider/model/outcome detail, then cycles through Turn,
Provider & Model, Dialect & Policy, current Thread, Agent, Team, Named Window,
and Token & Cache groups and opens each detail across deterministic resizes.
Exact layout assertions pin requested/observed and unknown distribution buckets
plus cache-read, cache-write, and reasoning-token quantities. Empty and
unavailable snapshots clamp stale selection, close detail, render explicit
empty state, and create no Ledger. Runtime, Team, and Tool Ledger bytes remain
unchanged. These tests do not prove richer cache distributions or background
refresh.
A real-key loop now opens `/agent` over a valid Runtime plus Team and Tool
sidecars, selects a Dormant child Agent, opens bounded detail, redraws after
resize, and closes detail before returning to the Slash Panel. The fixture adds
an incomplete three-byte Team frame and proves the UI reports recovery required
while Runtime, Team, Tool, and Config bytes remain identical. Task titles,
capability labels, and scope labels are absent from VT output. Core tests
separately prove shared-lock
Team inspection never creates missing state, never repairs a torn final frame,
and does not rewrite checksum corruption; Product tests prove missing and
incomplete sidecar handling. These tests do not prove lifecycle mutation,
Team-operation acknowledgement, Workspace coordination, or
real ConPTY behavior.
A read-only real-key `/blockers` loop projects a pending Tool approval from the
three Product Ledgers, keeps canonical arguments out of the list, survives
resize, renders the Provider/credential/usage/cost/quota warning, and proves the
first Enter does not start recovery while Runtime, Team, Tool, and Config bytes
remain unchanged. Separate
action loops recover an exact in-memory approval view, render its identity,
canonical arguments, and filesystem/process/network resources, traverse every
detail row, and route Approve or Deny only from the final choices. They prove
denial has no acknowledgement, Escape drops the in-memory approval while all
Ledgers remain unchanged, resizing preserves an already selected Approve or
Deny action, failed resolution returns to blocker inspection and
requires a fresh recovery, and failed Provider-output acknowledgement keeps the
same delivery live for retry. ProductDriver tests independently use a rebound
Active Agent Session after reopen, compare the exact recovered arguments and
resources, execute an approved effect once, execute a denied effect zero times,
acknowledge prepared output, and reopen the Runtime as ready. The VT action tests
inject their Product action adapter; they do not prove live remote Provider
availability, a general Tool policy UI, App Server approval, or real ConPTY.
A second set of real-key loops proves manual snapshot refresh. One loop adds a
configured Preset and selected model externally, then verifies `/model` and the
statusline update without credential disclosure or Config/Ledger writes. Another
completes an external Agent-scoped Turn and verifies `/stats` receives its
Attempt and token/cache detail while Runtime, Team, and Tool Ledger bytes remain
unchanged. The Agent loop adds a child, temporarily removes one Product sidecar
to force a failed refresh, opens the old child detail, restores state, adds a
second child, and refreshes successfully. The final Config and all three Ledger
byte streams match the external writer exactly. Input tests pin F6 and Ctrl-R,
read-only-view scoping, the fixed failure notice, candidate Config isolation,
and selection clamping after a refreshed dataset shrinks. Ledger reads are
independent; these tests do not prove a cross-Ledger transactional snapshot,
background polling, Provider catalog discovery, Agent mutation, or real ConPTY.
A real-key loop now carries `/config pricing add` through its bounded ID prompt,
all 17 schema fields, manual provenance choice, preview, commit, and Config
reopen. Schema tests pin the 64-byte Provider Profile input, 512-byte text and
integer inputs, three optional dialect choices, and manual-only editable source.
A recovery test rejects an invalid context range without consuming the Draft,
repairs it, then proves a stale revision cannot overwrite the winner, dirty quit
is blocked, explicit discard preserves the winner, and Config reopen contains no
losing schedule. These tests do not prove live pricing-book refresh,
provider-reported charge ingestion, or rich terminal cost presentation.
Additional Config tests assert every schema field has a rendered interaction,
edit top-level Provider/model/output-limit values, edit every statusline field,
and route existing Model Preset, Price Schedule, and Usage Window fields through
their object selectors before commit and reopen. A real-key Provider test edits
all non-secret Profile fields and reopens the normalized snapshot; a recovery
test proves insecure-loopback permission is rejected for a remote HTTPS origin
without consuming the Draft. They do not constitute real ConPTY, panic-abort
cleanup, secret-store UI, approval, background refresh, or resource evidence.
App Server integration tests launch the product over piped standard input and
output. They stream schema and effective reads, reject credential-reference
reads, recover after malformed and oversized frames, and prove read-only calls
create no Config files. Further flows keep typed Draft changes isolated from
effective Config, recover the same handle after wrong-type and cross-field
validation failures, compare normalized preview and commit diffs, consume a
successful handle, and reopen the written Config. A two-process test starts from
one base revision, commits one winner, rejects the stale writer, proves the
losing Draft remains editable, refreshes the losing connection to the winner,
begins a new Draft at that revision, and compares winning bytes before and after
the conflict. A no-change commit consumes its handle without creating or
rewriting Config. A malformed startup Config keeps its exact bytes and makes
`config.get` return `repair_required`; the same stream can then begin a Draft,
reset the invalid field, validate, commit, and read the repaired ready value.
The 65th active Draft is rejected and capacity returns after one handle is
consumed. Cross-platform in-memory tests drive App Server credential
bind/duplicate-bind/replace, origin-isolated availability, idempotent forget,
duplicate-secret-key rejection, explicit loopback permission, invalid
scope/secret recovery, and fixed no-readback responses. The product
stdio test proves macOS and Linux return `credential_unavailable` for all four
operations without writing Config; its Windows branch performs the same flow
against the current user's Credential Manager, including duplicate bind,
replace, forget, and final not-found status. Tests assert secret bytes never
appear in stdout or stderr. They do not prove remote network transport,
multi-client authentication, non-Windows
credential backends, OS-level memory locking, or long-lived resource behavior.
Additional real-process JSONL tests pin the App Server's fixed Ledger path and
four read-only operational flows. Missing `runtime.status`, `agent.list`, and
`tool.status` state returns ready/empty projections without creating files.
Two completed Turns prove `runtime.stats` summary and revision/as-of-bound Cursor
paging while the Runtime Ledger remains byte-identical. A persisted Team proves
Agent identity/status/budget projection while Task titles, message bodies,
capability labels, and scope labels remain absent. An awaiting Tool approval
proves only Call/Agent/Tool/status/expiry/digest fields cross the wire while call
identity, arguments, resources, and Task metadata remain absent. Runtime, Team,
and Tool Ledger bytes are compared before and after inspection. A one-sided
product sidecar returns a fixed error, preserves its bytes, creates no missing
files, and the next request on the same stream succeeds. These tests do not
prove a cross-Ledger transactional snapshot.

Bounded App Server control tests cover the fixed recovery flow.
`runtime.cancel` rejects missing, wrong, non-Provider, and incomplete-tail state
without mutation, closes one exact Provider-origin block, remains idempotent,
and changes only the Runtime Ledger when product sidecars are present.
`runtime.retry` rejects non-retryable and wrong Turns, then durably rearms the
exact early Provider failure without constructing a Provider, resolving a
credential, executing a Tool, or changing Team/Tool bytes. `runtime.resume`
requires that exact `resume-required` Turn, reconstructs the frozen Provider and
product Session, and returns prepared output without acknowledging it. Ordinary
and product tests recover that output through `runtime.delivery`, acknowledge
it, and reopen Ready. A frozen external Provider with an unavailable credential
returns a fixed error without exposing its reference, origin, or input and
leaves the retry transaction recoverable. Runtime and Team incomplete-tail
fixtures prove resume does not repair or change any Ledger.
`runtime.delivery` retrieves exact persisted Assistant text for the matching
prepared delivery without changing Runtime bytes, rejects missing or wrong
deliveries, and rejects an incomplete tail without repair.
`runtime.acknowledge` rejects missing, wrong, and incomplete-tail state before a
write, closes the exact delivery, remains idempotent on repeat, and reopens
Ready. Tool reconciliation rejects invalid digests and unknown calls without a
write, records observed success/failure without an executor, preserves Runtime
and Team bytes, remains terminally idempotent, and refuses an incomplete Tool
tail without repair. Core Ledger tests also prove the strict existing-writer open
rejects a tail under its exclusive lock and leaves exact bytes for explicit
writer recovery. A loopback Responses fixture interrupts before the original
decision, then proves direct `tool.decide` approval is rejected until the same
stream reviews exact canonical arguments/resources and receives their hashes.
Wrong confirmation hashes fail without a Provider or executor call; correct
confirmation reconstructs the frozen Provider again and revalidates the binding
under the recovered Active Agent Session. Approval executes fixed `local.echo`
exactly once, returns output that
`runtime.delivery` can recover, and reaches Ready only after explicit
acknowledgement; denial executes zero effects and remains repeat-safe. Public
errors omit secret, call identity, Provider details, and private failure reasons;
only the explicit review result exposes the exact Tool material being approved.
A clean zero-session Team cannot be auto-admitted by a failed review, and all
three Ledgers remain byte-identical. These tests do not prove remote transport,
general Runtime control beyond the exact cancel/retry/resume flow, Agent
lifecycle mutation, arbitrary Tool
approval/execution, or multi-client authentication.
Context Pressure tests freeze exact 65%/90% threshold transitions, estimated and
missing-fact propagation, invalid policy/limit and arithmetic failure, and the
no-side-effect hard admission gate. Product presentation tests assert estimated
occupancy renders with `~` while an unavailable pressure fact remains unknown.
These tests do not constitute Artifact offload, compaction, checkpoint,
provider-native context adaptation, or P6 resource evidence.
Provider-wizard tests derive a normalized Profile from an uncommitted multi-field
Draft, keep credential references out of serialized screens, leave Config
untouched during the connection check, invalidate a prior result after any staged
change, and reject an observed stale revision before invoking the tester. CLI
tests prove `config test-provider` returns only a fixed pre-network failure status
when the selected Profile's credential is unavailable. Provider Catalog tests
freeze the release schema and revision, sorted unique records, template/model
referential integrity, exact dialect mappings, field provenance, and explicit
unknown context/capability/price/availability facts. Config tests prove official
defaults resolve before user overrides, custom DeepSeek origins default to a
distinct mirror of the reviewed bundled rate card, explicit pricing choices
override that mirror, templates without a bundled card still require a pricing
decision, and release models bind only under template-enabled catalog modes. The
read-only `config catalog` integration test proves the snapshot contains no
local path or credential reference.

Remaining terminal-backend evidence covers real terminal/ConPTY resize and input,
project/new-Agent Preset defaults, richer cache distributions, Agent Center
lifecycle actions, background refresh, broader multi-Tool/App Server approval,
panic-abort cleanup, final host cell
geometry, and input-ready/idle resource budgets.

Tests assert that `/config pro url` resolves through Provider selection to the
focused rendered editor without registering a flat root command. Every Config
Object must have a generic or purpose-built editor route. Credential fields must
never support read-back.

## Provider Testing

Deterministic local simulators are the required gate. The end-state simulator
suite supports success, slow streams, malformed events, disconnects, resumable
and non-resumable partial output, reordered tool fragments, missing usage,
unknown usage fields, and retryable/fatal errors. The current Phase 1
simulator implements deterministic bounded success; module tests inject
malformed completion and process interruption after admission. The OpenAI
Responses decoder has redacted fixtures for text, function calls, optional
usage, failed, incomplete, and error terminals. A DeepSeek Responses fixture
also proves bounded reasoning items are validated but not normalized into
visible output. The separate Chat Completions
decoder has redacted fixtures for fragmented text, one fragmented function call,
usage-only completion, incomplete termination, and Tool continuation.
The Anthropic Messages decoder has redacted fixtures for ordered message and
content-block transitions, fragmented text and one `tool_use`, cumulative
usage, incomplete/error terminals, bounds, redaction, and Tool continuation.
Fixture Providers pass normalized events through the Kernel and Tool Runtime
for one approved call and continuation. HTTP interruption fixtures now classify
zero-event and post-event EOF separately without reconnecting or retrying. The
remaining scenarios land with partial-stream reconnect, automatic retry policy, multiple
tools, broader delta kinds, Messages reasoning blocks, and broader usage
normalization.

The product connection-test adapter has deterministic loopback coverage for one
explicit GET to the frozen Profile's `models` route. Tests validate the synthetic
Authorization header server-side, prove a missing origin-bound credential causes
no request, classify HTTP 503 as retryable without exposing its private body,
and keep endpoint and credential data out of the serialized result. Success
tests parse both exact release-catalog and unknown IDs, ignore remote capability
and endpoint fields, and prove the unknown OpenCode Go model is still rejected
before credential lookup or network I/O. Rejection tests cover wrong content
type, malformed JSON, a body over 256 KiB, more than 1,024 models, duplicate IDs,
whitespace in IDs, and IDs over 256 bytes. A truncated 2xx response body is
classified as retryable unavailability rather than a provider-format error.
This is a configuration check plus ephemeral model-list observation, not a
persistent catalog-discovery merge, selector refresh, or live inference
conformance test.

The official OpenAI template identity is also exercised with an explicit
loopback origin override through the Responses adapter and models probe. The
test proves inherited routes and dialects cross the same endpoint and credential
gates as an explicit compatible gateway. The Responses adapter admits OpenAI
and explicit openai-compatible identities, plus the exact DeepSeek/V4-Flash
pair and exact OpenCode Go/GPT-5.6 Luna pair. OpenCode Go Responses tests assert
the frozen `/responses` route, Bearer request shape, bounded SSE normalization,
catalog and policy rejection before network I/O, Provider Epoch reconstruction,
and one Tool continuation through durable approval. The Chat Completions adapter admits the OpenAI and explicit
openai-compatible template identities plus the exact
official DeepSeek identity with its DeepSeek-specific request policy. The
Chat adapter also admits exact release-catalog OpenCode Go Chat model/dialect
pairs with the OpenCode-specific request policy. The
Messages adapter admits only the official DeepSeek template identity and its
explicit Messages route. V4 Pro resolves a preferred Responses dialect to Chat
before admission; this bounded capability resolution is not a network retry or
general Preset fallback. OpenCode Go Messages remains outside this
slice; a declared route or dialect is not treated as proof of wire compatibility.

Live provider inference tests are not implemented yet. Planned opt-in,
credential-gated tests will verify OpenAI, DeepSeek, and OpenCode Go without
running on untrusted pull requests or gating local performance. Future
model-catalog refresh and persistent discovery work records the source URL and
observation date; the current release seed already freezes both.

## Performance Testing

All performance work follows the [Performance Contract](performance-contract.md). Microbenchmarks may explain a result but cannot approve a release.

- Developer machines: quick local regressions and allocation profiles
- GitHub-hosted Windows: correctness and coarse smoke measurements
- FMDev: fixed 30-run p50/p95 regression suites, warning above 5%, block above 10%
- Target Machine: signed asynchronous Acceptance Run for absolute release evidence

Results include main process, TUI, per-Agent increment, child processes, and total process tree. Compiler and user-command cost remains visible but separately attributed.

## CI Matrix

This is the required end-state matrix for a release candidate. The current
bootstrap workflow implements formatting, checks, tests, lints, release
packaging, the x86-64-v3 guard, an acceptance-harness smoke, a one-sample
benchmark-pipeline smoke, isolated compile/test coverage for eight storage
workloads, the terminal render matrix, the HTTP loopback transport matrix, and
three process-global allocator runners, plus real same-binary cross-process
CAS, migration interruption, child termination/restart integration tests, one
prepared-Tool-effect product process-death/reconciliation test, the 18-case
executor-return/outcome-append core process-death matrix, and synthetic SQLite
WAL VFS fault recovery on macOS ARM and Windows. Remaining
Runtime/tool-effect crash, security, terminal process/input/resource, transport
TLS/proxy-auth/fuzz/resource, allocator P0-P6 resource, and general fuzz jobs
are added with the slices that make them executable.

| Environment | Required evidence |
| --- | --- |
| Windows 11 x64 | Build, unit/property/golden tests, integration, crash recovery, fuzz smoke, packaging, x86-64-v3 build-flag and startup feature-guard verification |
| FMDev | Windows suite plus repeatable performance regression package |
| macOS ARM | Build, formatting/lints, pure core tests, provider fixture tests |
| Linux x64 | Build and pure core tests where supported; not a v1 product gate |

Sanitizer, loom-style concurrency exploration, or equivalent tools should run on a supported CI host when they add evidence unavailable on Windows. Platform-specific behavior still requires Windows tests.

Windows CI executes the release-built acceptance binary's x86-64-v3 feature guard, a three-sample acceptance smoke, and a one-sample benchmark-pipeline smoke. It separately builds and tests the optional storage-, terminal-, transport-, and allocator-candidate runners. Storage smoke covers `critical-append-replay`, `timer-expiry-streaming-replay`, `cas-one-winner`, `interrupted-migration`, and `cross-process-crash-replay` for SQLite WAL and append log, plus SQLite-only `sqlite-vfs-fault-recovery`; terminal smoke covers `render-matrix` for direct VT and Ratatui/Crossterm; transport smoke covers HTTP `loopback-sse` for native WinHTTP through Wrest and Reqwest/Rustls; allocator smoke covers `allocation-pressure` in distinct system, Mimalloc, and Snmalloc executables and requires a shared correctness digest. Candidate binaries remain outside the portable product package. FMDev and Target evidence still use at least 30 measured runs for every required named workload; CI smoke samples are never treated as performance evidence.

## Release Gate

A release candidate is acceptable only when:

1. Windows correctness, recovery, security, migration, and packaging suites pass.
2. Protocol fixtures cover every enabled Provider Dialect and required capability.
3. FMDev passes the provisional numeric budgets and relative performance gates.
4. The asynchronous Target Machine package passes, or the absence of a run is explicitly accepted and documented.
5. On-disk migrations were tested from every supported released schema.
6. No known failure can duplicate a successful or ambiguous external effect.

The Windows artifact must self-report the same x86-64-v3 baseline recorded by its build manifest, and CI must execute its CPU-feature guard. The Target ZIP must match the candidate ID, source revision, executable hash, workload version, and configuration hash in the release evidence record. A Target waiver is approved by the Release Owner, is scoped to one executable hash, and permits only a pre-Alpha candidate; it never replaces the passing Target run required for Alpha.

## Test Data Rules

Fixtures use synthetic repositories, prompts, credentials, paths, and responses. Real user content and production keys never enter source control or CI artifacts. Failure bundles apply the same redaction policy as Diagnostic Bundles.
