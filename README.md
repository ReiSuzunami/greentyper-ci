# GreenTyper

[![Temporary public CI](https://github.com/ReiSuzunami/greentyper-ci/actions/workflows/ci.yml/badge.svg)](https://github.com/ReiSuzunami/greentyper-ci/actions/workflows/ci.yml)

GreenTyper is a Windows-first coding-agent runtime written in Rust. It is designed to remain responsive on memory-constrained developer laptops while supporting recoverable long-running work, provider portability, first-class Agent Teams, Skills, MCP, Durable Memory, and Compaction.

GreenTyper is an independent product with selected Codex-compatible protocol and Agent semantics. It is not a Codex CLI clone, a command-compatible replacement, or a wrapper around another agent process.

> Status: feature implementation is active. The repository now contains deterministic Agent Team policy, a recoverable single-Agent headless spine, a bounded OpenAI Responses SSE decoder, and a core Tool Runtime seam with durable call identity, approval binding, prepared-effect ordering, and explicit reconciliation. It is not yet a complete coding-agent product.

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
cargo run -p greentyper -- headless --ledger ./target/tool-runtime.ledger --tool local.echo --input "echo this"
cargo run -p greentyper -- status --ledger ./target/dev-runtime.ledger
cargo run -p greentyper -- stats --ledger ./target/dev-runtime.ledger
cargo run -p greentyper -- config schema
cargo run -p greentyper -- config get provider.model
```

The core Agent Team policy, Config Runtime, recoverable single-Agent Runtime,
and first Tool Runtime policy slice compile and run through interface-level and
cross-process headless tests. Config currently includes versioned TOML, drafts,
provenance, atomic replacement, repair, typed Provider Profile snapshots, and a
terminal-neutral schema-driven editor session, but not rendered TUI/App Server
editors or catalogs. The product CLI can
bind, replace, test, and forget origin-bound credential references without
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

Configured OpenAI-compatible Responses profiles now run through a no-proxy,
no-redirect HTTPS client, origin-bound credential lookup, bounded streaming
decode, and a fixed deadline. Config Runtime freezes the normalized origin,
Responses route, dialect, pricing decision, and opaque credential reference in
the Provider Epoch; `resume` reconstructs the adapter from that snapshot. A
private loopback fixture retains synthetic authorization for deterministic
transport tests. The core also normalizes one Responses function call, crosses
durable Tool approval/effect policy, continues the Provider once, and prepares
canonical output without repeating successful or ambiguous effects. Headless
execution keeps the deterministic simulator when no custom profile is selected.
Runtime Event schema 4 now brackets every Provider invocation with a durable
Usage Attempt, records UTC start/completion and outcome, preserves exact,
estimated, and unknown token classes, and rebuilds cached Turn, Thread, Agent,
Team, rolling, and named-window rollups. Config Epochs freeze normalized Usage
Windows with concrete IANA identity and rule-set provenance; `stats` reads the
projection without exposing prompt text. Price Schedules and provider charge
calculation are not implemented, so cost provenance remains explicitly unknown.
The first terminal-neutral presentation slice now derives a bounded hierarchical
Slash Panel, configured-preset selector, adaptive status summary, and explicit
Runtime, Team, Tool, and Config blockers from core snapshots. Config Schema
metadata supplies every field-level editor route and the Config Runtime exposes
provenanced, credential-safe field views for existing Provider Profiles, Model
Presets, and Usage Windows. Config Runtime can now open a selected field route as
one revision-bound draft, preview the normalized diff through the real validation
and locking path, reset it, and commit it atomically. Credential routes expose
binding state and require the separate secure credential operation. The product
now also has a terminal-neutral interaction controller and deterministic 40/80/160
column row layouts with Unicode-safe fitting and adaptive status degradation.
Dirty drafts cannot be discarded implicitly, and failed validation or revision
conflicts leave the editor live. This is not an ANSI/VT backend, live keyboard
loop, ConPTY integration, or rendered Config Center.
Live-provider validation, configurable proxy policy, reconnect/retry, richer
approval presentation, broader Provider and Tool adapters, Workspace, TUI, and
App Server work remain. The loopback Provider tracer remains an internal
harness; `local.echo` is intentionally a fixed opt-in command rather than a
general process runner. The file Ledger remains
provisional. The acceptance runner can emit bound raw evidence,
but is not yet a full Target Acceptance Run. Follow the
[implementation plan](docs/implementation-plan.md).

## License

Apache License 2.0. See [LICENSE](LICENSE).
