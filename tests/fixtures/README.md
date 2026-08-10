# Test Fixtures

Fixtures are synthetic, redacted, and versioned. A fixture is immutable once a
released schema references it; upstream or workload changes add a new fixture
version and a reviewed explanation instead of rewriting prior evidence.

The Phase 0 acceptance, benchmark-pipeline, storage comparison, cross-process
storage-crash, terminal render-matrix, loopback SSE transport, and allocator
pressure fixtures are compiled into candidate-specific acceptance runners. The
Phase 1 deterministic Provider success fixture is compiled into the core
simulator test. Versioned Responses and Chat Completions fixtures now exercise
their bounded dialect decoders, canonical normalization, HTTP request shapes,
one Tool continuation, and fixed failure handling. Persistent Ledger corruption
is generated at exact frame boundaries by core tests; broader Config, Provider,
and Memory fixtures land with the first vertical slice that consumes them.
