# Freeze Capability Snapshots for each Turn

GreenTyper will deterministically sort tools into a versioned Toolset Epoch and freeze each Agent's permitted subset as a Capability Snapshot before a Turn starts. High-frequency authorized tools may be exposed directly while long-tail tools remain behind a stable search, describe, and call gateway; catalog changes wait until the next Turn to preserve authorization clarity, model behavior, and prompt-cache stability.
