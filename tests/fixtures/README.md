# Test Fixtures

Fixtures are synthetic, redacted, and versioned. A fixture is immutable once a
released schema references it; upstream or workload changes add a new fixture
version and a reviewed explanation instead of rewriting prior evidence.

The Phase 0 acceptance, benchmark-pipeline, and first storage comparison
fixtures are compiled into the acceptance runner. Provider, persistent Ledger,
cross-process crash, terminal, configuration, and Memory fixtures land with the
first vertical slice that consumes them.
