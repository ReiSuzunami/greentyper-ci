# GreenTyper

[![Temporary public CI](https://github.com/ReiSuzunami/greentyper-ci/actions/workflows/ci.yml/badge.svg)](https://github.com/ReiSuzunami/greentyper-ci/actions/workflows/ci.yml)

GreenTyper is a Windows-first coding-agent runtime written in Rust. It is designed to remain responsive on memory-constrained developer laptops while supporting recoverable long-running work, provider portability, first-class Agent Teams, Skills, MCP, Durable Memory, and Compaction.

GreenTyper is an independent product with selected Codex-compatible protocol and Agent semantics. It is not a Codex CLI clone, a command-compatible replacement, or a wrapper around another agent process.

> Status: feature implementation is active. The repository now contains deterministic Agent Team policy, a recoverable single-Agent headless spine, bounded OpenAI Responses, Chat Completions, and Anthropic Messages SSE decoders, and a core Tool Runtime seam with durable call identity, approval binding, prepared-effect ordering, and explicit reconciliation. It is not yet a complete coding-agent product.

> Repository topology: [`ReiSuzunami/greentyper`](https://github.com/ReiSuzunami/greentyper) is the private canonical repository. [`ReiSuzunami/greentyper-ci`](https://github.com/ReiSuzunami/greentyper-ci) is a temporary public, non-authoritative mirror used only for hosted CI and build artifacts. See the [repository policy](docs/repository-policy.md).

## Target Product Shape

The following bullets describe the intended v1 product, not the current
implementation status. The concrete status and remaining work are listed under
[Development](#development) and in the [implementation plan](docs/implementation-plan.md).

- Windows 11 x64 first, targeting x86-64-v3 and modern VT/ConPTY terminals.
- Keyboard-first TUI plus a headless App Server; IDE and graphical clients come later.
- One resource-budgeted runtime process for logical Agents, with controlled child processes only for tools.
- Canonical provider-neutral Threads, Turns, Items, Tasks, and Events.
- OpenAI Responses, OpenAI Chat Completions, and Anthropic Messages dialects behind adapters.
- Built-in Provider Templates and a seed Model Catalog for OpenAI, DeepSeek, and OpenCode Go, with user-defined gateway URLs.
- Append-only Event Ledger as truth; Context Views, checkpoints, memory retrieval, usage statistics, and indices remain derived.
- Evidence-linked Durable Memory and safe-barrier Compaction that cannot acquire authority or erase history.
- Explicit task ownership, capability Delegation, workspace leases, and worktree isolation for Agent Teams.
- Schema-driven configuration shared by TUI dialogs, CLI, App Server, and TOML.

## Performance Position

The release target is a 16 GB RedmiNotebook 2022 running the latest Windows 11 while normal developer applications already consume about 80% of memory. Compatibility may be reduced when required to preserve correctness, security, low resident memory, low idle CPU, and input responsiveness.

Initial design budgets include a headless idle Private Bytes limit of 25 MB, a single idle TUI limit of 35 MB, idle CPU at or below 0.1%, and input-ready latency of 60 ms p50 and 100 ms p95. FMDev continuously checks the provisional numeric limits and relative regressions; only an asynchronous Target Machine run supplies absolute release evidence. See the [Performance Contract](docs/performance-contract.md).

## Documentation

- [Domain language](CONTEXT.md)
- [Architecture](docs/architecture.md)
- [Code layout](docs/code-layout.md)
- [Performance Contract](docs/performance-contract.md)
- [Configuration and command surface](docs/configuration.md)
- [Testing strategy](docs/testing.md)
- [Implementation plan](docs/implementation-plan.md)
- [Agent orchestration](docs/agent-orchestration.md)
- [Recoverable single-Agent Runtime](docs/runtime-kernel.md)
- [Provider Runtime](docs/provider-runtime.md)
- [Tool Runtime](docs/tool-runtime.md)
- [Measurement harness](docs/measurement-harness.md)
- [Technology benchmarks](docs/technology-benchmarks.md)
- [Repository policy](docs/repository-policy.md)
- [Architecture Decision Records](docs/adr/README.md)

## Non-goals for v1

- Drop-in Codex CLI compatibility
- ChatGPT OAuth or private ChatGPT backend integration
- Local model inference
- Remote Agent worker nodes
- In-process third-party plugin ABI
- Legacy Windows console support
- MCP Apps UI, automatic extension installation, or arbitrary embedded code
- Automatic cross-provider model routing
- Resident updater or automatic update service

## Development

Run `direnv allow` once after cloning. The checked-in `.envrc` scopes GitHub CLI metadata, commit identity, and Cargo build output to this workspace without containing credentials.

```text
cargo check --workspace --all-targets
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo run -p greentyper-acceptance -- verify-cpu
cargo run -p greentyper-acceptance -- bench list
cargo run -p greentyper -- tui --ledger ./target/dev-runtime.ledger
cargo run -p greentyper -- headless --ledger ./target/dev-runtime.ledger --input "hello"
cargo run -p greentyper -- headless --preset frontier --ledger ./target/dev-runtime.ledger --input "hello"
cargo run -p greentyper -- headless --ledger ./target/tool-runtime.ledger --tool local.echo --input "echo this"
cargo run -p greentyper -- status --ledger ./target/dev-runtime.ledger
cargo run -p greentyper -- retry --ledger ./target/dev-runtime.ledger --turn 1
cargo run -p greentyper -- cancel --ledger ./target/dev-runtime.ledger --turn 1
cargo run -p greentyper -- stats --ledger ./target/dev-runtime.ledger
cargo run -p greentyper -- stats --ledger ./target/dev-runtime.ledger --summary-only
cargo run -p greentyper -- stats --ledger ./target/dev-runtime.ledger --limit 100
cargo run -p greentyper -- tool status --ledger ./target/tool-runtime.ledger
cargo run -p greentyper -- tool reconcile --ledger ./target/tool-runtime.ledger --call 1 --failed
cargo run -p greentyper -- config schema
cargo run -p greentyper -- config catalog
cargo run -p greentyper -- config discovery status
cargo run -p greentyper -- config discovery refresh openai-main
cargo run -p greentyper -- config discovery catalog openai-main
cargo run -p greentyper -- config discovery accept frontier-live openai-main gpt-5.6-live --dialect responses --scope user
cargo run -p greentyper -- config get provider.model
cargo run -p greentyper -- config test-provider
```

For a configured OpenAI or openai-compatible Profile, `headless` accepts
`--dialect responses` or `--dialect chat_completions`. A configured DeepSeek
Profile accepts all three installed dialects. A DeepSeek Responses preference
resolves against the release model record before admission: V4 Flash freezes
Responses, while V4 Pro freezes Chat Completions because its Responses support
is not yet available. This is capability resolution before network I/O, not a
retry after partial output or Tool effects. Every other template/dialect pair
fails closed unless the product has an explicit adapter for that exact identity.
`headless --preset ID` resolves one configured Model Preset by exact ID and
freezes its Profile, model, dialect, and optional output-token limit for the
Turn. It cannot be combined with `--dialect`; missing IDs fail rather than
falling back to a model-name match.

All three HTTP dialect adapters classify unavailability at one of three stable
boundaries: before a streaming response, after the response but before the
first decoded event, or after at least one event. Early EOF and interrupted SSE
framing retain that boundary; other malformed Provider data remains an invalid
response. No boundary triggers an automatic retry or reconnect, and the
requests carry no inference idempotency key, so an absent response is not proof
that the remote service did no work or incurred no usage. Runtime Event schema
12 preserves schema 11's rule that only Provider-origin failures before a response or before the first
event as `retryable`. `retry --turn ID` durably rearms that exact Turn, input,
Config Epoch, and Provider Epoch before making one new Provider attempt. It may
repeat remote work, usage, or billing. A preflight failure after rearming leaves
the Turn `resume-required`; another early failure becomes blocked again and
requires another explicit retry request. Failures after the first event,
malformed output, Tool-derived or post-Tool continuation failures, and historical
stage-untyped blocks reject retry without writing. `cancel --turn ID` remains the
explicit terminal recovery for a typed Provider-origin block. Cancellation
preserves Usage/cost facts and frozen Epochs, invokes neither Provider nor Tool,
and is idempotent. Product retry and cancellation additionally require the one
recovered Active Agent Session, strictly open complete existing state, and leave
Team and Tool Ledgers unchanged.

The core Agent Team policy, Config Runtime, recoverable single-Agent Runtime,
and first Tool Runtime policy slice compile and run through interface-level and
cross-process headless tests. Config currently includes versioned TOML, drafts,
provenance, atomic replacement, repair, typed Provider Profile snapshots, and a
terminal-neutral schema-driven editor session and Provider Profile wizard. A
release-bundled Provider Catalog now supplies versioned OpenAI, DeepSeek, and
OpenCode Go template defaults plus seed model facts with field provenance. The
selector exposes compatible release candidates while keeping unverified live
availability explicit. A first Direct VT product tracer now renders the Slash
Panel, controller screens, and adaptive status rows through `greentyper tui` and
includes rendered user-scope interactions for every Config Schema field,
complete Config Object creation, existing-object field editing, and typed
deletion confirmations. `greentyper app-server --stdio [--ledger PATH]` now exposes the same
non-secret Config Schema, effective reads with redacted repair errors and status,
process-local typed Drafts,
validation, and atomic CAS commit path over bounded newline-delimited JSON.
Credential fields expose only opaque references and generic reads reject them;
the same local stdio stream can bind, replace, status-test, and forget
origin-bound credentials without returning their values. Bind and replace
accept the secret only in the bounded request frame and scrub the product-owned
frame after dispatch. The product CLI exposes the same operations without
putting secret material in arguments, Config, or Ledgers. Windows stores values
in the current user's Credential Manager; other platforms currently fail
closed. The same stream has read-only `runtime.status`, bounded `runtime.stats`,
redacted `agent.list`, and redacted `tool.status` operations. Missing state does
not create files; inspection never repairs a partial Ledger tail. Bounded
control operations reuse the existing Kernel and ProductDriver authority.
`runtime.cancel` terminalizes one exact Provider-origin blocked Turn,
`runtime.retry` durably rearms only an explicitly retryable initial Provider
failure without contacting a Provider, and `runtime.resume` reconstructs the
frozen Provider and recovered Active Agent Session for that exact
`resume-required` Turn. Resume may resolve an origin-bound credential, contact
the Provider, append Usage/cost facts, and affect quota or billing. Completed
output remains unacknowledged: `runtime.delivery` retrieves it and
`runtime.acknowledge` durably closes it. `tool.reconcile` records an externally
observed terminal result without executing the Tool, while `tool.decide` uses a
same-stream review/confirmation handshake for only the exact pending fixed
`local.echo` call. Review returns canonical arguments and resources plus their
confirmation hashes; approve or deny must echo both hashes. Review and the
confirmed decision each reconstruct and revalidate the frozen Provider request,
so they carry the same credential, Usage, quota, and billing warning. Mutating
control opens existing Ledgers under an exclusive lock and rejects incomplete
tails without repair; it never admits a root Agent or converts a numeric Agent
ID into authority.
Tool call identity, argument hashing, approval binding, independent
authority checks, and ambiguous-effect reconciliation are durable core policy.
The product has a private `local.echo` process tracer: it launches only a fixed
same-binary child after the durable effect boundary, clears ambient environment
and working-directory state, bounds I/O and time, terminates a Unix process
group, and assigns the Windows child to a constrained Job Object before
execution. It rejects filesystem and network resources and is not a general
process sandbox, network Tool, or MCP adapter.

`headless --tool local.echo` now composes that adapter with the recoverable
Agent Team, Tool Runtime, and Provider Turn driver. It writes the durable Team
operation receipt and exact Tool approval request to stderr, accepts only an
explicit `approve` or `deny` decision from stdin, and acknowledges final output
only after stdout is flushed. A restart before approval re-presents the durable
request; a successful or ambiguous effect is never automatically repeated.
`tool status` inspects the Tool sidecar without creating or repairing state.
After external investigation, `tool reconcile` records one observed failure or
an observed-success SHA-256 digest for the original call; it never reruns the
effect. A same-binary crash test kills the product after `EffectPrepared` is
durable and executor entry is observed, then proves restart blocking, explicit
reconciliation, and later Turn admission. A separate 18-case core matrix kills
same-binary children after executor return and around terminal outcome-frame
writes for success, failure, and ambiguous results. Restart either recognizes a
complete outcome or requires reconciliation, while the external effect remains
exactly once; this does not claim real power-loss or exhaustive byte-offset
coverage.

Configured OpenAI-compatible Responses and Chat Completions profiles, the
official DeepSeek Responses, Chat Completions, and Anthropic-compatible Messages
profiles, plus release-catalog OpenCode Go Chat Completions models and the
GPT-5.6 Luna Responses pair now run through
no-proxy, no-redirect HTTPS clients, origin-bound credential lookup, bounded
streaming decode, and a fixed deadline. DeepSeek Chat uses Bearer authorization,
the explicit `/chat/completions` route, `max_tokens`, and non-thinking mode; it
caps an explicit output limit at 384K and rejects preset reasoning effort or
service tier before network I/O. Messages uses `x-api-key`, pins the
Anthropic API version header, explicitly disables DeepSeek's default thinking
mode, and sends the selected Model Preset output limit as `max_tokens`, with a
conservative 4096 fallback when no limit is selected. OpenCode Go Chat uses
Bearer authorization, the frozen Chat route (template default
`/chat/completions`), and
`max_completion_tokens`. It admits only release-catalog-verified Chat models
before credential lookup and rejects preset reasoning effort or service tier
before network I/O. OpenCode Go Responses uses Bearer authorization, the frozen
Responses route (template default `/responses`), and `max_output_tokens`. It
admits only the exact GPT-5.6 Luna Responses catalog pair before credential
lookup, rejects preset reasoning effort or service tier before network I/O, and
supports one stateful Tool continuation through the same durable approval
boundary. Config Runtime freezes the
normalized origin, selected-dialect route, dialect, pricing decision, and opaque
credential reference in the Provider Epoch; adapter reconstruction uses that
snapshot without changing an explicit dialect. Private loopback fixtures retain
synthetic authorization for deterministic transport tests. Each core decoder
normalizes one function call, crosses durable Tool approval/effect policy,
continues the Provider once with its dialect-specific wire shape, and prepares
canonical output without repeating successful or ambiguous effects. Provider
continuation correlation remains process-local, so restart after a completed
Tool effect blocks instead of reconstructing or repeating it. Headless execution
keeps the deterministic simulator when no custom profile is selected.
Current configured Provider Epochs freeze an explicit dialect. Historical
pre-dialect Epochs retain their schema-compatible Responses default and still
must pass the frozen Profile's adapter and capability checks during replay.
DeepSeek Responses uses Bearer authorization and `/responses`, supports only V4
Flash, maps `max_output_tokens` plus `reasoning.effort` values `low`, `high`, and
`max`, and rejects service tier before network I/O. Its stateless Tool
continuation reconstructs bounded input items instead of using
`previous_response_id`. Reasoning text is bounded and transition-validated by
the dialect decoder but is not projected as visible output or persisted raw.
Runtime Event schema 12 preserves schema 11's redacted Provider-unavailability
stage and durable `TurnRetryRequested` recovery, schema 10's typed block origin
and `TurnCancelled`, and schema 9's selected Preset policy. It adds an exact-head
Context checkpoint event.
OpenAI Responses sends reasoning as `reasoning.effort`, OpenAI Chat Completions
sends `reasoning_effort`, and both send `service_tier`; their output-token
fields are `max_output_tokens` and `max_completion_tokens`. DeepSeek Chat and
Messages send the output limit as `max_tokens` but reject a selected reasoning
effort or service tier because their reasoning blocks and tier semantics are
not yet mapped by these adapters.
Unset fields remain omitted, except Messages retains its 4096 token fallback.
One in-process Tool continuation uses the same policy; replay reconstructs it
after restart without making continuation resumable. Requested effort/tier
enter durable Usage Attempts separately from observed Provider metadata.
Historical schema 8 preserves the schema-7 request-policy and schema-6 Usage and Cost contract, which
brackets every Provider invocation with a durable
Usage Attempt, records UTC start/completion and outcome, preserves exact,
estimated, and unknown token classes, and rebuilds cached Turn, Thread, Agent,
Team, rolling, and named-window rollups. Config Epochs freeze normalized Usage
Windows with concrete IANA identity and rule-set provenance plus resolved,
versioned Price Schedules. A cost-evaluation event follows each Usage Attempt
in the same transaction, freezes the matching schedule, and records an exact,
estimated, or explicit-unknown pay-as-you-go Cost Estimate with checked integer
arithmetic. Replay recalculates the estimate from the frozen evidence and rejects
tampering; `stats` reads the cached projection without exposing prompt text.
The unchanged bare `stats` command preserves the complete legacy JSON snapshot.
Explicit `--summary-only` and `--limit 1..1000` modes instead return a
revision-stamped report; bounded pages carry a checksummed `next_cursor` tied to
the Ledger head and requested instant, so an append makes an old cursor fail
stale rather than mixing revisions.
Core now also has a deterministic Context foundation. Its Pressure projector combines an
explicit context limit, used-token fact, output reserve, and exact/estimated
marker with checked integer arithmetic and default 65% soft / 90% hard
thresholds. Missing facts remain explicitly unknown. The optional Runtime
admission path rejects a known hard-pressure Turn before any Ledger append or
Provider call. Soft pressure publishes a bounded checkpoint at a Safe Barrier
before the next Turn; unknown pressure admits without inventing a checkpoint. The
terminal-neutral status projection serializes the immutable facts and marks an
estimate as `ctx ~N%`.

The Context View projects ordered canonical Item/Turn/role facts from an exact
Ledger head. Reduction keeps a configurable recent raw tail and replaces older
text with Item-bound SHA-256, byte-count, role, and token-estimate references;
the Event Ledger remains authoritative. Runtime Event schema 12 publishes that
projection only at a Ready Safe Barrier, verifies the prior Ledger head on
replay, rejects stale drafts without writing, and rebuilds every checkpoint
from complete canonical Items instead of recursively reducing summaries.
`context status` inspects this state without creating or repairing a Ledger.
`context reduce` explicitly publishes one bounded checkpoint, checks complete
Product sidecars when present, and does not change Team or Tool Ledger bytes.
Provider-request consumption, semantic Compactor output, provider-native
compaction, external Artifact storage, and Durable Memory remain Phase 6 work.
Editable Config schedules require manual provenance. The bundled DeepSeek rate
card supplies versioned official schedules. A custom DeepSeek origin with no
pricing override defaults to an immutable `template_mirror` estimate; explicit
`unknown`, `manual`, or `provider_reported` decisions win. Mirror provenance
does not carry the official credential, endpoint authority, or a claim about a
gateway's hidden backend. Templates without a bundled rate card still require
an explicit custom-origin pricing decision. Provider-reported charges and
subscription quota values remain separate from estimates.
The first terminal-neutral presentation slice now derives a bounded hierarchical
Slash Panel, configured-preset selector, adaptive status summary, and explicit
Runtime, Team, Tool, and Config blockers from core snapshots. Config Schema
metadata supplies every field-level editor route and the Config Runtime exposes
provenanced, credential-safe field views for existing Provider Profiles, Model
Presets, Price Schedules, and Usage Windows. Config Runtime can now open a selected field route as
one revision-bound draft, preview the normalized diff through the real validation
and locking path, reset it, and commit it atomically. Credential routes expose
binding state; the TUI may replace only the opaque reference. From the clean
Provider credential field, F7 opens a separate secure-store flow for hidden
bind/replace input, status-only test, and confirmed forget. Existing values and
scope identifiers are never rendered. The product
now also has typed nested Config Object add/remove routes. One schema-driven
Draft can create a Profile, Preset, Price Schedule, or Usage Window across multiple focused
fields; whole-object deletion is target-layer explicit and reference validated.
The terminal-neutral interaction controller projects these operations alongside
deterministic 40/80/160 column row layouts with Unicode-safe fitting and adaptive
status degradation. Provider Profile create/edit routes now use a purpose-built
terminal-neutral wizard over the same revision-bound Draft. It can build the
validated, frozen candidate Profile and explicitly test its configured `models`
route with one bounded, no-proxy, no-redirect GET. Results expose only a fixed
status category, retryability, the candidate identity, and a bounded model-list
observation. A valid JSON response may contribute at most 1,024 unique model
IDs of at most 256 bytes each inside a 256 KiB body. Exact release-catalog IDs
carry their release key; unknown IDs carry no dialect, capability, or execution
authority. The check does not commit the Draft, merge a live catalog, update a
Provider Epoch, or return endpoint or credential data. The CLI offers the same
read-only observation for the currently selected committed Profile through
`config test-provider`. The explicit `config discovery refresh PROFILE` path
reuses that bounded probe for a committed Profile and writes only a separate
schema-versioned local observation after success. `status` is missing-safe,
`catalog PROFILE` merges the release seed with the latest
Profile-fingerprint-bound observation, and failed refreshes preserve the last
successful bytes. Unknown discovered IDs remain non-executable until
`config discovery accept PRESET_ID PROFILE MODEL --dialect DIALECT --scope
user|project` creates an ordinary revision-bound Model Preset. Stale
observations and unknown IDs fail before Config write or credential lookup.
Dirty drafts cannot be discarded implicitly, and failed validation or revision
conflicts leave the editor live. The public `tui` command enters the alternate
screen and raw mode, maps blocking Crossterm key and resize events into the
existing controller, emits a Unicode-aware Direct VT cell diff, suppresses
identical frames, clears stale cells, and restores the terminal on normal and
error returns. Every Config Schema field now has a rendered interaction. Choice
fields stage with Up/Down, preview with Enter, commit with `c`, and discard with
`d`; bounded text, integer, boolean, and TOML-list fields preview with Enter and
commit with a second Enter. Top-level Provider/model/output-limit and statusline
routes open directly. Object field routes enter a kind-filtered Config Center,
select an existing Profile, Preset, Price Schedule, or Usage Window, and open the
exact field. Invalid previews and revision conflicts render a bounded notice and
keep the Draft live; Escape, Ctrl-C, and Ctrl-Q do not implicitly discard it. A
no-change commit does not create a Config file, and tests reopen committed files.
`/config provider add` now opens a bounded lowercase Profile-ID prompt, then a
release-template choice. Tab and Shift-Tab move across template, opaque
credential reference, base URL, four routes, dialect list, catalog mode, pricing
source, and explicit insecure-loopback permission. Credential references are
never rendered or read back; F7 performs the separate origin-bound credential
operation without changing Config or a Ledger. Replace and forget require an
extra confirmation, and a changed Profile origin/reference discards any pending
secret before dispatch. The same fields edit existing Profiles. Core validation still owns
route normalization, dialects, pricing provenance, and loopback-origin policy.
Within the wizard, F5 runs the existing bounded connection and model-list test
against the current revision-bound candidate. The rendered status is
ephemeral, a staged change resets it, and a stale revision stops before the
tester. The action does not commit Config, mutate a Provider Epoch, merge a
catalog, expose the credential reference, or grant execution authority. The
current terminal action runs synchronously under the tester's 10-second timeout.
The typed `/config provider|model|pricing|stats-window remove` routes carry the
pending command into a section-filtered object selector and then render an exact
target confirmation. Enter dry-runs and CAS-commits the deletion; Escape cancels
it. Reference validation or revision conflict leaves the confirmation live, and
a real-key test reopens Config to prove the selected object is absent. The
delete action does not write a Ledger or grant Runtime or Provider authority.
`/config model add` now prompts for a bounded Model Preset ID and moves across
all nine fields. Provider, model, and dialect are required; reasoning effort,
service tier, maximum output tokens, context mode, favorite, and explicit
fallback list are optional. Enum and boolean values use schema-owned choices;
numeric and TOML-list input is buffered until it parses atomically. Existing
Preset field routes use the same selector and Draft. Tests commit and reopen
required and optional values. Missing references, fallback cycles, invalid
limits, and revision conflicts keep the Draft live for repair or explicit
discard. This form defines and edits Presets; the separate `/model` flow can
accept one compatible release candidate into an ordinary user-owned Preset,
then apply a configured Preset. Automatic starter offers and updates remain
unavailable.
`/config stats-window add` now prompts for a bounded Usage Window ID and moves
across start, end, weekday-list, and IANA-time-zone fields in one user-scope
Draft. Each input is bounded to 512 bytes. The weekday list accepts a TOML
string array; incomplete structured text remains visible and dirty until Tab or
Enter parses it atomically. Enter on the final text field previews, a second
Enter CAS-commits, and tests reopen the file to prove the resolved window
survives. Invalid arrays, invalid window rules, and revision conflicts keep the
Draft live for correction or explicit discard. This does not automatically
rebuild the running TUI usage projection.
`/config pricing add` now completes the rendered Config Object creation set. It
prompts for a bounded Price Schedule ID and moves through all 17 schema fields:
13 required values plus optional dialect, service tier, maximum context, and
effective-until selectors. The Provider Profile ID is bounded to 64 bytes; all
other text and non-negative-integer inputs are bounded to 512 bytes. Editable
schedules offer only the valid `manual` provenance choice and require the
referenced Profile to use manual pricing. Partial integer input remains local
and dirty until Tab or Enter parses it. Enter on the final reasoning-output rate
previews, a second Enter CAS-commits, and tests reopen the resolved schedule.
Invalid selector ranges, effective intervals, provenance, and stale revisions
retain the Draft for repair or explicit discard. This does not rebuild the
running TUI's frozen Price Schedule book or add rich cost presentation.
`/model` now opens a browser over the latest successful local snapshot of
configured Presets, release-catalog candidates, and bounded Provider-discovery
observations. Character input filters the snapshot, Tab and
Shift-Tab move across Favorites, Recent, Compatible, and All, Up/Down move the
selection, and Enter opens bounded source, freshness, availability, and
capability detail. Recent is derived only from durable Usage Attempts and is
known-empty when no attempts exist. On compatible release detail, a second
Enter requests a bounded Preset ID, opens a prefilled user-scope Draft for the
exact Profile/model/dialect, and uses the normal preview/CAS-commit flow. On a
current discovered-model detail, a second Enter requests the ID and then an
explicit trusted dialect from the current Profile before entering that same
ordinary Draft/CAS flow. The Profile fingerprint, observation timestamp, and
exact model are revalidated immediately before Draft creation; drift returns to
the browser without writing Config. F5 is the only Provider-discovery action:
it synchronously probes the selected Profile, atomically replaces its separate
discovery observation only after success, and preserves the previous file and
browser on failure. F6 or Ctrl-R remains a local-only snapshot reload and never
contacts a Provider. The accepted result is an ordinary user-owned Preset;
duplicate IDs, incompatible Profiles, validation failures, and stale revisions
never overwrite Config. A second Enter on configured detail durably selects
that Preset for the existing current
Agent's next Turn; the pending ID is visible and another configured Preset
replaces it. Nothing is installed or executed implicitly. Selection
authenticates the recovered Active Agent Session,
writes no Config, reads no credential, and performs no Provider request. The
next headless Turn without an explicit Preset resolves the exact pending ID,
rechecks its Config fingerprint and identity, and consumes the selection in the
same admission transaction that freezes Config and Provider Epochs. Config
drift, an explicit conflicting Preset, missing credentials, or unsupported
Provider policy fails before Provider execution and preserves the pending
selection. Running child Agents are unchanged.
`/stats` now browses the latest successful Usage snapshot. It shows 1-hour, 1-day,
and 7-day summaries. Tab and Shift-Tab move across Attempts, Turn,
Provider & Model, Dialect & Policy, current Thread, Agent, Team, Named Window,
and Token & Cache groups; Up/Down selects a row and Enter opens bounded detail.
Turn detail uses the cached Turn rollup. Provider, requested/observed model,
dialect, reasoning, and requested/observed service-tier distributions remain
scoped to their Turn and keep unknown values explicit. Attempt detail includes
provider, model, policy, token, cost, outcome, and timing facts. Aggregate
detail preserves exact, estimated, unknown, and overflow token/cost states.
Token & Cache and every scoped rollup detail also show token-weighted
cache-read/input and cache-write/input ratios. Exact and estimated records stay
separate; missing, internally inconsistent, and overflowed facts stay explicit.
The adaptive statusline shows compact 1-hour read/write ratios when width permits.
`/agent` now browses the latest successful read-only Team sidecar projection.
Up/Down selects
canonical Agents and Enter shows status, Task identity and state, budgets,
reservations, and bounded metadata counts. It never renders Task titles,
message bodies, Completion Capsules, capability or scope labels, or
process-local Agent Session
authority. Missing Team state stays unavailable without creating files; an
incomplete final frame is reported as recovery required without truncation or
repair, while corruption and incomplete Product sidecars fail closed. Browsing
does not acknowledge Team operations or mutate Runtime, Team, Tool, or Config
state.
`/blockers` now lists the latest Runtime, Team, Task, Tool, and Config blockers.
The list and its non-Tool details come from local snapshots. Enter on a Tool
approval first shows a local recovery warning. A second Enter explicitly
recovers the current Active Agent Session and frozen Provider Epoch, resumes
the pending Provider request, and renders the exact canonical arguments plus
filesystem, process, and network resources before a decision is available.
This confirmed recovery may perform a Provider request and credential lookup,
append Usage Attempt and cost records, and affect Provider quota or billing;
raw arguments and resources remain ephemeral and are not added to a Ledger.
Up/Down traverses every rendered detail row before the
Approve and Deny choices. Escape drops the in-memory approval context and leaves
the durable call awaiting approval. Denial never invokes the executor. Approval
uses the existing bound call identity, argument hash, resources, capability, and
Agent Session checks, then runs only the fixed `local.echo` effect. Prepared
Provider output remains on screen until its delivery acknowledgement succeeds;
failed recovery or resolution returns to blocker inspection, while failed
acknowledgement keeps the same output visible and retryable.
Config commits do not automatically rebuild the running TUI projection. From the
Slash Panel, `/model`, `/stats`, `/agent`, or `/blockers`, F6 or Ctrl-R reads a new local
Config/statusline, Model, Usage, and Team snapshot, then replaces the prior view
only after every read succeeds. A failed refresh keeps the previous complete
view and remains retryable; refresh never performs a
Provider request, credential lookup, Config/Ledger write, or Team action. There
is no background polling or automatic refresh. Runtime, Team, and Tool Ledgers
are inspected independently, so one refresh is not a cross-Ledger transactional
snapshot. The terminal and App Server approval surfaces are limited to the exact
pending `local.echo` call; neither is a general Tool policy editor, audited
ConPTY integration, automatic starter-update workflow, or automatic/on-open
Provider discovery. Live inference conformance,
automatic retry policy or partial-stream reconnect, OpenCode Go Messages execution,
Messages reasoning blocks, Preset context/fallback execution, broader multi-Tool approval
presentation, broader Provider and Tool adapters,
Workspace, project/new-Agent Preset defaults,
Agent lifecycle actions, general App Server Runtime control beyond the exact
cancel/retry/resume recovery flow, and
remote App Server transport remain. The loopback Provider tracer remains
an internal harness; `local.echo` is intentionally a fixed opt-in command rather
than a general process runner. The file Ledger remains
provisional. The acceptance runner can emit bound raw evidence,
but is not yet a full Target Acceptance Run. Follow the
[implementation plan](docs/implementation-plan.md).

## License

Apache License 2.0. See [LICENSE](LICENSE).
