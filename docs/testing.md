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

The current Phase 1 spine covers a strict subset of these end-state contracts:
versioned Config TOML, schema/path/type validation, precedence/provenance,
dry-run drafts, revision conflicts, atomic writes, backup repair, last-valid
external-edit behavior, symlink rejection, and immutable Runtime epochs;
canonical ID and Item bounds;
Ledger append/sync/replay, single-writer exclusion, expected-Head conflict,
torn-tail read-only inspection and writer repair, checksum/length/schema
tampering, and symlink rejection; deterministic Provider success and malformed
output; Runtime admission resume, prepared-output reconciliation, idempotent
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
acknowledgement is exposed as pending and blocks later Team commands. Product
Provider/Tool driving, receipt delivery to a user-visible sink, and the final
product reconciliation presentation remain pending. Migration/backup remains
in the candidate storage harness rather than this provisional product adapter.

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

Golden terminal tests cover root Slash Panel size, hierarchical Command Path completion, scoped fuzzy matching, Config Center navigation, narrow-width statusline degradation, expanded status details, model selector states, approval/blocker visibility, and text-fit constraints.

Tests assert that `/config pro url` resolves to the focused Provider editor without registering a flat root command. Every Config Object must have a generic or purpose-built editor route. Credential fields must never support read-back.

## Provider Testing

Deterministic local simulators are the required gate. The end-state simulator
suite supports success, slow streams, malformed events, disconnects, resumable
and non-resumable partial output, reordered tool fragments, missing usage,
unknown usage fields, and retryable/fatal errors. The current Phase 1
simulator implements deterministic bounded success; module tests inject
malformed completion and process interruption after admission. The remaining
scenarios land with reconnect, tools, and usage normalization.

Live provider tests are opt-in and credential-gated. They verify current integration for OpenAI, DeepSeek, and OpenCode Go but do not run on untrusted pull requests and do not gate local performance. Model-catalog refresh work records the source URL and observation date.

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
