# Preserve stored state over external parity

GreenTyper may spend its Compatibility Budget by dropping legacy consoles, broad CPU support, or external CLI and configuration parity, but released Event Ledger, Thread, and Durable Memory schemas are durable product contracts. Schema changes require explicit forward migrations and recoverable backups, and older binaries must refuse unsupported newer state instead of silently downgrading it.
