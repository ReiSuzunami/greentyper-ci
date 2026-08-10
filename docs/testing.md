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
- Provider Runtime: capability freeze, dialect/transport fallback, epoch changes, retry classification, and usage normalization
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
reopens in a reconciliation-required state. The configured Responses adapter
now binds typed, frozen Provider Profile and route metadata to origin-scoped
credential lookup. Live-provider validation, non-Windows credential backends,
configurable proxy policy, broader TLS platform evidence, broader canonical
Runtime Items, reasoning/refusal/annotation and other unimplemented Responses
event kinds, reconnect/retry fixtures, MCP adapters, richer TUI/App Server Tool
presentation, the cross-process Tool byte-offset matrix, and final
reconciliation presentation remain pending. The fixed CLI path now flushes the
Team receipt and exact approval event before acknowledgement.
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

The first fixture Provider/Tool tracer bullet decodes and normalizes one
Responses function call, requires a current Session and exact Tool authority,
durably crosses approval and `EffectPrepared`, invokes one injected executor,
continues the Provider once, and replays the acknowledged canonical output plus
two Usage Records. Companion tests prove stale Sessions invoke no Provider,
ambiguous effects never reach continuation, non-UTF-8 Tool output is blocked,
and process death after a durable Tool success cannot repeat the effect.
Migration tests replay a historical schema-1 Ledger before appending schema-4
events, decode historical schema-2 and schema-3 Provider Epoch shapes
separately, and round-trip schema-4 Provider Profile, dialect, Config Usage
Window, and Usage Attempt data while rejecting fingerprint, outcome, timestamp,
and transition tampering.

Product integration tests also run the configured Responses adapter against a
concrete loopback HTTP tracer. They resolve and freeze the fixture Provider
Profile through Config Runtime, then validate its POST route, model, input,
streaming flag, and synthetic Authorization header; stream a fragmented SSE
fixture through the core decoder and Runtime; replay the canonical assistant
item; classify HTTP 503 and request timeout without exposing an upstream
private marker; and reject unsafe endpoints. Module tests verify a locally
trusted HTTPS root, reject an untrusted certificate, enforce origin-bound
credential lookup before network access, and cover status/endpoint policy.
Another module test validates the exact two-request Tool protocol: advertised
`local_echo`, streamed call, canonical `local.echo` mapping, correlated
`function_call_output`, previous response ID, final text, and two Usage Records.
Windows-only tests exercise Credential Manager bind, replace, resolve, and
forget. This does not cover live credentials, proxy authentication,
reconnect/retry, live Providers, or broader Tool presentation.

The first Usage projection suite durably records Provider request and
continuation attempts, closes interrupted attempts only on explicit resume,
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

Current pure tests freeze the four root Command Paths, nested Config routes,
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
not constitute an ANSI/VT backend, live terminal input, or ConPTY terminal claim.
Provider-wizard tests derive a normalized Profile from an uncommitted multi-field
Draft, keep credential references out of serialized screens, leave Config
untouched during the connection check, invalidate a prior result after any staged
change, and reject an observed stale revision before invoking the tester. CLI
tests prove `config test-provider` returns only a fixed pre-network failure status
when the selected Profile's credential is unavailable. Provider Catalog tests
freeze the release schema and revision, sorted unique records, template/model
referential integrity, exact dialect mappings, field provenance, and explicit
unknown context/capability/price/availability facts. Config tests prove official
defaults resolve before user overrides, custom origins cannot inherit template
pricing, and release models bind only under template-enabled catalog modes. The
read-only `config catalog` integration test proves the snapshot contains no
local path or credential reference.

Remaining terminal-backend goldens cover real resize and stale-cell clearing,
hierarchical input events, rendered Config dialogs, model selector states,
approval/blocker visibility, styles, and final cell geometry.

Tests assert that `/config pro url` resolves to the focused Provider editor without registering a flat root command. Every Config Object must have a generic or purpose-built editor route. Credential fields must never support read-back.

## Provider Testing

Deterministic local simulators are the required gate. The end-state simulator
suite supports success, slow streams, malformed events, disconnects, resumable
and non-resumable partial output, reordered tool fragments, missing usage,
unknown usage fields, and retryable/fatal errors. The current Phase 1
simulator implements deterministic bounded success; module tests inject
malformed completion and process interruption after admission. The OpenAI
Responses decoder has redacted fixtures for text, function calls, optional
usage, failed, incomplete, and error terminals. One fixture Provider now passes
its normalized events through the Kernel and Tool Runtime for a single approved
call and continuation. The remaining scenarios land with concrete transport,
reconnect, multiple tools, and broader usage normalization.

The product connection-test adapter has deterministic loopback coverage for one
explicit GET to the frozen Profile's `models` route. Tests validate the synthetic
Authorization header server-side, prove a missing origin-bound credential causes
no request, classify HTTP 503 as retryable without exposing its private body, and
keep endpoint and credential data out of the serialized result.
This is a configuration/status probe, not model discovery or live-provider
validation.

The official OpenAI template identity is also exercised with an explicit
loopback origin override through the current Responses adapter and models probe.
The test proves inherited routes and dialects cross the same endpoint and
credential gates as an explicit compatible gateway. DeepSeek and OpenCode Go
Chat Completions/Messages execution remains outside this slice.

Live provider integration tests are not implemented yet. Planned opt-in,
credential-gated tests will verify OpenAI, DeepSeek, and OpenCode Go without
running on untrusted pull requests or gating local performance. Future
model-catalog refresh and discovery work records the source URL and observation
date; the current release seed already freezes both.

## Performance Testing

All performance work follows the [Performance Contract](performance-contract.md). Microbenchmarks may explain a result but cannot approve a release.

- Developer machines: quick local regressions and allocation profiles
- GitHub-hosted Windows: correctness and coarse smoke measurements
- FMDev: fixed 30-run p50/p95 regression suites, warning above 5%, block above 10%
- Target Machine: signed asynchronous Acceptance Run for absolute release evidence

Results include main process, TUI, per-Agent increment, child processes, and total process tree. Compiler and user-command cost remains visible but separately attributed.

## CI Matrix

This is the required end-state matrix for a release candidate. The current bootstrap workflow implements formatting, checks, tests, lints, release packaging, the x86-64-v3 guard, an acceptance-harness smoke, a one-sample benchmark-pipeline smoke, isolated compile/test coverage for seven storage workloads, the terminal render matrix, the HTTP loopback transport matrix, and three process-global allocator runners, plus a real same-binary child termination/restart integration test on macOS ARM and Windows. Broader Runtime/tool-effect crash, security, cross-process CAS/migration, terminal process/input/resource, transport TLS/proxy-auth/fuzz/resource, allocator P0-P6 resource, and general fuzz jobs are added with the slices that make them executable.

| Environment | Required evidence |
| --- | --- |
| Windows 11 x64 | Build, unit/property/golden tests, integration, crash recovery, fuzz smoke, packaging, x86-64-v3 build-flag and startup feature-guard verification |
| FMDev | Windows suite plus repeatable performance regression package |
| macOS ARM | Build, formatting/lints, pure core tests, provider fixture tests |
| Linux x64 | Build and pure core tests where supported; not a v1 product gate |

Sanitizer, loom-style concurrency exploration, or equivalent tools should run on a supported CI host when they add evidence unavailable on Windows. Platform-specific behavior still requires Windows tests.

Windows CI executes the release-built acceptance binary's x86-64-v3 feature guard, a three-sample acceptance smoke, and a one-sample benchmark-pipeline smoke. It separately builds and tests the optional storage-, terminal-, transport-, and allocator-candidate runners. Storage smoke covers `critical-append-replay`, `timer-expiry-streaming-replay`, and `cross-process-crash-replay` for SQLite WAL and append log; terminal smoke covers `render-matrix` for direct VT and Ratatui/Crossterm; transport smoke covers HTTP `loopback-sse` for native WinHTTP through Wrest and Reqwest/Rustls; allocator smoke covers `allocation-pressure` in distinct system, Mimalloc, and Snmalloc executables and requires a shared correctness digest. Candidate binaries remain outside the portable product package. FMDev and Target evidence still use at least 30 measured runs for every required named workload; CI smoke samples are never treated as performance evidence.

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
