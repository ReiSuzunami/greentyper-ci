# Implementation Plan

## Delivery Rule

Implementation proceeds as runnable vertical slices. Each phase must preserve Ledger recovery, authority boundaries, and measured resource behavior; no phase may defer all testing or performance work to the end.

The repository remains scaffold-only until explicit feature implementation authorization.

## Phase 0: Repository and Measurement Foundation

Create the Cargo workspace, Windows-first build profiles, formatting/lint policy, GitHub CI, schema/version conventions, deterministic fixture harness, benchmark harness, and portable packaging skeleton.

Benchmark WinHTTP versus a cross-platform HTTP stack, direct VT versus a TUI library, SQLite WAL versus a custom append log, and allocator options using minimal representative workloads.

Exit criteria:

- Windows x64 build and test run from a clean checkout.
- macOS ARM builds core modules and runs pure tests.
- FMDev benchmark harness produces versioned raw results.
- Storage, terminal, transport, and allocator choices are recorded from evidence.

## Phase 1: Recoverable Single-Agent Spine

Implement Config Runtime basics, canonical Thread/Turn/Item/Event types, Ledger Store append/replay, Runtime Kernel admission, one logical Agent, deterministic provider simulator, and headless output.

Exit criteria:

- One Turn survives crash and replay without duplicate output acknowledgement.
- Config layers resolve into an immutable Config Epoch.
- Ledger corruption and unsupported schema fail explicitly.
- Headless idle memory and CPU are measured against the contract.

## Phase 2: Provider and Tool Tracer Bullet

Add OpenAI Responses SSE, canonical tool calls, Tool Ledger identities, Approval Grants, one local process tool, Windows process control, usage normalization, and provider retry/recovery behavior.

Exit criteria:

- A real or fixture Responses Turn can call one approved tool and finish canonically.
- Successful and ambiguous effects cannot auto-repeat after injected crashes.
- Credentials stay outside files and Ledgers.
- Provider raw events are diagnostic artifacts, not core state.

## Phase 3: TUI, Config Center, and Observability

Add VT/ConPTY TUI, hierarchical Command Paths, global command palette, Config Schema-driven editors, Provider wizard, model selector, adaptive statusline, Context Pressure, Usage Records/Rollups, `/stats`, and named Usage Windows.

Exit criteria:

- Every Config Object has an interactive editor route.
- `/config pro url` reaches a focused validated gateway editor.
- Narrow and wide terminal golden tests pass.
- Context, cost, cache, effort, tier, rolling, and workday values preserve exact/estimated/unknown states.
- TUI input-ready, idle memory, and idle CPU budgets pass on FMDev.

## Phase 4: Provider Portability

Add Provider Templates and seed catalogs for OpenAI, DeepSeek, and OpenCode Go; Chat Completions and Anthropic Messages adapters; lazy discovery; custom gateway routes; Model Presets; capability probes; explicit fallback chains; and provider/model epoch switching.

Exit criteria:

- Golden fixtures pass for all three dialects.
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
