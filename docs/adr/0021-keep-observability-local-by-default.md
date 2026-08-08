# Keep observability local by default

GreenTyper will emit structured diagnostics locally with zero default remote telemetry and will never automatically upload prompts, tool data, workspace paths, or secrets. Crash investigation uses a locally generated, redacted Diagnostic Bundle that the user must explicitly choose to share, accepting less fleet-wide visibility to protect sensitive coding work and avoid background resource cost.
