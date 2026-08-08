# Protect secrets separately from the Ledger

Provider and MCP credentials will live in Windows Credential Manager or DPAPI-protected storage and will never be written into the Event Ledger or Context Checkpoints. The Ledger will rely on restrictive file ACLs, sensitive-field separation, and redaction rather than default full-database encryption, trading broad at-rest secrecy for lower runtime cost and simpler recovery while preserving a future explicit encrypted mode.
