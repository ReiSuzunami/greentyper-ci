# Implementation Plan

## Delivery Rule

Implementation proceeds as runnable vertical slices. Each phase must preserve Ledger recovery, authority boundaries, and measured resource behavior; no phase may defer all testing or performance work to the end.

Feature implementation was authorized on 2026-08-09. The first core slice fixes the Agent Team command/event interface, process-local Agent Sessions, and pure orchestration policy early; it does not claim Phase 7 completion. Plain `TeamRuntime` remains volatile. A separate durable Team adapter proves synchronous persistence, and the core Runtime Kernel now owns it, gates root admission, persists operation identity and acknowledgement records in the same Team Ledger, and rebinds the complete non-terminal Session set after recovery. The first Phase 2 slices persist Tool call identity, Approval Grant binding, prepared-effect state, terminal digests, and explicit ambiguous-effect reconciliation, and add bounded generic SSE framing plus a strict OpenAI Responses streaming decoder. Configured product Provider driving and one explicit `local.echo` approval/delivery path are now present; broader Tool catalogs and presentation remain pending.

## Phase 0: Repository and Measurement Foundation

Create the Cargo workspace, Windows-first build profiles, formatting/lint policy, GitHub CI, schema/version conventions, deterministic fixture harness, benchmark harness, and portable packaging skeleton.

Current implementation includes the workspace, CI, schema convention, one embedded deterministic Agent Team fixture, versioned acceptance and benchmark evidence, an x86-64-v3 runtime guard, a portable ZIP skeleton, an isolated eight-workload SQLite WAL versus checksummed append-log matrix, an isolated direct VT versus Ratatui/Crossterm render matrix, an isolated HTTP loopback SSE matrix for Reqwest/Rustls and native WinHTTP through Wrest, and three separately built system/Mimalloc/Snmalloc allocator runners. The storage matrix covers critical sync/replay, max-event and deterministic 250ms-expiry streaming batches, barrier-synchronized one-winner cross-process CAS, backup/restore, cross-process old-or-new migration recovery, critical cross-process termination/restart with known-not-repeated or ambiguous-blocked outcomes, and SQLite-only synthetic WAL VFS write, short-write, and sync failures followed by no-fault integrity-checked reopen. The CAS workload launches eight authenticated child contenders against one frozen expected head; every child atomically validates that expected head inside the candidate's serialization boundary, exactly one commits, seven report stale-head loss, and parent replay verifies the canonical winner. The migration workload creates three independently supervised v1 stores, terminates each child at an early unpublished, complete unpublished, or published-v2 boundary, and requires recovery to expose exactly two complete v1 generations plus one complete v2 generation. The terminal matrix verifies 27 ANSI-replayed frames, live resize, clearing stale content, styles, Unicode width, and zero-byte model-equality no-ops across three viewport sizes. The transport matrix verifies seven cold/warm, error, timeout, cancellation, explicit-proxy, and custom-origin cases with split UTF-8/line framing and credential non-leakage. The allocator matrix currently proves compile-time global-allocator isolation and identical deterministic allocation-pressure results; it does not yet prove the named P0-P6 resource workloads. Phase 0 still requires fuzzing, all Runtime durability boundaries, real power-loss and Windows directory-entry durability evidence, terminal Windows/ConPTY and resource evidence, transport HTTPS/TLS/proxy-auth/Windows runtime/resource evidence, allocator cold-start/idle/streaming/Agent/context-pressure resource evidence, complete same-commit 30-run FMDev comparisons, and recorded technology choices.

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

Add OpenAI Responses SSE, canonical tool calls, Tool Ledger identities, Approval Grants, one local process tool, Windows process control, usage normalization, and explicit Provider interruption/recovery behavior.

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
reconciliation. A private same-binary child-process matrix now terminates all
three executor results (success, failure, and ambiguous) at six representative
points from executor return through the terminal outcome append. Across all 18
cases, restart either recognizes the complete terminal outcome or exposes the
call as reconciliation-required; the external effect marker remains exactly
once, repeat identity never returns to approval, and raw arguments, outputs,
and executor reasons remain outside the Tool Ledger.

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
success blocks rather than repeating the effect. Runtime Event schema 14 preserves
durable Usage Attempt boundaries, frozen Usage Windows, Provider dialect, the
expanded optional Usage Records, frozen Provider Profile snapshot, and the
subsequent frozen Price Schedule cost evaluation plus an optional selected-Preset
output-token limit, typed reasoning/service-tier policy, distinct template-mirror
pricing provenance, pending current-Agent Model selection, typed Provider-block
origin and unavailability stage, durable cancellation, explicit early Provider
retry, frozen Preset fallback recovery, and typed Context Mode while replaying
historical schema 1 through schema 13.

A product-private `local.echo` tracer now exercises the concrete process seam.
It launches a fixed same-binary child without a shell, clears inherited
environment and working-directory state, bounds all three standard streams and
execution time, kills the Unix process group on abort, and on Windows creates
the process suspended before assigning a single-process, 128 MiB,
kill-on-close Job Object. The adapter rejects filesystem and network resources.
The product exposes it only through explicit `--tool local.echo` approval; it
is one narrow local process Tool, not a general process sandbox or
caller-selected command. Public `tool status` performs read-only Tool Ledger
inspection, while `tool reconcile` records an externally observed failure or
success digest for the original Agent-bound call. A same-binary integration
test terminates the product after durable `EffectPrepared` and executor entry,
then proves blocked restart, explicit reconciliation, no effect replay, and
later Turn admission. A separate core same-binary matrix terminates after the
executor returns and around the terminal outcome frame header, body, commit,
flush, and sync points for success, failure, and ambiguous results. It proves
complete-frame replay or explicit reconciliation without re-executing the
effect; it is representative process-termination evidence, not real power-loss
or exhaustive byte-offset evidence.

Three configured dialect adapters now exercise a closed set of exact
template/model/dialect pairs across the concrete Responses, Chat Completions,
and Messages HTTP seams.
Config Runtime resolves and freezes its typed Provider Profile, origin,
selected-dialect route, dialect, pricing decision, and opaque credential
reference. Each adapter resolves a secret from the origin-bound product vault,
requires HTTPS for remote origins, disables proxy discovery and redirects,
sends a bounded canonical request, applies a fixed deadline, and streams the
response through its core decoder into the real single-Agent Runtime. The
DeepSeek Responses adapter admits V4 Flash only, uses Bearer authorization,
maps `max_output_tokens`, validates bounded reasoning without projecting raw
reasoning text, and reconstructs its one stateless Tool continuation. A
preferred Responses dialect for V4 Pro resolves to Chat before admission and
freezes that effective dialect; this is not a network retry. The DeepSeek Chat
adapter uses Bearer authorization, `max_tokens`, a 384K output
ceiling, non-thinking mode, and an ordinary non-Beta Tool schema; it rejects
unsupported reasoning/service-tier policy before network I/O. The DeepSeek
Messages adapter uses `x-api-key`, pins the compatibility version,
disables unsupported reasoning blocks, and admits only the exact frozen
DeepSeek/Messages pair. Private loopback fixtures retain synthetic
authorization. Tests cover fragmented success, canonical replay, exact
dialect-specific request and one-Tool continuation bodies, HTTP failure-body
redaction, timeout, endpoint/status policy, trusted and untrusted TLS, and
missing credential failure before network access.
The OpenCode Go adapters admit only release-catalog-verified model/dialect
pairs. Chat Completions uses the frozen Chat route and Bearer credential, maps
the frozen output limit, rejects unsupported reasoning/service-tier policy
before network I/O, and reconstructs the same frozen dialect from the Provider
Epoch. The exact GPT-5.6 Luna Responses pair uses the frozen Responses route,
`max_output_tokens`, Bearer authorization, the same policy rejection, and one
Responses Tool continuation through durable approval. Messages uses the frozen
Messages route, `x-api-key`, the pinned Anthropic version header, and
`max_tokens`; unlike DeepSeek Messages it omits the DeepSeek-only thinking
field. One approved Messages Tool continuation crosses the same durable effect
boundary, and prepared output survives restart without repeating that effect.

The Provider interruption/recovery batch is implemented across all three HTTP
dialects. Each adapter classifies unavailability before a response, before the
first decoded event, or after the first event; early EOF uses the same decoder
progress while semantic format failures remain invalid responses. Loopback
fixtures assert one connection and therefore no automatic transport retry.
Schema 11 added the unavailability stage and `TurnRetryRequested`; schema 12
added the exact-head Context checkpoint; schema 13 added bounded frozen Preset
fallback candidates and `ProviderFallbackRequested`; current Runtime Event
schema 14 adds typed Context Mode plus Config source.
Execution switches candidates only after an initial request fails before its
first event. After restart, the CLI explicitly selects the next frozen candidate
first and otherwise retries the active one. Each attempt retains candidate-bound
Usage/cost evidence; partial streams, malformed output, Tool-derived state,
continuation failure, and old stage-untyped blocks reject without mutation. A
recovery may repeat remote work or billing, and another early failure requires
another explicit request. Cancellation keeps
its immutable Usage/cost and Config/Provider Epoch evidence, invokes neither
Provider nor Tool, requires recovered Active Agent authority for Product state,
and leaves Team/Tool Ledgers byte-identical. Missing, incomplete, prepared,
resume-required, streaming, Tool-derived, reconciliation-required, and legacy
untyped states fail closed.

The remaining slices are still policy, protocol, and fault-adapter work:
live inference conformance, non-Windows credential backends, configurable proxy
policy, DeepSeek Chat/Messages reasoning
blocks, provider-native Context Mode execution, broader
canonical Items, multiple
Tool calls,
durable resumable Tool result references, richer TUI/App Server
approval/delivery, caller-selected process policy, complete Windows Job
lifetime/resource evidence, automatic retry policy/partial-stream reconnect behavior, and cross-process crash
matrices for the remaining Runtime, Provider, Tool, delivery, and product
acknowledgement boundaries remain pending.
Background/periodic catalog discovery is excluded by the current Performance
Contract unless measured evidence supports an approved exception.

Exit criteria:

- A fixture Responses, Chat Completions, or Messages Turn can call one approved
  Tool and finish canonically.
- Successful and ambiguous effects cannot auto-repeat after injected crashes.
- Credentials stay outside files and Ledgers.
- Provider raw events are diagnostic artifacts, not core state.

## Phase 3: TUI, Config Center, and Observability

Add VT/ConPTY TUI, hierarchical Command Paths, global command palette, Config Schema-driven editors, Provider wizard, model selector, adaptive statusline, Context Pressure, Usage Records/Rollups, `/stats`, and named Usage Windows.

The first observability slice is implemented. Runtime Event schema 14 preserves
the schema-6 contract that durably brackets each Provider request and continuation with an immutable Usage Attempt,
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
revision-bound editors, the configured-preset selector, Stats, Agent, and
Blocker views.
It refuses to discard dirty drafts implicitly and retains the editor after
validation or revision-conflict failure. Its deterministic row layout freezes
Unicode-safe text fitting, prioritized compact status segments, a wide detail
row, and exact 40x12, 80x24, and 160x50 snapshots. Those terminal-neutral slices
did not by themselves claim an ANSI/VT backend, keyboard event loop, rendered
terminal dialog, ConPTY integration, or App Server surface. Typed nested `add`
and `remove` paths now open
revision-bound create/delete sessions for Provider Profiles, Model Presets, and
Usage Windows. Creation keeps one schema-driven Draft across focused fields;
deletion removes only a target-layer object, passes through reference validation,
and preserves backup and compare-and-swap behavior.
Provider Profile create/edit routes now enter a terminal-neutral purpose-built
wizard backed by the same Config Draft. It stages opaque credential references
only through the credential field, derives a validated frozen candidate Profile
without writing Config, and can explicitly test the configured `models` route.
The product adapter performs one bounded no-proxy/no-redirect GET and accepts
only a JSON body up to 256 KiB containing at most 1,024 unique model IDs of at
most 256 bytes each. It returns fixed redacted status categories plus a sorted,
ephemeral observation whose entries contain only the ID and an exact optional
release-catalog key. Unknown IDs gain no dialect, capability, or adapter
authority. The same adapter powers `greentyper config test-provider` for the
selected committed Profile. Tests cover bounded parsing, known and unknown IDs,
unknown-model admission rejection before network I/O, missing credentials,
redacted upstream 503 classification, candidate non-mutation, result
invalidation after a Draft change, and observed revision conflicts before a
probe.
A release-bundled Provider Catalog now freezes schema-versioned OpenAI,
DeepSeek, and OpenCode Go template defaults and seed model facts with
field-level source references and observation time. Config Runtime applies
template defaults before explicit Profile overrides, defaults custom origins to
the distinct `template_mirror` source only when the template has a reviewed
bundled rate card, preserves explicit pricing overrides, and binds release
models only to Profiles whose
catalog mode includes template data. The selector searches configured Presets,
release candidates, and saved discovery observations, reports compatibility
from frozen dialect support plus the installed product-adapter boundary, and
derives Recent from durable Usage. Observed availability is explicit but is not
a live health claim. `greentyper config catalog` emits the static snapshot without Config,
credential, or network access. The current Responses and Chat Completions
adapters accept the official OpenAI template through the same frozen capability,
dialect, and endpoint checks as compatible gateways. The current Messages
adapter accepts the official DeepSeek template and exact release-catalog
OpenCode Go Messages model/dialect pairs. A declared route or catalog record
alone never grants wire compatibility.
The DeepSeek release records also carry reviewed, versioned Flash and Pro price
cards; official origins freeze `template` provenance and custom origins freeze
`template_mirror` provenance without inheriting credential or origin authority.
Price Schedules are now a fourth typed Config Object with nested lifecycle and
schema-generated editor routes. The Config Runtime validates provider/pricing
provenance, effective intervals, selector overlap, and fixed integer rates before
freezing the resolved book. Product admission passes that book into both plain
and Product-driver Turns; `stats` and the terminal-neutral status projection
surface immutable pay-as-you-go estimates without scanning history. Editable
Config accepts manual provenance only; bundled template-rate ingestion now has a
dedicated frozen source, while provider-reported charge ingestion remains a
separate future authority path.
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
Ledger or Provider effects. Soft pressure publishes a bounded Context checkpoint
at a Safe Barrier before admission; unknown facts preserve admission without
inventing a checkpoint.
The terminal-neutral status projection carries the immutable snapshot and marks
estimated occupancy with `~`. This presentation slice does not perform Provider
consumption or semantic compaction; Runtime now performs the bounded checkpoint
projection independently of terminal rendering.

A first product terminal tracer is now implemented. `greentyper tui`
inspects the selected Ledger without creating it, renders the existing
Presentation Controller through a Unicode-aware Direct VT cell diff, maps
blocking Crossterm key and resize events, suppresses model-equality redraws,
clears stale cells, bounds the viewport and Slash query before allocation, and
restores raw mode, cursor visibility, and the alternate screen on normal and
error returns. Windows startup explicitly requires VT output support. Every
Config Schema field now has a rendered interaction in the user scope. Top-level
and statusline fields open directly; object field routes carry their pending
query through a kind-filtered Config Center. Choice, bounded text, integer,
boolean, credential-reference, and TOML-list interactions all require a real
dry-run preview before commit, persist through the Config Runtime CAS/backup
path, explicitly discard, block dirty escape/quit, preserve the editor on
validation or revision conflict, and render bounded error notices. Tests prove
no-change commit creates no file, existing objects can be edited through their
field routes, and committed state survives reopen. The TUI's non-Config
projection remains snapshot-based. `/config provider add` prompts for a bounded
Profile ID and traverses template, opaque credential reference, base URL,
routes, dialects, catalog mode, pricing source, and loopback permission. It
reopens the created Profile, never renders a credential reference, and retains
invalid-ID, policy, or stale-revision Drafts for correction or explicit discard.
F5 now invokes the existing bounded Provider connection and model-list tester
against the current revision-bound candidate and renders its ephemeral status.
The action does not commit Config; staged edits invalidate the result, and a
stale revision fails before the tester runs. This slice adds no Runtime, Tool,
secret-store, catalog, Provider Epoch,
or approval authority and does not automatically rebuild the active status
projection after commit.
Typed `/config provider|model|pricing|stats-window remove` routes now carry the
pending command into a section-filtered selector and render an exact object
confirmation. Enter dry-runs and CAS-commits the target-layer deletion; Escape
cancels it. Reference-validation and stale-revision failures keep the
confirmation live, and a real-key test reopens Config to prove persistence.
`/config model add` now supplies the second rendered object-name workflow. It
collects a bounded Preset ID and traverses all nine fields in one revision-bound
Draft. Provider, model, and dialect remain required; reasoning effort, service
tier, maximum output tokens, context mode, favorite, and fallback list are
optional. Preview, CAS commit, cross-reference/cycle validation, stale-revision
retention, explicit discard, and reopen persistence use the same terminal-neutral
editor path. Application and compatible release-starter acceptance are separate
`/model` actions; live catalog data remains outside the form.
`/config stats-window add` now supplies a third rendered object-name workflow.
It collects a bounded ID plus start, end, weekday-list, and IANA-time-zone text
in one schema-driven Draft. Structured weekday input is visibly buffered until
Tab or Enter can parse and stage the complete TOML array; dirty local input,
validation failure, and stale revisions all remain recoverable. The final text
field previews and CAS-commits on two explicit Enter actions, and a real-key
test reopens Config to prove the resolved window persists. This slice does not
automatically rebuild the active usage projection.
`/config pricing add` now supplies the fourth rendered object-name workflow and
completes the Config Object creation set. It traverses all 17 Price Schedule
schema fields, keeps optional selectors skippable, offers only valid manual
provenance for an editable schedule, and visibly buffers non-negative-integer
text until parsing succeeds. The final rate previews and CAS-commits on two
explicit Enter actions. Real-key and recovery tests prove Config reopen,
domain-validation repair, stale-revision retention, dirty-quit blocking, and
explicit discard without overwriting the winner. This slice does not rebuild
the frozen pricing book, ingest provider-reported charges, or add rich cost
presentation.
The root `/model` route now supplies a Direct VT browser over the latest
successful configured-Preset, release-catalog, and local-discovery projection. It supports
bounded
fuzzy query input, Favorites/Recent/Compatible/All groups, selected-row
navigation, Usage-derived Recent, and source/freshness/availability detail. A
second Enter on configured detail stages a bounded,
Session-authenticated Runtime Event for the current Agent's next Turn; the
pending ID is visible and replaceable. On compatible release detail, a second
Enter creates a prefilled ordinary user-scope Preset Draft and uses the existing
preview/CAS-commit path; incompatible or duplicate candidates remain
recoverable without Config overwrite. Current unknown discovered models use a
separate bounded ID plus explicit trusted-dialect path into the same Draft, with
final observation revalidation before creation. F5 is explicit foreground
discovery; failure preserves the prior state/view. The next
headless admission rechecks the exact Preset and Config fingerprint, consumes the
selection atomically with Config/Provider freeze, and leaves it pending on
preflight failure. Selection performs no Config write, credential lookup,
Provider request, child-Agent mutation, or authority grant.
The root `/stats` route now supplies a read-only browser over the latest
successful Usage projection. It renders rolling 1-hour, 1-day, and 7-day
summaries and
groups for durable Attempts, cached Turns, per-Turn Provider/Model/Dialect/Policy
distributions, current Thread, Agents, Team, Named Windows, and rolling Token &
Cache quantities. Token & Cache and every scoped rollup detail show
token-weighted cache-read/input and cache-write/input ratios while keeping exact,
estimated, missing, internally inconsistent, and overflowed states distinct.
The adaptive statusline exposes compact 1-hour ratios when width permits.
Up/Down selects rows and Enter opens bounded detail. Real-key tests prove
Runtime, Team, and Tool Ledger bytes remain unchanged.
The root `/agent` route now supplies a read-only browser over a shared-lock Team
sidecar inspection. It navigates canonical Agent rows and bounded detail without
exposing message bodies, capability/scope labels, Completion Capsules, or Agent
Session authority. Missing state creates nothing; incomplete final-frame bytes
are reported without repair; corruption and incomplete Product sidecars fail
closed. Real-key tests prove Runtime, Team, Tool, and Config bytes remain
unchanged. Lifecycle actions, Team-operation acknowledgement, and Workspace
presentation remain outside this slice.
F6 or Ctrl-R now refreshes the local Config/statusline, Model, Usage, and Team
snapshot from the Slash Panel or any read-only browser. Replacement is
all-or-old at the TUI boundary: inspection or projection failure keeps the prior
Config runtime and complete view and permits another refresh. Runtime, Team,
and Tool Ledgers are inspected independently rather than under one shared
transaction. Real-key tests add a Config Preset, complete a Usage Turn, add Agents,
inject an incomplete Product-sidecar pair, recover, and prove Config plus all
three Ledgers remain byte-identical to the external writer's state. The refresh
performs no Provider discovery, credential lookup, mutation, or background
polling.

The next contiguous terminal slice closes the fixed `local.echo` approval flow.
The root `/blockers` route lists Runtime, Team, Task, Tool, and Config blockers
from local projections without exposing raw Tool material. The first Enter on a
pending Tool approval shows a local recovery warning. A second Enter explicitly
reopens ProductDriver state, authenticates the rebound Active Agent Session,
reconstructs the configured Provider from the frozen Epoch, and resumes the
request to recover the exact ephemeral arguments and resources. That confirmed
recovery may use credentials, append Usage Attempt and cost records, and affect
Provider quota or billing. The terminal renders every detail row before its
Approve and Deny actions. Escape drops only that in-memory context; denial executes nothing; a
grant crosses the existing prepared-effect boundary and the fixed process
executor once. Prepared Provider output cannot be escaped or quit past and is
durably acknowledged only after terminal rendering; acknowledgement failure
keeps the output live for retry. Targeted tests cover read-only blocker
inspection, exact detail navigation, approve, deny, cancel, resolution failure
followed by a fresh recovery, acknowledgement retry, restart-bound AgentSession
recovery, and no duplicate executor call.

The first App Server product slice is also complete. `greentyper app-server
--stdio` owns a 64 KiB newline-delimited JSON request boundary and exposes
`config.schema`, `config.get`, plus process-local `config.draft.begin`, `set`,
`reset`, `validate`, and `commit`. Typed staging never changes effective Config;
validation failure keeps the bounded Draft live; successful commit consumes the
handle and uses the existing lock, revision CAS, backup, atomic write, and reload
path. Integration tests prove fixed malformed/oversized-frame recovery,
credential-reference read rejection, no-write staging, validation repair,
no-change commit, commit/reopen, and two-process stale-writer rejection without
overwriting the winner. Conflict refreshes the connection to the winning
revision so a new Draft can begin while the stale handle remains live. This
slice preserves a last-valid effective value when one exists, reports repair
state without filesystem paths, and lets a startup repair proceed through the
same begin/reset/validate/commit flow. A second contiguous slice exposes
origin-bound credential `bind`, `replace`, `test`, and `forget` through the same
bounded stdio stream. Secrets are moved into the zeroing vault value, the owned
request frame is scrubbed, responses are status-only, and non-Windows platform
vaults fail closed. Credential operations add no Provider request or Runtime,
Team, Tool, Agent, or approval authority.
A third contiguous App Server slice adds local read-only operational inspection.
`runtime.status` projects bounded recovery facts without item text or block
reasons and reports whether a blocked Turn is explicitly retryable.
`runtime.stats` reuses revision/as-of-bound summary and Attempt paging.
`agent.list` reuses the redacted Agent Center projection, while `tool.status`
returns only call and Agent numbers, Tool/status, expiry, and result digest.
Missing state creates nothing; incomplete product sidecars fail closed and the
stream remains usable; Runtime, Team, and Tool Ledger bytes stay unchanged.
These independently read projections do not add Provider calls, credential
lookup, Runtime/Team/Tool mutation, approval, delivery, or acknowledgement.
A fourth contiguous App Server slice closes the fixed local recovery flow.
`runtime.delivery` recovers exact prepared text without a write, and
`runtime.acknowledge` durably closes only that delivery with idempotent repeats.
`tool.reconcile` records an observed-success digest or fixed observed failure
without invoking the executor. `tool.decide` authenticates the rebound Active
Agent Session and uses a connection-local two-step protocol: review reconstructs
the frozen Provider and returns exact canonical arguments/resources with
confirmation hashes; approve or deny must echo those hashes, reconstruct the
request again, and revalidate the binding. Denial executes nothing; approval
crosses the existing bound prepared-effect transaction once and returns
unacknowledged output for explicit retrieval and acknowledgement. All mutations
strictly open existing Runtime/Team/Tool Ledgers under exclusive locks and reject
incomplete tails without repair. Empty Team state never admits a root Agent.
Tests prove missing, wrong, corrupt-tail, duplicate, interrupted-response,
review mismatch, approve, deny, and restart paths; exact Ledger bytes and
executor counts pin no-repair and no-reexecution behavior. Review and confirmed
decision may each use credentials, append Usage/cost records, and affect quota
or billing.
A fifth contiguous App Server batch completes three bounded Provider Recovery
slices. The cancel slice terminalizes one exact typed Provider-origin blocked
Turn and remains idempotent. The retry slice accepts only the early retryable
Provider failure and durably moves that same Turn to `resume-required` without
contacting a Provider, resolving a credential, executing a Tool, or delivering
output. The resume slice then requires the exact Turn, frozen Provider Epoch,
and, for product state, the recovered Active Agent Session. It returns prepared
output or the exact Tool approval without acknowledgement; ordinary delivery
and acknowledgement remain explicit. Tests cover ordinary and product Ledgers,
missing/wrong/non-retryable state, repeat safety, credential failure, Runtime and
Team incomplete tails, restart recovery, cross-Ledger byte identity, output
retrieval, and final acknowledgement. Resume may contact the Provider, append
Usage/cost records, and repeat quota or billing; cancel and retry cannot.

The terminal credential batch closes the Provider credential flow in four
bounded slices. F7 on a clean Provider credential field opens Bind, Replace,
Test, and Forget for the exact Profile/reference/canonical-origin scope. Bind
uses hidden bounded input; Replace adds a separate confirmation without reading
the old value; Test reports vault presence without a Provider request; Forget
supports cancel before its confirmation. The terminal revalidates scope before
dispatch, clears discarded input, disables F5 inside the credential modal, and
uses only fixed status notices. Real-key tests prove bind-to-F5 scope reuse,
replace, test, cancel/forget, stale-scope recovery, secret/reference redaction,
Config and Ledger byte identity, and non-Windows fail-closed behavior.

This does not complete Phase 3. Audited Windows ConPTY behavior, broader
multi-Tool/App Server policy, general App Server Runtime control beyond the
exact cancel/retry/resume recovery flow, remote App Server transport,
automatic/background snapshot refresh,
custom-template and automatic starter-offer workflow,
automatic Context View/token-source
projection, provider-reported
charge and subscription-quota accounting, richer observed Provider metadata,
new-Agent Preset-default inheritance, Agent lifecycle
actions, and the P0/P1/P2/P6
performance evidence remain pending.
Background/periodic catalog discovery is outside this default plan unless an
approved Performance Contract exception is added first.

Exit criteria:

- Every Config Object has an interactive editor route.
- `/config pro url` reaches a focused validated gateway editor.
- Narrow and wide Direct VT unit goldens pass; real Windows/ConPTY and final backend evidence remain.
- Context, cost, cache, effort, tier, rolling, and workday values preserve exact/estimated/unknown states.
- TUI input-ready, idle memory, and idle CPU budgets pass on FMDev.

## Phase 4: Provider Portability

Continue the release templates and seed catalog with OpenCode Go adapter
execution, explicit starter updates, provider capability probes, observed availability,
and broader provider/model epoch switching. The
OpenAI/openai-compatible Responses and Chat Completions adapters, official
DeepSeek Responses, Chat Completions, and Messages pairs, and release-verified
OpenCode Go Chat Completions and Messages pairs plus the exact GPT-5.6 Luna
Responses pair are implemented; custom gateway routes and
user-owned Model Presets can be selected explicitly by ID for headless Turns.
The connection-test port also emits a bounded, one-shot model-list observation.
An explicit four-slice CLI flow now adds missing-safe status, successful-only
atomic refresh into an independent schema-1 state file, read-only release plus
discovery merge with Profile-fingerprint freshness, and explicit current-model
acceptance into an ordinary Config Preset. Failed probes preserve prior state;
stale or absent models and unsupported dialects fail before Config write.
Unknown remote fields never gain capability, endpoint, pricing, credential,
instruction, or execution authority. The Direct VT selector now consumes the
same shared release/discovery projection and derives Recent from durable Usage.
A three-slice foreground-discovery flow probes an eligible selected Profile
once when `/model` opens, skips disabled or route-less Profiles without network
I/O, and retries on re-entry or explicit F5. Failed probes keep the prior
observation/view; typing, navigation, resize, idle time, and F6/Ctrl-R never
probe. A current unknown model requires a bounded Preset
ID plus an explicit trusted Profile dialect; the exact observation is checked
again before an ordinary Config Draft is created, and CAS conflict remains
recoverable through discard/reopen.

A four-slice discovery-task settlement now keeps that flow compatible with the
Performance Contract: the task object is idle until `/model` on-open or F5,
each action lazily creates one bounded worker, the terminal waits for its single
result and joins it, and no worker, timer, periodic poll, or automatic retry
survives into idle time. Before a successful result is committed, a freshly
loaded Config candidate must still match the exact Profile, template, and
opaque fingerprint captured at task start. Drift discards the result without
creating discovery state; probe failure preserves the previous bytes until an
explicit F5 retry succeeds.
Their Profile/model/dialect, optional output-token limit, typed reasoning effort,
and typed service tier resolve through the frozen Config/Provider Epoch boundary.
OpenAI Responses and Chat map the supported request fields. DeepSeek Responses
maps its supported reasoning effort and rejects service tier, while DeepSeek
Chat/Messages reject both unsupported policy fields before network I/O. Flash
keeps a preferred Responses dialect and Pro resolves that preference to Chat
before admission. Configured Presets can now be staged from `/model` for the
existing current Agent's next Turn and are consumed at durable admission.

A three-slice default-Preset batch now adds the schema-owned
`agent.default_model_preset` field, resolves user/project precedence, and lets a
headless Turn use that exact Preset only when neither an explicit ID nor a
pending current-Agent selection applies. Unknown targets fail validation before
write or Ledger creation; Config CLI set/reset keeps the failure recoverable.
An already-effective set/reset is an idempotent zero-write commit.
The Direct VT `/model` list and detail identify the effective default while a
second Enter continues to create a separate, Agent-session-bound pending
selection. Targeted tests prove project-over-user precedence, explicit override
when no conflicting pending selection exists, Config set/get/reset, and
Runtime-only mutation for current-Agent selection.

An explicit five-slice fallback batch now resolves each Preset graph depth-first
with first-occurrence deduplication, rejects unknown/cyclic/over-16 or
capability-lowering plans, and preflights every adapter before Runtime state
opens. Schema 13 freezes one Config/Provider Epoch pair per candidate. Runtime
switches only after `BeforeResponse` or `BeforeFirstEvent`, records immutable
Usage and cost evidence for every candidate, and never switches after partial
output or Tool-derived state. Product and plain headless paths both deliver the
successful candidate once. Crash recovery selects the next frozen candidate
before retrying the active one, resumes that exact Provider Epoch, and never
replays the failed primary. Team and Tool Ledgers remain byte-identical.

An explicit five-slice Context-Mode batch now makes `canonical` and
`provider_native` schema-owned choices with built-in `canonical` default,
freezes the mode and Config source in schema-14 Config Epochs, and maps older
epochs to built-in `canonical`. Canonical execution keeps the first Turn's
single-input request, replays bounded completed canonical history on later
Turns without a checkpoint, consumes the verified checkpoint tail when present,
and reconstructs the same history on explicit resume. `provider_native` remains
visible but fails before credential lookup, Provider construction, network I/O,
identifier allocation, or Ledger append; every fallback candidate's mode is
preflighted before any credential access, and explicit or soft-pressure Context
reduction performs the same check before checkpoint publication. Requested
Context Mode is durable Usage metadata and appears in the per-Turn policy
distribution without changing token or cost arithmetic.

A four-slice release-starter update batch now makes Config schema 2 the emitted
format while preserving schema-1 read compatibility without an open-time write.
Accepted starters carry a complete read-only catalog key, seed revision,
Profile, model, and dialect tuple. Core, CLI, App Server, and Direct VT paths
open the same ordinary revision-bound Draft only when that tuple and every
release-owned identity field still belong to the requested scope and a newer
compatible bundled record exists. Preview/commit updates identity and
provenance together while preserving user policy. Manual drift, mixed-scope
overrides, incompatible or already-current seeds, and CAS conflict remain
recoverable without overwrite. The paths perform no discovery, credential
lookup, Provider request, Agent action, or Ledger write; automatic starter
offers and silent updates remain absent.

New-Agent default inheritance, provider-native Context Mode execution, and
automatic starter offers remain pending. Background/periodic discovery is
outside the current Performance Contract unless measured evidence supports an
approved exception.

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

The Context foundation is implemented as four vertical slices. The core projects
ordered canonical Items from an exact Ledger head; reduction replaces old raw
text with Item-bound SHA-256 references while retaining a bounded recent tail;
schema 12 introduced the singleton checkpoint contract, which current schema 14
preserves at a Safe Barrier while rejecting a
stale source head; and `context status`/`context reduce` expose missing-safe
inspection plus explicit recovery. Soft pressure uses the same checkpoint path
before admission, hard pressure still stops before effects, and unknown pressure
does not invent state. Every checkpoint is a full rebase from authoritative
Items. Tests preserve Runtime bytes on stale/unsafe failure and preserve Team and
Tool bytes for Product state.

A second four-slice batch closes checkpoint consumption. Core materializes a
bounded request from the recent checkpoint tail plus canonical Items completed
after that checkpoint; Runtime validates it before admission and reconstructs it
for explicit resume; Responses, Chat Completions, and Messages map the ordered
conversation and preserve it through the supported one-Tool continuation; and a
cross-process CLI test proves `context reduce`, next-Turn admission interruption,
explicit `resume`, and post-recovery inspection. Archived artifact bodies remain
excluded and Config/Provider Epochs remain frozen. Under canonical Context Mode,
a missing checkpoint keeps the first Turn's current-input request and projects
bounded completed canonical history on later Turns.

This is not the full phase. There is no semantic handoff, provider-native
compactor, external Artifact store, typed Memory Candidate, retrieval,
supersession, or user memory lifecycle.

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
