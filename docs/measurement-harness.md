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

## Evidence Schema

Schema version 1 records:

- generation time, candidate ID, and source revision;
- executable and fixture configuration SHA-256 values;
- workload identity and version;
- compiled CPU baseline, required features, missing features, and guard result;
- OS, architecture, logical processors, machine name, OS version, processor, and physical memory when the platform exposes them;
- every measured duration in nanoseconds; and
- count, minimum, maximum, nearest-rank p50 and p95, arithmetic mean, and population standard deviation.

Schema zero is reserved. Readers accept only the schema versions they explicitly implement. Schema changes add a new version and migration or compatibility tests; they do not reinterpret existing evidence.

## CI and Release Boundary

Windows CI uses the repository's configured x86-64-v3 target policy, requires the release binary to self-report that baseline, runs its host-feature guard, and packages a redacted three-sample acceptance smoke plus a one-sample benchmark-pipeline smoke. That proves executable wiring and configuration agreement only. FMDev performance evidence uses the fixed host and at least 30 samples. A Target Acceptance ZIP additionally binds the release-candidate package, applies redaction, runs all required named workloads, and remains mandatory for Alpha.

## Technology Comparison Evidence

`greentyper-acceptance bench` uses a separate Benchmark Evidence schema. It keeps the release candidate, comparison, implementation, and workload as independent identities so the same versioned workload can compare multiple implementations without changing its meaning. Each file records the implementation revision, declared feature set, locked dependency-graph fingerprint, fixture hash, timing boundary, process mode, operation units, correctness digest, every raw duration, and the common host and CPU evidence.

`bench list` reports only implementations compiled into the runner. The initial `harness/sha256-loop` entry proves parsing, timing, correctness checks, evidence serialization, and CI packaging. It is not a technology candidate and cannot select storage, terminal, transport, or allocator behavior.
