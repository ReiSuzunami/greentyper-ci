# Test Fixtures

Fixtures are synthetic, redacted, and versioned. A fixture is immutable once a
released schema references it; upstream or workload changes add a new fixture
version and a reviewed explanation instead of rewriting prior evidence.

The Phase 0 acceptance, benchmark-pipeline, storage comparison, cross-process
storage-crash, terminal render-matrix, loopback SSE transport, and allocator
pressure fixtures are compiled into candidate-specific acceptance runners.
Provider, persistent Ledger, configuration, and Memory fixtures land with the
first vertical slice that consumes them.
