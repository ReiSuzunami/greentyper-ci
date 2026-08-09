# GreenTyper

[![Temporary public CI](https://github.com/ReiSuzunami/greentyper-ci/actions/workflows/ci.yml/badge.svg)](https://github.com/ReiSuzunami/greentyper-ci/actions/workflows/ci.yml)

GreenTyper is a Windows-first coding-agent runtime written in Rust. It is designed to remain responsive on memory-constrained developer laptops while supporting recoverable long-running work, provider portability, first-class Agent Teams, Skills, MCP, Durable Memory, and Compaction.

GreenTyper is an independent product with selected Codex-compatible protocol and Agent semantics. It is not a Codex CLI clone, a command-compatible replacement, or a wrapper around another agent process.

> Status: feature implementation started. The first runnable core slice implements deterministic Agent Team orchestration policy; no complete product workflow exists yet.

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
```

The core Agent Team policy slice compiles and runs through interface-level tests. Presentation, persistent storage, Provider, Tool, and Workspace behavior remain unimplemented; follow the [implementation plan](docs/implementation-plan.md).

## License

Apache License 2.0. See [LICENSE](LICENSE).
