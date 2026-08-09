# Performance Contract

## Purpose

This contract turns low-resource operation into a release condition. Numbers are initial design budgets until the first Target Machine baseline; they may be tightened from evidence. Loosening a limit requires an explicit decision with benchmark evidence.

## Environments

### Target Machine

- Latest Windows 11 on a 16 GB RedmiNotebook 2022.
- Exact SKU and CPU features are fingerprinted by the first Acceptance Run.
- Provisional CPU floor: the weakest plausible 16 GB 2022 configuration, in the Intel Core i5-12450H or AMD Ryzen 5 6600H class.
- Distribution target: Windows x64, x86-64-v3. A v2 fallback is considered only if the fingerprint proves it necessary.
- Target Load: normal Chrome, VS Code, security software, and other work applications already use roughly 80% of physical memory.

### Validation Host

- Name: FMDev
- QEMU virtual machine running Windows Server 2025 Standard, version 10.0.26100
- AMD EPYC 4584PX exposed as four virtual sockets, four assigned cores, and four logical processors
- 8 GB RAM and PowerShell 5.1
- Freely configurable for repeatable correctness and relative performance runs

This fingerprint was read directly from FMDev on 2026-08-09. A changed VM shape starts a new baseline series rather than silently extending the old one.

FMDev is the continuous regression host, not proof of absolute Target Machine performance. GitHub-hosted Windows runners cover correctness. The macOS ARM development machine covers build and core correctness only.

## Absolute Budgets

| Measure | Contract | Measurement boundary |
| --- | ---: | --- |
| TUI input-ready latency | p50 <= 60 ms; p95 <= 100 ms | OS process start until first input can be accepted and rendered |
| Headless idle Private Bytes | <= 25 MB | Main process after settling, no clients, children, or active work |
| Single idle TUI Private Bytes | <= 35 MB | Main process with one terminal attached and one empty Thread |
| Dormant Agent increment | <= 5 MB per Agent | Difference after checkpointing an otherwise equivalent Agent |
| Idle CPU | <= 0.1% | Whole process tree average over five minutes after settling |
| Idle behavior | No periodic polling or redraw | Verified by tracing wakeups and terminal writes |

Private Bytes is the primary memory gate because it reflects committed private memory pressure on Windows. Working Set, peak Working Set, commit size, handle count, thread count, and child-process memory are also reported. A child compiler or user command is reported separately and in total process-tree figures; it is not hidden inside the runtime number.

## Named Workloads

### P0: Cold Input Ready

Launch the signed portable binary into a supported VT terminal with a minimal local configuration and an empty project. Measure process start to accepted and visible input. No provider network request is made.

### P1: Stable Idle

Measure headless and TUI modes after configuration, ledger, and terminal initialization settle. No timer-driven status segment is enabled. Any recurring wakeup requires attribution.

### P2: Single Streaming Turn

Run one deterministic provider fixture through the canonical request, SSE parsing, ledger batching, TUI rendering, and final Usage Record. Report first-event latency, render cadence, CPU, allocation peak, and final memory return.

### P3: Agent Team

Run two Active Agents on the Target Machine and four on FMDev, with additional Agents checkpointed as Dormant Agents. Exercise Task ownership, communications, reads, separate writable worktrees, and Completion Capsules.

### P4: Tool and MCP Lifecycle

Launch a short child command, reconcile its Tool Ledger record, then start and stop a lazy MCP server. Report GreenTyper, child, and total process-tree resources independently.

### P5: Context Pressure and Compaction

Replay a long fixture through artifact offload, deterministic reduction, Runtime Fold, semantic checkpoint, and provider adapter. Verify that memory returns after work and that the Event Ledger remains unchanged.

### P6: Status and Usage Views

Render context occupancy, Thread cost, cache read/write ratios, reasoning effort, service tier, rolling 1-hour/1-day/7-day views, and a named workday Usage Window. Values update from Events and scheduled interval boundaries, not polling.

## Runtime Resource Policy

- One I/O event loop.
- At most two lazily created CPU workers.
- Default Target Machine admission: two Active Agents.
- Agents above the active budget become Dormant Agents rather than resident workers.
- Concurrency may decrease under memory pressure; it does not increase automatically by consuming available RAM.
- Caches are bounded. Large provider/tool output becomes an Artifact.
- No resident updater, background telemetry process, or idle model-catalog refresher.

## Measurement Procedure

1. Record binary hash, GreenTyper version, schema versions, configuration hash, and workload version.
2. Record Windows build, CPU, memory, storage, power mode, virtualization, terminal, security software, and background load.
3. Warm the environment explicitly, then collect at least 30 measured runs for latency workloads.
4. Report raw samples plus p50, p95, minimum, maximum, and dispersion. Do not report only the best run.
5. Compare against both the absolute contract and the latest accepted FMDev baseline.
6. Warn on a repeatable regression above 5%; block above 10%. An absolute-budget violation always blocks.
7. Run the same package against jcode on the same host when available. This comparison is diagnostic; GreenTyper's absolute contract remains the gate.

Network-provider latency is reported separately from local processing. Deterministic fixtures are the regression gate; live provider smoke tests establish integration health, not local performance.

The initial `agent-team-smoke` fixture measures a small in-process policy transaction solely to prove the versioned evidence pipeline, candidate binding, CPU guard, raw samples, and summary calculation. It is not the P3 workload and cannot approve a technology choice or satisfy a numeric release gate.

## Acceptance Package

The Target Machine uses a signed, portable, no-admin executable that completes in about 10 to 15 minutes and produces one ZIP. It fingerprints the machine, runs the named workloads, records raw and summarized measurements, redacts local paths, and includes no prompt content, source code, tool output, or credentials.

Acceptance is asynchronous during the user's workday availability. A release candidate requires:

1. Windows correctness CI passing.
2. FMDev numeric budgets and regression gates passing.
3. A Target Machine Acceptance Run for the release candidate or an explicitly documented waiver when the machine is unavailable.

The package manifest binds the result to a release-candidate ID, source revision, executable SHA-256, workload version, and configuration hash. The returned ZIP receives its own SHA-256 and is attached to that candidate's release evidence record; manual transfer is acceptable, but an unbound ZIP is not evidence. The Release Owner reviews and records the result.

A Target waiver names its reason and approver and applies to one executable hash only. Any binary change invalidates it. A waiver may allow internal circulation of a release candidate, but it cannot satisfy Alpha completion; a passing Target Machine run is required before the candidate is designated Alpha.

The acceptance executable reports its compiled CPU baseline and checks host CPU features before running. The manifest records that check. An x86-64-v2 fallback requires a fingerprint proving the need, an explicit architecture decision, and a separately identified artifact; it is never selected silently.

## Benchmark-gated Technology Choices

The first implementation phase compares these choices under the named workloads before committing:

- WinHTTP-backed transport versus a cross-platform HTTP stack
- Direct VT rendering versus a TUI library
- SQLite WAL versus a custom append log
- Default system allocator versus measured alternatives

The selected option must win on total correctness cost and the Performance Contract, not a single microbenchmark.
