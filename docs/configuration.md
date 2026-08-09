# Configuration and Command Surface

## Principles

1. Every supported Config Object is editable without opening a file.
2. One Config Schema drives parsing, defaults, validation, TUI editors, CLI operations, App Server operations, and documentation metadata.
3. Effective values always retain their source layer and application timing.
4. Secrets are referenced by configuration but stored and edited separately.
5. Runtime-affecting changes become visible at the next Config Epoch; presentation-only changes may render immediately.

## Layers and Locations

Configuration resolves in this order, with later layers winning:

1. Release-bundled defaults
2. User configuration: `%APPDATA%\GreenTyper\config.toml` on Windows,
   `~/Library/Application Support/GreenTyper/config.toml` on macOS, and
   `$XDG_CONFIG_HOME/greentyper/config.toml` (falling back to
   `~/.config/greentyper/config.toml`) on other Unix systems
3. Project configuration: `.greentyper/config.toml`
4. Command-line overrides

Project Skills live under `.greentyper/skills/`. User Skills live under the user configuration area. Runtime state, Ledgers, checkpoints, caches, logs, and usage projections live under `%LOCALAPPDATA%\GreenTyper` and are not configuration.

The TUI shows both the effective value and its source. A Config Draft targets one writable layer. A command-line override remains visible as the winning read-only value; editing a lower layer does not pretend to change the current process.

## Config Schema

Each Config Object declares at least:

- Stable identity and schema version
- Supported user, project, and command-line scopes
- Type, default, validation, and normalization rules
- Sensitivity and credential-reference behavior
- Whether a change is immediate, next-Turn, next-Provider-Epoch, or restart-bound
- Generic editor metadata or a purpose-built wizard
- Migration and deprecation metadata

The Config Runtime stages edits, presents a diff, validates the complete affected layer, writes atomically, and keeps a recoverable backup. Partial form submission never becomes an effective configuration.

Before implementation, the following names and types are the normative v1 design contract for the fields used in this document. Phase 1 materializes this contract as the versioned machine-readable Config Schema; generated reference documentation may extend it but may not change these meanings without an ADR.

Implementation status: the current Phase 1 Config Runtime parses and emits
schema-version-1 TOML for user and project layers, resolves effective values
with provenance, and exposes the addressable Provider Profile, Model Preset,
statusline, and Usage Window fields listed below. Typed drafts support dry-run,
revision compare-and-swap, atomic replacement, one recoverable backup, and a
last-valid repair state. The headless Runtime freezes the bootstrap projection
(`provider.profile`, `provider.model`, and `runtime.max_output_bytes`) for each
new Turn. The schema registry currently exposes identity, type, writable
scopes, application timing, credential-reference status, and editor identity;
default/constraint/normalization/migration metadata, TUI and App Server
surfaces, Provider Templates/catalogs, and the credential store remain Phase 1
or later work.

| Config Object | Type and constraint | Application timing |
| --- | --- | --- |
| `providers.<id>.template` | Provider Template ID | Next Provider Epoch |
| `providers.<id>.credential` | Lowercase secure-store reference ID; never credential material | Next Provider Epoch |
| `providers.<id>.base_url` | Absolute HTTP(S) URL with no user info, query, or fragment | Next Provider Epoch |
| `providers.<id>.routes.<name>` | Path-only suffix for `responses`, `chat_completions`, `messages`, or `models` | Next Provider Epoch |
| `providers.<id>.dialects` | Non-empty set of `responses`, `chat_completions`, and `messages` supported by that profile | Next Provider Epoch |
| `providers.<id>.catalog.mode` | `template`, `discovery`, `template_and_discovery`, or `manual` | Next Provider Epoch |
| `providers.<id>.pricing.source` | `unknown`, `template`, `manual`, or `provider_reported` | Next Provider Epoch |
| `providers.<id>.allow_insecure_loopback` | Boolean; false by default and invalid for a non-loopback host | Next Provider Epoch |
| `model_presets.<id>` | Provider, model, dialect, inference settings, context mode, and explicit fallback list | Next Turn and Provider Epoch when identity changes |
| `ui.statusline` | Preset, expansion policy, segments, and optional named Usage Window reference | Immediate presentation update |
| `stats.windows[]` | Unique ID, local start/end, day set, and resolvable time-zone ID | Next Config Epoch |

Unknown keys fail validation unless their owning schema version explicitly reserves them. A field rejected at one surface is rejected identically at every other surface.

## Interactive Configuration

The root Slash Panel exposes `/config` as one Command Path. It does not register one flat command for every field.

```text
/config
|- provider
|  |- add
|  |- edit <profile>
|  `- remove <profile>
|- model
|- statusline
|- stats-window
|- agent
|- skills
|- mcp
`- security
```

Prefix and fuzzy matching work token by token. `/con` finds `/config`; `/config pro url` finds the focused Provider Profile base-URL editor. Pressing Enter on `/config` opens the searchable Config Center. A global command palette can search all nested and recent actions without adding them to the root Slash Panel.

`/config stats-window` edits named Usage Windows. The separate root command `/stats` displays usage reports and never mutates configuration. Likewise, `/model` selects a runnable Model Preset while `/config model` edits preset definitions.

Slash commands navigate and select scope; mutation occurs in a validated
dialog. The current headless CLI provides `config schema`, `config get`,
`config set`, `config reset`, and backup-based `config repair`. `set` and
`reset` require an explicit `user` or `project` scope, are single-operation
Config Drafts, and accept `--dry-run` to return the normalized diff without
committing. `--user-config` and `--project-config` select explicit absolute
paths for tests and controlled automation; normal execution uses the platform
user path and `.greentyper/config.toml` in the current project.

The App Server exposes the same Config Runtime operations, independent of its eventual wire encoding:

| Operation | Required behavior |
| --- | --- |
| `config.schema` | List addressable objects, types, scopes, editor metadata, and application timing |
| `config.get` | Return requested and effective values with source layer; secret values are never addressable |
| `config.draft.begin` | Open a draft for one writable layer and return its base revision |
| `config.draft.set` / `config.draft.reset` | Stage a typed change without affecting the current Config Epoch |
| `config.draft.validate` | Return the normalized diff and field-addressed validation errors |
| `config.draft.commit` | Compare the base revision, write atomically, and return the new revision and application timing |
| `credential.bind` / `replace` / `test` / `forget` | Operate on secure-store references without returning credential material |

All surfaces share stable error categories: `unknown_object`, `wrong_type`, `invalid_value`, `read_only_scope`, `revision_conflict`, and `secret_read_forbidden`. A running process retains its last valid Config Epoch after an invalid external edit; startup with no valid epoch enters a configuration-repair surface instead of silently dropping a layer.

## Provider Profiles

Built-in Provider Templates supply official defaults for OpenAI, DeepSeek, and OpenCode Go. A normal official profile usually requires only a credential binding:

```toml
schema_version = 1

[providers.openai-main]
template = "openai"
credential = "openai-main"
```

A user-operated gateway overrides the base URL and, only when necessary, individual routes:

```toml
[providers.corp]
template = "openai"
base_url = "https://gateway.example.com/v1"
credential = "corp-gateway"
dialects = ["responses", "chat_completions"]
allow_insecure_loopback = false

[providers.corp.routes]
responses = "/responses"
chat_completions = "/chat/completions"
models = "/models"

[providers.corp.catalog]
mode = "manual"

[providers.corp.pricing]
source = "unknown"
```

The selected non-simulator `provider.profile` must name an effective Provider
Profile. At Turn admission, Config Runtime resolves that profile into an owned,
typed snapshot. Provider Runtime freezes the template identity, opaque
credential reference, normalized origin/routes, dialects, pricing source, and
loopback decision into the Provider Epoch; later Config edits affect only a new
Provider Epoch.

URL composition is intentionally not RFC relative-reference resolution. The runtime removes trailing slashes from `base_url`, normalizes each route to exactly one leading slash, and concatenates them: the example resolves `responses` to `https://gateway.example.com/v1/responses`. Routes may contain only a path; absolute URLs, authority components, dot segments, query strings, and fragments are rejected. A base URL may contain an origin path such as `/v1`, but no user info, query, or fragment.

Provider Template defaults are suggestions, not locked endpoints. Provider Origin changes have these effects:

- The official credential is not carried to the new origin; a credential must be selected explicitly.
- Official catalog pricing is not assumed. Pricing stays unknown until the user selects catalog inheritance, defines a Price Schedule, or the provider reports a trusted charge.
- HTTPS is required for remote origins. Plain HTTP is valid only for a loopback host and only when that profile sets `allow_insecure_loopback = true`.
- The profile remains a distinct statistics dimension. A gateway's hidden backend is unknown unless trusted response metadata identifies it.
- The next request starts a new Provider Epoch and cannot reuse incompatible continuation identity.

The provider wizard edits name, template, base URL, routes, credential binding, supported dialects, price source, and catalog behavior, then tests the connection before presenting the Config Draft.

## Model Catalog and Presets

The release ships a seed Model Catalog and marks every field with its source and freshness. Each catalog record has a stable model key, provider-template identity, catalog schema version, seed revision, and observation time. Every capability, limit, dialect, and price reference is represented as `{ value, source_kind, source_ref, observed_at }`; an explicit user field outranks discovery, which outranks the release seed. Pricing still resolves through a Price Schedule rather than becoming an unversioned catalog number. The initial catalog targets, as verified during design on 2026-08-09, are:

| Provider Template | Seed family | Primary dialects |
| --- | --- | --- |
| OpenAI | GPT-5.6 Sol, Terra, Luna | OpenAI Responses |
| DeepSeek | DeepSeek V4 Flash and Pro | Responses where supported; Chat Completions and Anthropic Messages as declared |
| OpenCode Go | Current Go catalog | Per-model Responses, Chat Completions, or Anthropic Messages |

Release work must refresh this snapshot from the [OpenAI model guide](https://developers.openai.com/api/docs/guides/latest-model), [DeepSeek API updates](https://api-docs.deepseek.com/updates), and [OpenCode Go catalog](https://opencode.ai/docs/go/). A release seed never claims to remain current indefinitely.

Provider discovery runs lazily when the model selector opens or the user requests refresh. It never runs as an idle background task. Discovered records augment seed fields with provenance; a model with no verified dialect remains visible but unavailable until an explicit override supplies one. Remote discovery cannot add credentials, arbitrary endpoints, instructions, or capabilities.

When an official Provider Profile is created, its wizard offers versioned starter Model Presets copied from the release snapshot and bound to that profile. The release set covers GPT-5.6 Sol, Terra, and Luna for OpenAI plus DeepSeek V4 Flash and Pro using only the dialects verified for each model at release time. OpenCode Go starter choices come from its heterogeneous per-model catalog. Accepting the starters writes ordinary user-owned presets; later catalog refreshes may offer an update but never rewrite them silently.

A Model Preset is a runnable choice rather than catalog metadata:

```toml
[model_presets.frontier]
provider = "openai-main"
model = "gpt-5.6-sol"
dialect = "responses"
reasoning_effort = "high"
service_tier = "default"
max_output_tokens = 32768
context_mode = "provider_native"
favorite = true
fallback = []
```

A runnable preset must name its Provider Profile, model, and Provider Dialect. Its route is then resolved from that profile; there is no hidden dialect auto-selection. Presets may also define reasoning effort, reasoning mode where supported, service tier, output limit, context policy, and an explicit fallback chain. They never grant tools, approvals, credentials, or workspace authority.

The model selector provides Favorites, Recent, Compatible, and All views with fuzzy search. Each entry shows provider, dialect, known context limit, capabilities, price freshness, and observed availability. Incompatible entries remain visible with a reason. Catalog refresh is manual or selector-triggered.

A selection applies to the current Agent on its next Turn by default. The user may instead set the default for new Agents or the project. Running child Agents are not silently changed. Any provider/model/dialect change starts a Provider Epoch. Automatic price- or latency-based routing is outside v1; fallback is explicit and must preserve the required capability contract.

## Statusline

The statusline is event-driven and adaptive. It has four presets:

- `minimal`: model, Context Pressure, blocker/approval
- `balanced`: current Agent/Task, Git, model/effort/tier, context, Thread cost, cache
- `diagnostic`: balanced plus provider/transport, Team state, tokens, latency, compaction, and process resources
- `custom`: ordered user-selected segments

Balanced is the default. The compact row prioritizes current work, context, cost, and cache. A detail row can be toggled and may appear automatically when terminal width permits. Segments carry priority, compact labels, and hide thresholds so narrow terminals degrade predictably.

```toml
[ui.statusline]
preset = "balanced"
expand = "auto"
primary_usage_window = "workday"

[ui.statusline.custom]
left = ["mode", "agent", "task", "git"]
right = ["model", "reasoning", "tier", "context", "thread_cost", "cache"]
```

Core segments include mode, Agent, Task, Git, model, reasoning effort, service tier, Context Pressure, Thread cost, cache read/write ratios, input/output/reasoning tokens, selected Usage Window, Team state, provider, transport, latency, Compaction, and process resources.

`primary_usage_window` is optional, but a supplied value must identify a window in the same resolved configuration. Removing a referenced window and clearing or replacing the reference must happen in one Config Draft. Validation never silently falls back to a different window; with no configured reference, the segment is hidden.

Normal segments consume cached runtime state and update on Events. Clock and process-resource segments are opt-in and use scheduled low-frequency updates. v1 does not execute arbitrary shell commands from the statusline.

Context display distinguishes projected next-request occupancy from last provider-reported input and marks estimates with `~`. It includes reserved output tokens and soft/hard Compaction thresholds. Cache display uses read and write ratios where reported; unsupported values display as unknown rather than zero.

## Usage and Cost Windows

Rolling 1-hour, 1-day, and 7-day views are always available. Named Usage Windows add calendar-based views:

```toml
[[stats.windows]]
id = "workday"
start = "10:00"
end = "21:00"
days = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"]
timezone = "Asia/Hong_Kong"
```

Usage Windows are half-open intervals: the example includes 10:00 and excludes 21:00. A window may cross midnight and belongs to its start date. An inference attempt belongs wholly to the window containing its start time because providers normally report only final aggregate usage. Multiple named windows are supported; v1 has no holiday-calendar integration.

`timezone` accepts an IANA time-zone ID or `local`. At Config Epoch creation, `local` resolves to a concrete IANA ID using the Windows time-zone mapping; failure to resolve invalidates the draft rather than falling back to UTC. The resolved ID and time-zone rule-set version are projection provenance. Membership converts the attempt's UTC start instant into that zone: both occurrences of a repeated clock hour are eligible, while a skipped clock hour contains no instants. Overlapping named windows are independent, so one Usage Record may appear once in each matching window but never twice in one window.

The statusline may show a compact selected window such as `work 87.2K/$1.43`. `/stats` presents Turn, Thread, Agent, Team, rolling, and named-window views with model, Provider Profile, reasoning effort, service tier, token class, and cache distributions.

A Cost Estimate records its Price Schedule version and currency. Historical values are not recomputed after a price change. Provider-reported charge, estimated pay-as-you-go cost, and subscription quota value remain separate; OpenCode Go quota value is not labeled as cash paid.

## Credentials

Credential values live in Windows Credential Manager or DPAPI-protected storage, never TOML, the Event Ledger, checkpoints, diagnostics, command history, or exported configuration. The Config Center can bind, replace, test, and forget a credential but cannot reveal its existing value.

Credential scope includes Provider Profile or MCP identity and Provider Origin. Delegation does not propagate credentials implicitly.
