# Architecture decision records

GreenTyper records decisions here when they constrain later implementation. Each ADR describes the accepted direction, its consequences, and the alternatives that were rejected.

## Product and runtime

- [0001 - Build an independent Codex-compatible runtime](0001-build-an-independent-codex-compatible-runtime.md): keep protocol familiarity without inheriting another product's internal architecture.
- [0002 - Prioritize constrained Windows performance](0002-prioritize-constrained-windows-performance.md): make the loaded target laptop, not the development Mac, the performance authority.
- [0003 - Own the canonical runtime model](0003-own-the-canonical-runtime-model.md): normalize provider behavior behind GreenTyper-owned domain types.
- [0004 - Preserve the Event Ledger across compaction](0004-preserve-the-event-ledger-across-compaction.md): compact model context without rewriting authoritative history.
- [0005 - Run Agents as resource-budgeted state machines](0005-run-agents-as-resource-budgeted-state-machines.md): bound concurrency and make Agent lifecycle explicit.
- [0006 - Exclude an in-process plugin ABI](0006-exclude-an-in-process-plugin-abi.md): keep extension code outside the trusted runtime process.
- [0007 - Protect secrets separately from the Ledger](0007-protect-secrets-separately-from-the-ledger.md): keep credential material out of the Ledger and checkpoints while configuration holds only secure-store references.

## Providers and effects

- [0008 - Separate Provider Dialect, Transport, and Context Mode](0008-separate-provider-dialect-transport-and-context-mode.md): compose API shape, connection mechanism, and context behavior independently.
- [0009 - Make tool effects idempotent across retries](0009-make-tool-effects-idempotent-across-retries.md): persist effect identity and outcome before deciding whether retry is safe.

## Agent teams and authority

- [0010 - Coordinate Agents through an owned Task graph](0010-coordinate-agents-through-an-owned-task-graph.md): use a bounded DAG with explicit ownership instead of free-form spawning.
- [0011 - Isolate concurrent Writers by worktree](0011-isolate-concurrent-writers-by-worktree.md): prevent concurrent Agents from silently overwriting workspace changes.
- [0012 - Never expand capability through Delegation](0012-never-expand-capability-through-delegation.md): child authority must be a subset of parent authority.

## Skills, MCP, memory, and context

- [0013 - Pin Skill Invocations by content identity](0013-pin-skill-invocations-by-content-identity.md): make every invocation reproducible even when a Skill changes later.
- [0014 - Freeze Capability Snapshots for each Turn](0014-freeze-capability-snapshots-for-each-turn.md): prevent tools and policies from changing during an inference loop.
- [0015 - Share MCP connections with isolated capability views](0015-share-mcp-connections-with-isolated-capability-views.md): reuse transport resources without sharing authority.
- [0016 - Promote Memory only through evidence](0016-promote-memory-only-through-evidence.md): make durable Memory an explicit, attributable promotion.
- [0017 - Build portable Checkpoints at Safe Barriers](0017-build-portable-checkpoints-at-safe-barriers.md): checkpoint only at states that can be restored without inventing outcomes.
- [0018 - Isolate Compaction from authority and Memory](0018-isolate-compaction-from-authority-and-memory.md): summaries may reduce context but cannot grant rights or become durable facts implicitly.

## Configuration, security, and delivery

- [0019 - Freeze resolved configuration for each Turn](0019-freeze-resolved-configuration-for-each-turn.md): bind every Turn to one inspectable Config Epoch.
- [0020 - Separate tool authority by resource](0020-separate-tool-authority-by-resource.md): approve filesystem, process, and network access independently.
- [0021 - Keep observability local by default](0021-keep-observability-local-by-default.md): collect useful local diagnostics without remote telemetry.
- [0022 - Preserve stored state over external parity](0022-preserve-stored-state-over-external-parity.md): prioritize migration-safe state compatibility over command-line imitation.
- [0023 - Ship a portable Windows-first release](0023-ship-a-portable-windows-first-release.md): distribute a signed, non-resident package suited to locked-down machines.

## Catalogs, usage, and interaction

- [0024 - Separate Provider Templates, Model Catalogs, and Model Presets](0024-separate-provider-templates-catalogs-and-presets.md): keep connection defaults, model facts, and user choices independently replaceable.
- [0025 - Record Usage before deriving cost and stats](0025-record-usage-before-deriving-cost-and-stats.md): preserve immutable usage evidence and derive changing views later.
- [0026 - Bind Provider assumptions to origins](0026-bind-provider-assumptions-to-origins.md): do not transfer pricing or capabilities from an official API to an unknown gateway.
- [0027 - Make every Config Object UI-editable](0027-make-every-config-object-ui-editable.md): generate safe UI and headless configuration surfaces from one schema.
- [0028 - Keep slash commands hierarchical](0028-keep-slash-commands-hierarchical.md): keep the root panel small while making deep configuration discoverable.

## Repository structure

- [0029 - Organize code as a three-package Cargo workspace](0029-organize-code-as-three-package-workspace.md): keep runtime policy deep, dependency direction simple, and acceptance delivery isolated.
- [0030 - Use a temporary public CI mirror](0030-use-a-temporary-public-ci-mirror.md): keep private development canonical while hosted builds run against an explicitly disposable public mirror.
