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
cargo run -p greentyper -- headless --ledger ./target/dev-runtime.ledger --input "hello"
cargo run -p greentyper -- headless --preset frontier --ledger ./target/dev-runtime.ledger --input "hello"
cargo run -p greentyper -- headless --ledger ./target/tool-runtime.ledger --tool local.echo --input "echo this"
cargo run -p greentyper -- status --ledger ./target/dev-runtime.ledger
cargo run -p greentyper -- stats --ledger ./target/dev-runtime.ledger
cargo run -p greentyper -- stats --ledger ./target/dev-runtime.ledger --summary-only
cargo run -p greentyper -- stats --ledger ./target/dev-runtime.ledger --limit 100
cargo run -p greentyper -- config schema
cargo run -p greentyper -- config catalog
cargo run -p greentyper -- config get provider.model
cargo run -p greentyper -- config test-provider
```

For a configured OpenAI or openai-compatible Profile, `headless` accepts
`--dialect responses` or `--dialect chat_completions`. A configured DeepSeek
Profile accepts `--dialect messages`; every other template/dialect pair fails
closed unless the product has an explicit adapter for that exact identity.
`headless --preset ID` resolves one configured Model Preset by exact ID and
freezes its Profile, model, dialect, and optional output-token limit for the
Turn. It cannot be combined with `--dialect`; missing IDs fail rather than
falling back to a model-name match.

The core Agent Team policy, Config Runtime, recoverable single-Agent Runtime,
and first Tool Runtime policy slice compile and run through interface-level and
cross-process headless tests. Config currently includes versioned TOML, drafts,
provenance, atomic replacement, repair, typed Provider Profile snapshots, and a
terminal-neutral schema-driven editor session and Provider Profile wizard. A
release-bundled Provider Catalog now supplies versioned OpenAI, DeepSeek, and
OpenCode Go template defaults plus seed model facts with field provenance. The
selector exposes compatible release candidates while keeping unverified live
availability explicit. Rendered TUI/App Server editors remain pending. The product
CLI can bind, replace, test, and forget origin-bound credential references without
putting secret material in arguments, Config, or Ledgers. Windows stores values
in the current user's Credential Manager; other platforms currently fail
closed. Tool call identity, argument hashing, approval binding, independent
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

Configured OpenAI-compatible Responses and Chat Completions profiles plus the
official DeepSeek Anthropic-compatible Messages profile now run through
no-proxy, no-redirect HTTPS clients, origin-bound credential lookup, bounded
streaming decode, and a fixed deadline. Messages uses `x-api-key`, pins the
Anthropic API version header, explicitly disables DeepSeek's default thinking
mode, and sends the selected Model Preset output limit as `max_tokens`, with a
conservative 4096 fallback when no limit is selected. Config Runtime freezes the
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
Runtime Event schema 7 freezes the selected Preset's optional output-token
limit, typed reasoning effort, and typed service tier in the Config Epoch.
Responses sends reasoning as `reasoning.effort`, Chat Completions sends
`reasoning_effort`, and both send `service_tier`; their output-token fields are
`max_output_tokens` and `max_completion_tokens`. Messages sends the output limit
as `max_tokens` but rejects a selected reasoning effort or service tier because
its reasoning blocks and tier semantics are not yet mapped by this adapter.
Unset fields remain omitted, except Messages retains its 4096 token fallback.
One in-process Tool continuation uses the same policy; replay reconstructs it
after restart without making continuation resumable. Requested effort/tier
enter durable Usage Attempts separately from observed Provider metadata.
Schema 7 also preserves the schema-6 Usage and Cost contract, which
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
Core now also has a deterministic Context Pressure projector. It combines an
explicit context limit, used-token fact, output reserve, and exact/estimated
marker with checked integer arithmetic and default 65% soft / 90% hard
thresholds. Missing facts remain explicitly unknown. The optional Runtime
admission path rejects a known hard-pressure Turn before any Ledger append or
Provider call; soft and unknown projections preserve the existing path. The
terminal-neutral status projection serializes the immutable facts and marks an
estimate as `ctx ~N%`. Automatic Context View construction, reduction,
compaction, checkpoints, and provider-native adapters remain Phase 6 work.
Editable Config schedules require manual provenance. Provider-reported charges,
trusted template rates, and subscription quota values remain separate and are
not inferred from these estimates.
The first terminal-neutral presentation slice now derives a bounded hierarchical
Slash Panel, configured-preset selector, adaptive status summary, and explicit
Runtime, Team, Tool, and Config blockers from core snapshots. Config Schema
metadata supplies every field-level editor route and the Config Runtime exposes
provenanced, credential-safe field views for existing Provider Profiles, Model
Presets, Price Schedules, and Usage Windows. Config Runtime can now open a selected field route as
one revision-bound draft, preview the normalized diff through the real validation
and locking path, reset it, and commit it atomically. Credential routes expose
binding state and require the separate secure credential operation. The product
now also has typed nested Config Object add/remove routes. One schema-driven
Draft can create a Profile, Preset, Price Schedule, or Usage Window across multiple focused
fields; whole-object deletion is target-layer explicit and reference validated.
The terminal-neutral interaction controller projects these operations alongside
deterministic 40/80/160 column row layouts with Unicode-safe fitting and adaptive
status degradation. Provider Profile create/edit routes now use a purpose-built
terminal-neutral wizard over the same revision-bound Draft. It can build the
validated, frozen candidate Profile and explicitly test its configured `models`
route with one bounded, no-proxy, no-redirect GET. Results expose only a fixed
status category, retryability, and the candidate identity; they do not commit the
Draft, read a response body, or return endpoint or credential data. The CLI
offers the same status-only check for the currently selected committed Profile
through `config test-provider`.
Dirty drafts cannot be discarded implicitly, and failed validation or revision
conflicts leave the editor live. This is not an ANSI/VT backend, live keyboard
loop, ConPTY integration, rendered Config Center, object-name dialog, or
rendered template picker, starter-preset workflow, or live catalog discovery.
Live credential-gated provider validation, configurable proxy policy,
reconnect/retry, DeepSeek Responses/Chat Completions, all OpenCode Go execution,
Messages reasoning blocks, Preset context/fallback execution, richer approval
presentation, broader Provider and Tool adapters,
Workspace, TUI, and App Server work remain. The loopback Provider tracer remains
an internal harness; `local.echo` is intentionally a fixed opt-in command rather
than a general process runner. The file Ledger remains
provisional. The acceptance runner can emit bound raw evidence,
but is not yet a full Target Acceptance Run. Follow the
[implementation plan](docs/implementation-plan.md).

## License

Apache License 2.0. See [LICENSE](LICENSE).
