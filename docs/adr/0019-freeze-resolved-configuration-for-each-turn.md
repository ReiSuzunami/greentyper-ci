# Freeze resolved configuration for each Turn

GreenTyper will resolve TOML configuration in built-in, user, project, then command-line order and freeze the result as a Config Epoch for each Turn. User configuration lives under `%APPDATA%\\GreenTyper`, runtime data under `%LOCALAPPDATA%\\GreenTyper`, and project configuration and Skills under `.greentyper`; runtime-affecting changes wait for the next applicable Config or Provider Epoch while presentation-only changes may apply immediately, preserving deterministic recovery, cache identity, and approval reasoning.
