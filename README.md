# GreenTyper

[![Temporary public CI](https://github.com/ReiSuzunami/greentyper-ci/actions/workflows/ci.yml/badge.svg)](https://github.com/ReiSuzunami/greentyper-ci/actions/workflows/ci.yml)

GreenTyper is a Windows-first coding-agent runtime written in Rust. It is designed to remain responsive on memory-constrained developer laptops while supporting recoverable long-running work, provider portability, first-class Agent Teams, Skills, MCP, Durable Memory, and Compaction.

GreenTyper is an independent product with selected Codex-compatible protocol and Agent semantics. It is not a Codex CLI clone, a command-compatible replacement, or a wrapper around another agent process.

> Status: feature implementation is active. The repository now contains deterministic Agent Team policy, a recoverable single-Agent headless spine, a bounded OpenAI Responses SSE decoder, and a core Tool Runtime seam with durable call identity, approval binding, prepared-effect ordering, and explicit reconciliation. It is not yet a complete coding-agent product.

> Repository topology: [`ReiSuzunami/greentyper`](https://github.com/ReiSuzunami/greentyper) is the private canonical repository. [`ReiSuzunami/greentyper-ci`](https://github.com/ReiSuzunami/greentyper-ci) is a temporary public, non-authoritative mirror used only for hosted CI and build artifacts. See the [repository policy](docs/repository-policy.md).

## Product Shape

- Windows 11 x64 first, targeting x86-64-v3 and modern VT/ConPTY terminals.
- Keyboard-first TUI plus a headless App Server; IDE and graphical clients come later.
- One resource-budgeted runtime process for logical Agents, with controlled child processes only for tools.
- Canonical provider-neutral Threads, Turns, Items, Tasks, and Events.
- OpenAI Responses, OpenAI Chat Completions, and Anthropic Messages dialects behind adapters.
- Built-in Provider Templates and a seed Model Catalog for OpenAI, DeepSeek, and OpenCode Go, with user-defined gateway URLs.
- Append-only Event Ledger as truth; Context Views, checkpoints, memory retrieval, usage statistics, and indices remain derived.
- Evidence-linked Durable Memory and safe-barrier Compaction that cannot acquire authority or erase history.
- Explicit task ownership, capability Delegation, workspace leases, and worktree isolation for Agent Teams.
- Schema-driven configuration available through TUI dialogs, CLI, App Server, and TOML.

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
cargo run -p greentyper -- status --ledger ./target/dev-runtime.ledger
cargo run -p greentyper -- config schema
cargo run -p greentyper -- config get provider.model
```

The core Agent Team policy, Config Runtime, recoverable single-Agent Runtime,
and first Tool Runtime policy slice compile and run through interface-level and
cross-process headless tests. Config currently includes versioned TOML, drafts,
provenance, atomic replacement, and repair, but not the eventual TUI/App Server
editors, catalogs, or credential store. Tool call identity, argument hashing,
approval binding, independent authority checks, and ambiguous-effect
reconciliation are durable core policy. The product now has a private
`local.echo` process tracer: it launches only a fixed same-binary child after
the durable effect boundary, clears ambient environment and working-directory
state, bounds I/O and time, terminates a Unix process group, and assigns the
Windows child to a constrained Job Object before execution. It rejects
filesystem and network resources and is not a general process sandbox,
network Tool, or MCP adapter. The product also has a private loopback Responses
HTTP tracer with a real no-proxy, no-redirect client, bounded streaming decode,
a fixed deadline, synthetic authorization, and fixed request validation. Its
Config Runtime resolves a typed Provider Profile snapshot and freezes the
normalized loopback origin, Responses route, dialect, pricing decision, and
opaque synthetic credential reference into the Provider Epoch. It is not a
remote Provider or real credential-routing path. The core has bounded generic
SSE framing and a strict OpenAI Responses dialect decoder for text, function
calls, terminal states, and optional usage. A core fixture path normalizes one
Responses function call,
crosses durable Tool approval/effect policy, continues the Provider once, and
prepares canonical output without repeating successful or ambiguous effects.
The public product path still uses the deterministic simulator: no production
remote Provider transport, credential-vault routing, user-facing Tool approval/delivery
path, or retry path exists yet. The `local.echo` and loopback Provider tracers
are exercised through internal harnesses rather than public commands. The file
Ledger remains provisional. Real Provider and Tool adapters, Workspace, TUI,
and App Server work remains. The acceptance runner can emit bound raw evidence,
but is not yet a full Target Acceptance Run. Follow the
[implementation plan](docs/implementation-plan.md).

## License

Apache License 2.0. See [LICENSE](LICENSE).
