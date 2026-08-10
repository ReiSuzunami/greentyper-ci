# Measurement Harness

## Purpose

`greentyper-acceptance` owns delivery-specific machine fingerprinting, CPU-baseline checks, workload timing, and evidence output. The initial harness proves that these boundaries work before the full named workloads and technology comparisons land.

The embedded `agent-team-smoke` fixture is synthetic and versioned. One measured run constructs a fresh `TeamRuntime`, admits one root Agent, records one team message, submits one Completion Capsule, and verifies the exact terminal projection. Fixture parsing, CPU detection, executable hashing, machine fingerprinting, JSON serialization, and file I/O stay outside the timed region.

This workload is not P3 and does not exercise persistence, worktrees, child processes, Provider I/O, the TUI, or the complete Agent Team load. Its measurements are harness diagnostics only.

## Commands

The CPU guard reports the baseline compiled into the current binary and fails when required host features are absent:

```text
greentyper-acceptance verify-cpu --expect-baseline x86-64-v3
```

An evidence run requires explicit candidate and source identities. It defaults to 30 measured runs after three warmups:

```text
greentyper-acceptance run \
  --candidate-id rc-0001 \
  --source-revision 0123456789abcdef0123456789abcdef01234567 \
  --output acceptance-rc-0001.json \
  --expect-baseline x86-64-v3
```

The output path must not already exist. This prevents a later run from silently replacing evidence. A partial file from an interrupted run is not valid evidence and must never be repaired by hand.

Evidence is written to a unique file in the destination directory, flushed and synchronized, then committed to the requested name without replacing an existing file. Public CI passes `--machine-identifiers redacted`; fixed private validation hosts and Target runs retain the full fingerprint.

## Evidence Schemas

Acceptance Evidence schema version 1 records:

- generation time, candidate ID, and source revision;
- executable and fixture configuration SHA-256 values;
- workload identity and version;
- compiled CPU baseline, required features, missing features, and guard result;
- OS, architecture, logical processors, machine name, OS version, processor, and physical memory when the platform exposes them;
- every measured duration in nanoseconds; and
- count, minimum, maximum, nearest-rank p50 and p95, arithmetic mean, and population standard deviation.

Benchmark Evidence schema version 2 keeps the same candidate, executable, fixture, CPU, and machine binding while adding named `timings_ns` and unit-neutral `gauges` to every raw sample. It derives timing summaries with nanosecond-labelled fields and separate unit-neutral gauge summaries, so storage bytes, write counts, or handle counts are never represented as time. A target may leave both maps empty when it has no meaningful sub-measurements, as `harness/sha256-loop` does.

Benchmark Evidence version 1 remains historical pipeline output and is not reinterpreted as version 2. The current runner only produces evidence; it has no ingestion or migration path. The schema registry explicitly rejects version 1 when asked for the current Benchmark Evidence version. A future reader must name every version it accepts and add compatibility or migration tests.

Schema zero is reserved. Readers accept only the schema versions they explicitly implement; schema changes never reinterpret existing evidence.

## CI and Release Boundary

Windows CI uses the repository's configured x86-64-v3 target policy, requires the release binary to self-report that baseline, runs its host-feature guard, and packages a redacted three-sample acceptance smoke plus a one-sample benchmark-pipeline smoke. Separate feature-built runners execute redacted smoke samples for each current storage, terminal, transport, and allocator candidate; their evidence is uploaded separately and the candidate binaries are not added to the portable product package. These checks prove compilation, executable wiring, and configuration agreement only. FMDev performance evidence uses the fixed host and at least 30 samples. A Target Acceptance ZIP additionally binds the release-candidate package, applies redaction, runs all required named workloads, and remains mandatory for Alpha.

## Technology Comparison Evidence

`greentyper-acceptance bench` uses a separate Benchmark Evidence schema. It keeps the release candidate, comparison, implementation, and workload as independent identities so the same versioned workload can compare multiple implementations without changing its meaning. Each file records the implementation revision, a fingerprint binding the declared dependency/features input and locked dependency graph, fixture hash, timing boundary, process mode, operation units, correctness digest, every raw operation duration, optional named sub-timings and gauges, and the common host and CPU evidence.

`bench list` reports only implementations compiled into the runner. The initial `harness/sha256-loop` entry proves parsing, timing, correctness checks, evidence serialization, and CI packaging. The optional `bench-storage` feature adds `storage/sqlite-wal` and `storage/append-log`; `bench-terminal` adds `terminal/direct-vt` and `terminal/ratatui-crossterm`; `bench-transport` adds `transport/reqwest-rustls` and, on Windows, `transport/winhttp-wrest`. The mutually isolated `bench-allocator-system`, `bench-allocator-mimalloc`, and `bench-allocator-snmalloc` builds each expose only their compiled allocator. None enters the default product graph. Harness and CI smoke results cannot select storage, terminal, transport, or allocator behavior.

Every benchmark run names a workload independently from its implementation:

```text
greentyper-acceptance bench \
  --comparison storage \
  --implementation sqlite-wal \
  --workload critical-append-replay \
  --candidate-id rc-0001 \
  --source-revision 0123456789abcdef0123456789abcdef01234567 \
  --output storage-sqlite-wal.json \
  --expect-baseline x86-64-v3
```

The storage feature currently exposes eight workload IDs. CI executes the complete feature test suite and emits one redacted `critical-append-replay`, `timer-expiry-streaming-replay`, `cas-one-winner`, `interrupted-migration`, and `cross-process-crash-replay` sample per candidate, plus one SQLite-only `sqlite-vfs-fault-recovery` sample. The CAS workload records `process_mode = cross-process`, eight synchronized child contenders, one durable winner, and seven stale-head losers. The migration workload records `process_mode = cross-process`, three killed child processes, two v1 recoveries, one v2 recovery, and one shared canonical digest. The timer workload records virtual arrival duration, 250ms deadline, flush causes, batch count, and maximum planned batch age without wall-clock sleeping. The crash workload records `process_mode = cross-process`, six killed child processes, and separate known-not-repeated and ambiguous-blocked counts. The VFS workload records three injected WAL fault cases, three integrity-checked complete-prefix recoveries, two known-not-repeated write failures, and one ambiguous-blocked sync failure. The other workload tests are correctness evidence, not substitutes for the same-host 30-run FMDev matrix.

The terminal feature exposes one `render-matrix` workload. CI emits one redacted sample for direct VT and Ratatui/Crossterm. The fixture contains 27 frames across three viewport sizes and requires identical ANSI-replayed cells, styles, wide-character continuations, cleared cells, and digest. Model-equality no-op frames are suppressed before backend draw and must emit zero bytes. The smoke does not cover real console input, process startup, stable idle, resource budgets, or the same-host 30-run decision matrix.

The transport feature exposes one `loopback-sse` workload. Windows CI checks the combined feature for cross-candidate correctness, then builds candidate-specific WinHTTP-only and Reqwest-only runners so executable hashes and dependency costs remain independent; each emits one redacted sample. macOS ARM runs the Reqwest fixture tests only. Every sample creates private loopback origin and proxy servers and verifies seven cold/warm, HTTP-error, timeout, cancellation, proxy, and custom-origin cases. The fixture splits UTF-8 code points and line endings across parser fragments, freezes the canonical event digest, and requires zero authorization bytes at the proxy. This smoke is HTTP-only and does not cover HTTPS/TLS policy, proxy authentication, system-proxy discovery, live providers, resources, or the same-host 30-run decision matrix.

The allocator features compile the same acceptance source into three separate process-global allocator runners. Their `allocation-pressure` workload executes 24 rounds of 192 bounded live blocks with deterministic size, growth, mutation, release, and result sampling. CI requires one shared correctness digest plus three distinct executable and dependency fingerprints. The Snmalloc fingerprint includes the Windows-only `/wd4864` control required by Snmalloc 0.7.4 under MSVC 19.51 while retaining upstream `/WX`. Its gauges describe requested payload and operation counts, not Private Bytes, Working Set, commit, or allocator-reserved memory. This workload proves candidate isolation and result equivalence only; cold start, stable idle, streaming, Agent load, context-pressure return-to-idle, Windows process-tree resources, CRT comparability, and the same-host 30-run decision matrix remain required.
