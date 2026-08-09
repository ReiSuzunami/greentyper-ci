# Isolate technology candidates from the product graph

Phase 0 technology candidates run behind the acceptance package's benchmark interface, using identical versioned workloads and separately identified implementation binaries or feature sets. Candidate-only dependencies do not enter `greentyper-core` or the default product dependency graph before evidence selects them. CI benchmark smoke proves only compilation and evidence plumbing; a decision requires correctness gates and at least 30 same-host FMDev samples for every candidate under the named workload.
