# Implementation Plan

## Delivery Rule

Implementation proceeds as runnable vertical slices. Each phase must preserve Ledger recovery, authority boundaries, and measured resource behavior; no phase may defer all testing or performance work to the end.

Feature implementation was authorized on 2026-08-09. The first core slice fixes the Agent Team command/event interface, process-local Agent Sessions, and pure orchestration policy early; it does not claim Phase 7 completion. Plain `TeamRuntime` remains volatile. A separate durable Team adapter proves synchronous persistence, and the core Runtime Kernel now owns it, gates root admission, persists operation identity and acknowledgement records in the same Team Ledger, and rebinds the complete non-terminal Session set after recovery. The first Phase 2 slices persist Tool call identity, Approval Grant binding, prepared-effect state, terminal digests, and explicit ambiguous-effect reconciliation, and add bounded generic SSE framing plus a strict OpenAI Responses streaming decoder. Configured product Provider driving and one explicit `local.echo` approval/delivery path are now present; broader Tool catalogs and presentation remain pending.

## Phase 0: Repository and Measurement Foundation

Create the Cargo workspace, Windows-first build profiles, formatting/lint policy, GitHub CI, schema/version conventions, deterministic fixture harness, benchmark harness, and portable packaging skeleton.

Current implementation includes the workspace, CI, schema convention, one embedded deterministic Agent Team fixture, versioned acceptance and benchmark evidence, an x86-64-v3 runtime guard, a portable ZIP skeleton, an isolated seven-workload SQLite WAL versus checksummed append-log matrix, an isolated direct VT versus Ratatui/Crossterm render matrix, an isolated HTTP loopback SSE matrix for Reqwest/Rustls and native WinHTTP through Wrest, and three separately built system/Mimalloc/Snmalloc allocator runners. The storage matrix covers critical sync/replay, max-event and deterministic 250ms-expiry streaming batches, one-winner in-process CAS, backup/restore, in-process old-or-new migration recovery, and critical cross-process termination/restart with known-not-repeated or ambiguous-blocked outcomes. The terminal matrix verifies 27 ANSI-replayed frames, live resize, clearing stale content, styles, Unicode width, and zero-byte model-equality no-ops across three viewport sizes. The transport matrix verifies seven cold/warm, error, timeout, cancellation, explicit-proxy, and custom-origin cases with split UTF-8/line framing and credential non-leakage. The allocator matrix currently proves compile-time global-allocator isolation and identical deterministic allocation-pressure results; it does not yet prove the named P0-P6 resource workloads. Phase 0 still requires cross-process versions of storage CAS and migration, SQLite VFS fault injection, fuzzing, all Runtime durability boundaries, terminal Windows/ConPTY and resource evidence, transport HTTPS/TLS/proxy-auth/Windows runtime/resource evidence, allocator cold-start/idle/streaming/Agent/context-pressure resource evidence, complete same-commit 30-run FMDev comparisons, and recorded technology choices.

Benchmark WinHTTP versus a cross-platform HTTP stack, direct VT versus a TUI library, SQLite WAL versus a custom append log, and allocator options using minimal representative workloads.

Exit criteria:

- Windows x64 build and test run from a clean checkout.
- macOS ARM builds core modules and runs pure tests.
- FMDev benchmark harness produces versioned raw results.
- Storage, terminal, transport, and allocator choices are recorded from evidence.

## Phase 1: Recoverable Single-Agent Spine

Implement Config Runtime basics, canonical Thread/Turn/Item/Event types, Ledger Store append/replay, Runtime Kernel admission, one logical Agent, deterministic provider simulator, and headless output.

Current implementation includes provider-neutral Thread/Turn/Item identities,
immutable bootstrap Config and Provider Epochs, a synchronously durable
checksummed file Ledger with exclusive writer locking and read-only inspection,
canonical Runtime Event replay, explicit `resume` and output `reconcile`
states, a versioned deterministic Provider fixture, and a headless CLI that
flushes output before durably acknowledging it. Core and cross-process product
tests prove admission recovery, prepared-but-unacknowledged output blocking,
idempotent acknowledgement, malformed Provider blocking, corruption failure,
unsupported format failure, torn-tail reporting/repair, and writer exclusion.

The Agent Team now also has a standalone `DurableTeamRuntime` adapter with a
dedicated Ledger, versioned bounded encoding for all Team Event kinds,
synchronous checksum-bound receipts, exclusive writer ownership, complete-
prefix recovery, stale-session rejection, and fail-closed checksum/schema/state
replay. End-to-end tests persist and recover Delegation, messaging, failure and
Blocked propagation, cancellation, Completion Capsules, and terminal states.
`RuntimeKernel::open_with_team` owns the dedicated writer and issues one
consumable complete non-terminal Session bundle per validated open. It excludes
terminal Agents and exposes no Agent-ID-to-Session conversion. Root admission
uses the same typed Kernel dispatch interface; duplicate admission fails without
mutation. Private fault tests inject write/flush/sync failures at eight command
frame boundaries and another eight acknowledgement frame boundaries, then
terminate authenticated child processes at six representative
points from before write through sync-before-publish. Recovery accepts only a
known complete prefix or an already-complete transaction. I/O failure poisons
the live writer; process termination leaves no writer to continue. Neither path
retries automatically. Kernel commands persist a sequential `TeamOperationId`
marker in the same transaction as their domain Events. Until an explicit
acknowledgement transaction is durable, the recovered operation remains pending
and later Team commands are blocked. Duplicate acknowledgement is idempotent;
operation IDs remain non-authorizing inspection identities.

The Config Runtime now adds versioned user/project TOML, addressable Provider
Profile/Model Preset/Price Schedule/statusline/Usage Window fields, effective provenance,
typed single-operation drafts, dry-run, revision conflict detection, atomic
replacement, backup repair, and last-valid behavior for invalid external
edits. It is wired into new headless Turn admission through the immutable
bootstrap projection.

This does not complete Phase 1. Complete schema default/constraint/migration
metadata and rendered generated editor surfaces, non-Windows credential
storage, the exhaustive byte-offset Runtime/effect crash-fault matrix, storage
migration, and headless FMDev/Target idle resource evidence remain pending. See
[Recoverable Single-Agent Runtime](runtime-kernel.md).

Exit criteria:

- One Turn survives crash and replay without duplicate output acknowledgement.
- Config layers resolve into an immutable Config Epoch.
- Ledger corruption and unsupported schema fail explicitly.
- Headless idle memory and CPU are measured against the contract.

## Phase 2: Provider and Tool Tracer Bullet

Add OpenAI Responses SSE, canonical tool calls, Tool Ledger identities, Approval Grants, one local process tool, Windows process control, usage normalization, and provider retry/recovery behavior.

The first core Tool Runtime slice is implemented behind
`RuntimeKernel::open_with_team_and_tools`. It uses a dedicated locked Ledger,
canonical JSON argument hashing, monotonic call IDs, Agent-session-bound
approval requests, exact Tool/resource binding, independent filesystem,
process, and network Capability checks, and a durable `CallRequested ->
ApprovalGranted + EffectPrepared -> terminal outcome` fold. Raw arguments and
outputs are not persisted; only their hashes or digests are. Prepared effects
without a terminal outcome block new Tool, Team, and single-Agent execution
until explicit reconciliation. Tests prove restart deduplication, identity
conflict rejection, stale-session rejection, expiry denial, raw-argument and
external-reason exclusion, pre-effect durability failure with zero executor
calls, and post-effect outcome failure requiring reopen and explicit
reconciliation.

The first Provider dialect slice and two additional adapter slices are also
implemented. A reusable bounded SSE
framer handles LF, CRLF, lone CR, fragmented UTF-8, comments, multiline data,
and explicit byte limits. The OpenAI Responses decoder validates a documented
event subset, assembles streamed text and function arguments, preserves
optional cache/reasoning usage and service tier, rejects invalid state
transitions, and redacts Debug output. Its output remains dialect-scoped data,
not Runtime state or Tool authority. Redacted fixture tests cover success,
failure, incomplete, and error terminals.
The separate Chat Completions decoder validates one choice, streamed content,
one fragmented function call, usage-only completion, fixed incomplete states,
and `[DONE]`; it shares only the bounded SSE framer and provider-neutral output
contract with Responses.
The Anthropic Messages decoder validates the ordered message/content-block
protocol, bounded text and one fragmented `tool_use`, cumulative usage, explicit
incomplete/error terminals, and `message_stop`. It preserves unknown usage
instead of inventing zeroes and maps only the supported facts into the same
provider-neutral contract.

A fixture tracer bullet now normalizes the supported Responses facts into
provider-neutral text, one canonical function call, and optional Usage Records.
The Kernel authenticates a current Agent Session, routes that call through the
existing durable Tool approval/effect state machine, feeds one successful UTF-8
result into a Provider continuation, and durably prepares combined canonical
output. Recovery tests prove stale Sessions cannot invoke the Provider,
ambiguous effects cannot continue, and process death after a durable Tool
success blocks rather than repeating the effect. Runtime Event schema 6 stores
durable Usage Attempt boundaries, frozen Usage Windows, Provider dialect, the
expanded optional Usage Records, frozen Provider Profile snapshot, and the
subsequent frozen Price Schedule cost evaluation plus an optional selected-Preset
output-token limit while replaying historical schema 1 through schema 5.

A product-private `local.echo` tracer now exercises the concrete process seam.
It launches a fixed same-binary child without a shell, clears inherited
environment and working-directory state, bounds all three standard streams and
execution time, kills the Unix process group on abort, and on Windows creates
the process suspended before assigning a single-process, 128 MiB,
kill-on-close Job Object. The adapter rejects filesystem and network resources.
The product exposes it only through explicit `--tool local.echo` approval; it
is one narrow local process Tool, not a general process sandbox or
caller-selected command.

Three configured product adapters now exercise the concrete Responses, Chat
Completions, and Messages HTTP seams.
Config Runtime resolves and freezes its typed Provider Profile, origin,
selected-dialect route, dialect, pricing decision, and opaque credential
reference. Each adapter resolves a secret from the origin-bound product vault,
requires HTTPS for remote origins, disables proxy discovery and redirects,
sends a bounded canonical request, applies a fixed deadline, and streams the
response through its core decoder into the real single-Agent Runtime. The
DeepSeek Messages adapter uses `x-api-key`, pins the compatibility version,
disables unsupported reasoning blocks, and admits only the exact frozen
DeepSeek/Messages pair. Private loopback fixtures retain synthetic
authorization. Tests cover fragmented success, canonical replay, exact
dialect-specific request and one-Tool continuation bodies, HTTP failure-body
redaction, timeout, endpoint/status policy, trusted and untrusted TLS, and
missing credential failure before network access.

The remaining slices are still policy, protocol, and fault-adapter work:
live-provider validation, non-Windows credential backends, configurable proxy
policy, live catalog discovery, template-specific DeepSeek Responses/Chat
Completions and all OpenCode Go execution, Messages reasoning blocks, Preset
reasoning/service-tier/context/fallback execution, broader canonical Items, multiple
Tool calls,
durable resumable Tool result references, richer TUI/App Server
approval/delivery, caller-selected process policy, complete Windows Job
lifetime/resource evidence, reconnect/retry behavior, and the cross-process
Tool crash matrix remain pending.

Exit criteria:

- A fixture Responses, Chat Completions, or Messages Turn can call one approved
  Tool and finish canonically.
- Successful and ambiguous effects cannot auto-repeat after injected crashes.
- Credentials stay outside files and Ledgers.
- Provider raw events are diagnostic artifacts, not core state.

## Phase 3: TUI, Config Center, and Observability

Add VT/ConPTY TUI, hierarchical Command Paths, global command palette, Config Schema-driven editors, Provider wizard, model selector, adaptive statusline, Context Pressure, Usage Records/Rollups, `/stats`, and named Usage Windows.

The first observability slice is implemented. Runtime Event schema 6 durably
brackets each Provider request and continuation with an immutable Usage Attempt,
including UTC start/completion, outcome, Agent scope when present, frozen
Provider Profile/model/dialect, exact or estimated Usage Records, and explicit
unknown cost provenance. Recovery closes an interrupted attempt before an
explicit resume starts another. The Runtime Fold incrementally maintains Turn,
Thread, Agent, single-Team, rolling 1-hour/1-day/7-day, and versioned named-window
rollups; `greentyper stats` reads the replayed projection as JSON without
prompt text. Config Epochs freeze half-open/cross-midnight Usage Windows with a
concrete IANA identity and bundled rule-set version. Tests cover aggregation,
overflow-to-unknown, historical schema replay, repeated/skipped DST hours,
local-zone resolution, changed same-name windows, Product-driver Agent/Team
scope, and statistics redaction. Schema-owned Price Schedule Config Objects now
resolve into each Config Epoch. Every Usage completion is followed in the same
transaction by a frozen cost-evaluation event; replay recalculates its exact,
estimated, or explicit-unknown pay-as-you-go estimate from the frozen schedule
and rejects tampering. Cached rollups retain per-currency fixed-point totals and
unknown/overflow facts, while provider-reported charges and subscription quota
values remain distinct and unimplemented.

The terminal-neutral presentation and editor slices are also implemented. Config
Schema metadata now owns the bounded hierarchical Command Path registry and one
credential-safe editor route per field. Config Runtime exposes sorted existing
Config Objects, effective and target-layer field values with provenance, and
configured Model Presets without credential read-back. A product presentation
model derives a bounded Slash Panel, configured-preset search and favorites,
adaptive status facts, and explicit Runtime, Team, Tool, and Config blockers.
The Config Runtime can open the selected field route as one revision-bound
editor session, retain invalid staged values for correction, preview through the
real dry-run lock and validation path, reset, and atomically commit. Credential
routes expose binding status but refuse generic value mutation. Pure and
subprocess tests cover `/con`, `/config pro url`, focused Profile editing,
root/nested command separation, schema-route completeness, invalid-value
recovery, revision races, credential non-read-back, unknown facts, blocker
visibility, and a read-only smoke path. A terminal-neutral interaction controller
now routes the Slash Panel into existing-object Config Center sections, focused
revision-bound editors, the configured-preset selector, Stats, and Agent views.
It refuses to discard dirty drafts implicitly and retains the editor after
validation or revision-conflict failure. Its deterministic row layout freezes
Unicode-safe text fitting, prioritized compact status segments, a wide detail
row, and exact 40x12, 80x24, and 160x50 snapshots. No ANSI/VT backend, keyboard
event loop, rendered terminal dialog, ConPTY integration, or App Server surface
is claimed by these slices. Typed nested `add` and `remove` paths now open
revision-bound create/delete sessions for Provider Profiles, Model Presets, and
Usage Windows. Creation keeps one schema-driven Draft across focused fields;
deletion removes only a target-layer object, passes through reference validation,
and preserves backup and compare-and-swap behavior.
Provider Profile create/edit routes now enter a terminal-neutral purpose-built
wizard backed by the same Config Draft. It stages opaque credential references
only through the credential field, derives a validated frozen candidate Profile
without writing Config, and can explicitly test the configured `models` route.
The product adapter performs one bounded no-proxy/no-redirect GET, does not read
the response body, and returns only fixed redacted status categories. The same
adapter powers `greentyper config test-provider` for the selected committed
Profile. Tests cover success, missing credentials before network access, redacted
upstream 503 classification, candidate non-mutation, result invalidation after a
Draft change, and observed revision conflicts before a probe.
A release-bundled Provider Catalog now freezes schema-versioned OpenAI,
DeepSeek, and OpenCode Go template defaults and seed model facts with
field-level source references and observation time. Config Runtime applies
template defaults before explicit Profile overrides, refuses template pricing
inheritance for custom origins, and binds release models only to Profiles whose
catalog mode includes template data. The selector searches configured presets
and release candidates, reports compatibility from frozen dialect support plus
the installed product-adapter boundary, and keeps live availability and Recent
unknown. `greentyper config catalog` emits the static snapshot without Config,
credential, or network access. The current Responses and Chat Completions
adapters accept the official OpenAI template through the same frozen capability,
dialect, and endpoint checks as compatible gateways. The current Messages
adapter accepts only the official DeepSeek template and its explicit frozen
Messages route; no OpenCode Go wire compatibility is inferred from catalog data.
Price Schedules are now a fourth typed Config Object with nested lifecycle and
schema-generated editor routes. The Config Runtime validates provider/pricing
provenance, effective intervals, selector overlap, and fixed integer rates before
freezing the resolved book. Product admission passes that book into both plain
and Product-driver Turns; `stats` and the terminal-neutral status projection
surface immutable pay-as-you-go estimates without scanning history. Editable
Config accepts manual provenance only; trusted template-rate and provider-charge
ingestion remain separate future authority paths.
The compatible bare `stats` command still emits its complete replayed snapshot.
Explicit summary-only and bounded-page modes now avoid cloning or serializing
the complete attempt list, stamp every report with the Ledger revision, bind
checksummed cursors to that revision and requested instant, cap pages at 1,000
attempts, and reject stale or malformed cursors. They still replay the bounded
Ledger before building the cached projection; this slice is
output/materialization pagination, not an indexed on-disk query engine.
A first Context Pressure contract is also implemented. A pure core projector
combines caller-supplied context limit, used tokens, output reserve, and
exact/estimated provenance with checked arithmetic and explicit unknown reasons.
The default policy classifies normal, soft, and hard pressure at 65% and 90%.
An optional single-Agent Runtime path stops a known hard-pressure Turn before
Ledger or Provider effects; soft and unknown facts preserve existing admission.
The terminal-neutral status projection carries the immutable snapshot and marks
estimated occupancy with `~`. No reduction or compaction is performed.

This does not complete Phase 3. VT/ConPTY rendering and input, the keyboard event
loop and terminal-backed schema editors, rendered object-name and confirmation
dialogs, rendered credential binding and template-picker/starter-preset workflow,
live catalog discovery and Recent evidence, automatic Context View/token-source
projection, provider-reported
charge and subscription-quota accounting, richer observed Provider metadata,
and the P0/P1/P2/P6
performance evidence remain pending.

Exit criteria:

- Every Config Object has an interactive editor route.
- `/config pro url` reaches a focused validated gateway editor.
- Narrow and wide terminal-backend golden tests pass; terminal-neutral row goldens already pass.
- Context, cost, cache, effort, tier, rolling, and workday values preserve exact/estimated/unknown states.
- TUI input-ready, idle memory, and idle CPU budgets pass on FMDev.

## Phase 4: Provider Portability

Continue the release templates and seed catalog with template-specific DeepSeek
Responses/Chat Completions and OpenCode Go adapter execution, lazy discovery,
starter-preset acceptance, provider capability probes, explicit fallback
chains, observed availability, and provider/model epoch switching. The
OpenAI/openai-compatible Responses and Chat Completions adapters and the first
official DeepSeek Messages adapter are implemented; custom gateway routes and
user-owned Model Presets can be selected explicitly by ID for headless Turns.
Their Profile/model/dialect and optional output-token limit resolve through the
frozen Config/Provider Epoch boundary; rendered selection and the remaining
Preset policy fields are still pending.

Exit criteria:

- Golden fixtures continue to pass for all three dialects and every newly
  enabled template/adapter pair.
- Mixed-dialect OpenCode Go models resolve per Model Preset.
- Unknown discovered models remain unavailable until their dialect is trusted.
- Origin changes require credential and pricing decisions.
- Missing required capabilities fail unless the user explicitly selects a downgrade.

## Phase 5: Skills and MCP

Implement built-in, user, and project Skill discovery; pinned Skill Invocation identity; progressive loading; secure script execution through Tool Runtime; lazy MCP connections; shared transports with isolated capability views; direct and gateway tool exposure; Elicitation; and server-fault isolation.

Exit criteria:

- Changed Skill content blocks resume until explicit migration.
- Skills cannot grant tools or approval.
- MCP discovery/result content cannot alter Rules or authority.
- One failed MCP server cannot terminate unrelated Agents.
- Connection sharing reduces resources without sharing capabilities.

## Phase 6: Context, Memory, and Compaction

Implement Artifact offload, Context Pressure thresholds, deterministic reduction, Runtime Fold, Safe Barrier checkpoints, semantic handoff, provider-native compaction adapters, typed Memory Candidates, evidence promotion, scoped retrieval, supersession, user edit/forget/export, and periodic full rebase.

The pure pressure value object, default threshold classification, optional hard
admission gate, and terminal-neutral exact/estimated/unknown projection already
exist. This phase supplies the authoritative Context View inputs and every
mutation/recovery mechanism around that decision.

Exit criteria:

- Soft pressure triggers reduction near 65%; hard pressure near 90% stops unsafe admission rather than deleting truth.
- Failed or stale compaction leaves the prior Context View usable.
- Compactor has no tools, credentials, MCP access, or Durable Memory write authority.
- Memory never grants capabilities and drift-prone code facts revalidate.
- Repeated checkpoint cycles do not accumulate summary drift against full rebase fixtures.

## Phase 7: Agent Teams and Workspaces

Implement Task DAGs, one-owner transitions, Agent lifecycle, global/sub-budgets, ledgered direct/broadcast messages, Completion Capsules, downward-only Delegation, worktree allocation, Workspace Leases, Read Set validation, explicit merge/conflict handling, and TUI Team views.

Exit criteria:

- Target Machine defaults to two Active Agents; excess Agents become Dormant.
- Parallel writers never share a writable worktree.
- Stale Read Sets block or rebase before mutation.
- Child failure/retry/cancellation and budget exhaustion are explicit.
- Agent tree, state, budget, provider, worktree, diff, and blocker are inspectable.

## Phase 8: Hardening and Alpha Release

Complete storage migrations, backup/restore, Diagnostic Bundles, fuzz campaigns, crash matrices, signed portable packaging, x86-64-v3 verification, documentation, FMDev baselines, Target Machine Acceptance Run, and jcode same-host comparison.

Exit criteria:

- All [Testing Strategy](testing.md) release gates pass.
- All [Performance Contract](performance-contract.md) absolute limits pass or have an explicit accepted revision.
- Signed no-admin acceptance bundle completes in 10 to 15 minutes and produces a redacted ZIP.
- The Target Machine result is bound to the candidate executable hash and passes; an RC waiver may stage the candidate but cannot complete this phase or Alpha.
- Release contains no resident updater, remote telemetry, or unsupported compatibility promise.

## Alpha Completion

Alpha is complete when a Windows user can configure providers without editing files, select a model, run recoverable single- and multi-Agent coding work, use Skills and MCP under explicit authority, survive context pressure and restart, inspect cost/cache/context statistics, and produce an Acceptance Run that meets the contract on the Target Machine.
