# Technology Benchmarks

## Status

The versioned benchmark runner is implemented. It distinguishes release candidate, comparison, implementation, workload, fixture, process mode, and operation boundary; retains every raw sample and correctness digest; and reuses the acceptance CPU guard, host fingerprint, redaction, hashes, and no-clobber evidence commit.

The default runner compiles only `harness/sha256-loop`, which validates the pipeline. The optional `bench-storage` feature compiles SQLite WAL and checksummed append-log candidates against seven explicit workloads: `critical-append-replay`, `bounded-streaming-replay`, `timer-expiry-streaming-replay`, `cas-one-winner`, `backup-restore`, `interrupted-migration`, and `cross-process-crash-replay`. The runner requires `--workload`; evidence therefore cannot silently change boundaries while retaining only a comparison and implementation name.

The implemented matrix proves identical canonical replay, WAL `synchronous=FULL`, synchronous append-log commits, max-event bounded streaming batches, deterministic virtual-clock flushes at the 250ms deadline, one-winner stale-head CAS policy, verified backup/restore, transactional or generation-based old-or-new migration recovery, exhaustive two-transaction tail-cut classification, checksum rejection, bounded framing, and per-run timing and size evidence. The timer workload never sleeps: fixed arrivals produce batch sizes `[3, 4, 1, 3]` and record deadline, event-limit, and final-drain flushes for both candidates. The cross-process workload launches and forcibly terminates the feature-built candidate child before write, at four transaction progress points, and after sync before acknowledgement. Restart must safely write once, remain ambiguous-blocked, or recognize the durable transaction without repeating it; both candidates emit the same correctness digest. SQLite progress points exercise transaction rollback and never claim synthetic torn-page evidence.

This is not a storage decision. Cross-process CAS and migration interruption, SQLite VFS fault/corruption recovery, additional approval/tool-effect durability boundaries, fuzzing, same-commit 30-run FMDev evidence for every workload, and Windows directory-entry durability evidence remain required. Transport, terminal, and allocator candidates are not implemented.

## Decision Rule

Every comparison uses the same source revision, locked dependencies, x86-64-v3 build policy, fixture version, FMDev shape, warmup policy, and at least 30 measured runs per implementation. Correctness, recovery, security, and packaging are pass/fail gates before timing is considered. Raw p50, p95, minimum, maximum, standard deviation, resource measurements, artifact size, and dependency cost remain visible.

An absolute Performance Contract violation blocks a candidate. A repeatable regression above 5% warns and above 10% blocks. When measured results are within noise, prefer the smaller dependency and maintenance surface. A microbenchmark or CI smoke cannot approve a choice.

## Comparison Matrix

| Comparison | Primary candidates | Representative boundary | Required correctness evidence | Status |
| --- | --- | --- | --- | --- |
| Transport | Windows WinHTTP; [Reqwest 0.13.4](https://docs.rs/reqwest/0.13.4/reqwest/) with minimal Rustls, HTTP/2, and streaming features | cold and warm loopback SSE streams, fixed payloads, cancellation, timeout, proxy, and custom origin | exact byte/event order, split UTF-8 and lines, HTTP errors, TLS policy, cancellation, proxy route, no credential leakage | Pending |
| Terminal | direct VT cell-grid diff; [Ratatui 0.30.2](https://docs.rs/ratatui/0.30.2/ratatui/) with [Crossterm 0.29.0](https://docs.rs/crossterm/0.29.0/crossterm/) | fixed 40x12, 80x24, and 160x50 frames; no-op, status update, streaming update, resize, and Slash Panel | identical final cells, ANSI replay, Unicode width, zero writes for a no-op frame, no periodic redraw | Pending |
| Storage | SQLite WAL through [Rusqlite 0.40.2](https://docs.rs/rusqlite/0.40.2/rusqlite/) bundled/backup; checksummed custom append log | synchronous critical transactions, bounded streaming batches, replay, corrupt tail, CAS, backup, migration, and child-process termination | complete-prefix replay, durable receipt, tamper failure, one CAS winner, recoverable backup and interrupted migration, known-not-repeated or ambiguous-blocked restart | Seven workloads including timer expiry and critical cross-process restart implemented; remaining crash/fuzz/FMDev gates and selection pending |
| Allocator | Windows system allocator; [Mimalloc 0.1.52](https://docs.rs/mimalloc/0.1.52/mimalloc/); [Snmalloc 0.7.4](https://docs.rs/snmalloc-rs/0.7.4/snmalloc_rs/) only if build/CRT controls remain comparable | separate binaries for cold start, stable idle, streaming, Agent load, and context-pressure return-to-idle | identical workload results, fixed CRT/toolchain/features, process-tree attribution, no hidden resident worker | Pending |

Versions were observed from the public crate index on 2026-08-09 and become fixed only when their candidate feature lands in `Cargo.lock`. Jemalloc is excluded from the primary Windows MSVC comparison because its maintained Rust wrappers do not present it as a supported MSVC target.

## Workload Isolation

In-process operations report only their declared operation boundary; fixture parsing, executable hashing, host fingerprinting, serialization, and evidence I/O remain outside the timed region. Allocator and other process-global candidates run as separate binaries. Process launch is reported independently unless the named workload is explicitly cold start.

Candidate adapters remain inside the acceptance package or package-local benchmark targets until selected. Provider wire types, terminal backends, storage engines, and global allocators do not leak into canonical core interfaces during comparison.
