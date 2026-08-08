# Build portable Checkpoints at Safe Barriers

Context Checkpoints will combine a deterministic Runtime Fold, key Events, verified decisions and evidence, unfinished work, and a recent raw tail, and may commit only at a Safe Barrier against an exact Event range using compare-and-swap. Artifact offload and deterministic reduction precede optional semantic handoff, provider-native compaction remains an adapter, and periodic full rebases from the Event Ledger prevent recursive-summary drift.
