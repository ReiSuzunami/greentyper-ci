# Make tool effects idempotent across retries

Every tool invocation will be identified in the Tool Ledger by call identity and an arguments hash before it can cross its Durability Boundary. A successful effect is never repeated automatically after reconnect or replay, and an ambiguous outcome fails closed for reconciliation or renewed approval instead of guessing that a retry is safe.
