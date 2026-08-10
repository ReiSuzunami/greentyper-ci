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
Price Schedule, statusline, and Usage Window fields listed below. Typed drafts support dry-run,
revision compare-and-swap, atomic replacement, one recoverable backup, and a
last-valid repair state. The headless Runtime freezes the bootstrap projection
(`provider.profile`, `provider.model`, and `runtime.max_output_bytes`) for each
new Turn and freezes resolved Usage Windows with concrete IANA identity and the
bundled time-zone rule-set version plus the resolved Price Schedule book. The schema registry currently exposes
identity, type, writable scopes, application timing, credential-reference
status, and editor identity. A terminal-neutral Config editor session resolves
a Command Path plus selected object into one revision-bound Config Draft,
exposes the focused field without credential read-back, previews the normalized
diff through full validation and locking, and commits through the existing
atomic compare-and-swap path. The same session now creates Provider Profiles,
Model Presets, Price Schedules, and Usage Windows through schema-owned multi-field drafts, and
deletes whole target-layer objects only after full reference validation. Typed
`add` and `remove` Command Paths stay nested beneath their Config sections;
failed validation or revision compare-and-swap leaves the draft live.
Provider Profile create/edit routes now add a terminal-neutral purpose-built
wizard over that same Draft. The wizard can derive the candidate's normalized,
typed Profile snapshot without writing Config and can explicitly run one bounded
connection and model-list observation against its configured `models` route.
Any staged change
invalidates the prior check result.
The release-bundled Provider Catalog now supplies schema-versioned OpenAI,
DeepSeek, and OpenCode Go template defaults plus seed model facts with
field-level provenance. Effective Profiles inherit those defaults unless the
user overrides a field. A custom origin under a template with a bundled,
versioned release rate card defaults to `template_mirror`; other custom origins
still require an explicit pricing decision.
Default/constraint/normalization/migration metadata in Config Schema, remaining
rendered TUI and App Server surfaces, live discovery, and starter-preset workflows
remain later work. The product now provides origin-bound credential bind, replace,
test, and forget operations backed by Windows Credential Manager; non-Windows
access fails closed until another platform backend is implemented.

| Config Object | Type and constraint | Application timing |
| --- | --- | --- |
| `providers.<id>.template` | Provider Template ID | Next Provider Epoch |
| `providers.<id>.credential` | Lowercase secure-store reference ID; never credential material | Next Provider Epoch |
| `providers.<id>.base_url` | Absolute HTTP(S) URL with no user info, query, or fragment | Next Provider Epoch |
| `providers.<id>.routes.<name>` | Path-only suffix for `responses`, `chat_completions`, `messages`, or `models` | Next Provider Epoch |
| `providers.<id>.dialects` | Non-empty set of `responses`, `chat_completions`, and `messages` supported by that profile | Next Provider Epoch |
| `providers.<id>.catalog.mode` | `template`, `discovery`, `template_and_discovery`, or `manual` | Next Provider Epoch |
| `providers.<id>.pricing.source` | `unknown`, `template`, `template_mirror`, `manual`, or `provider_reported` | Next Provider Epoch |
| `providers.<id>.allow_insecure_loopback` | Boolean; false by default and invalid for a non-loopback host | Next Provider Epoch |
| `model_presets.<id>` | Provider, model, dialect, inference settings, context mode, and explicit fallback list | Next Turn and Provider Epoch when identity changes |
| `price_schedules.<id>` | Version, currency, Provider Profile, model, optional dialect/service tier/context band, half-open UTC effective interval, provenance, and non-negative integer token-class rates | Next Config Epoch |
| `ui.statusline` | Preset, expansion policy, segments, and optional named Usage Window reference | Immediate presentation update |
| `stats.windows[]` | Unique ID, local start/end, day set, and resolvable time-zone ID | Next Config Epoch |

Unknown keys fail validation unless their owning schema version explicitly reserves them. A field rejected at one surface is rejected identically at every other surface.

## Interactive Configuration

This section defines the target interaction contract. The current product has
a terminal-neutral hierarchical Command Path registry generated from Config
Schema metadata, provenance-aware field views, typed nested object lifecycle
actions, and a reusable Config Runtime editor session for focused multi-field
draft validation, atomic create/edit/delete, and commit. The Direct VT product
TUI now connects one non-secret route, `/config statusline preset`, to that real
session in the user scope. Up/Down stages a bounded enum choice, Enter previews,
`c` commits only the currently previewed choice, and `d` explicitly discards.
Validation and revision-conflict failures stay visible without consuming the
Draft; Escape and quit keys cannot discard a dirty Draft, and a no-change commit
does not create a Config file. The committed value is verified by reopening the
Config Runtime. The running TUI still uses its frozen startup snapshot, so live
statusline reconstruction is not claimed by this slice. Object deletion is
target-layer explicit and fails when the resulting effective configuration has
dangling references. The snapshot-based `tui` tracer renders controller screens
reachable from the Slash Panel, including the top-level Config Center; only the
statusline-preset dialog above mutates a Draft. It does not yet ship remaining
schema-driven terminal editor dialogs,
object-name input form, rendered Provider
template picker, or App Server surface described below. The current
terminal-neutral Provider Profile wizard resolves release template defaults into
user-configured Profile Drafts and supports the explicit bounded connection and
model-list observation described below; it does not merge discovered records,
change selector availability, or install starter presets.

The root Slash Panel exposes `/config` as one Command Path. It does not register one flat command for every field.

```text
/config
|- provider
|  |- add
|  |- edit <profile>
|  `- remove <profile>
|- model
|- pricing
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
`config set`, `config reset`, backup-based `config repair`, and
`config test-provider` for the currently selected committed Profile. `set` and
`reset` require an explicit `user` or `project` scope, are single-operation
Config Drafts, and accept `--dry-run` to return the normalized diff without
committing. `--user-config` and `--project-config` select explicit absolute
paths for tests and controlled automation; normal execution uses the platform
user path and `.greentyper/config.toml` in the current project.

The current headless `stats` command reads the immutable Runtime usage
projection and accepts an optional Unix-millisecond `--at` instant for
deterministic rolling-window queries. TUI `/stats` and interactive
`/config stats-window` presentation remain Phase 3 work.

The product CLI also exposes `credential bind`, `replace`, `test`, and `forget`
for one lowercase secure-store reference, Provider Profile, and Provider Origin.
Secret values never appear in command arguments or output: an interactive bind
or replace uses a no-echo prompt, while controlled automation may provide one
bounded value on standard input. Existing values are never returned.
Generic effective-value reads also reject credential-reference fields; editor
views expose only whether the target and effective layers are bound.

The planned App Server exposes the same Config Runtime operations, independent of its eventual wire encoding:

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

Provider Catalog schema 1 currently supplies versioned official defaults for
OpenAI, DeepSeek, and OpenCode Go. Config Runtime resolves those defaults into a
user-defined Profile Draft and freezes the resulting origin, routes, dialects,
and pricing source in the Provider Profile snapshot. Catalog mode controls the
Config projection but is not Provider wire identity. Explicit Profile fields
win over template fields. A normal official profile therefore
usually requires only a credential binding:

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
- A bundled, versioned release rate card is mirrored by default when one exists for the selected template. The Profile and generated schedules record `template_mirror`, and an explicit `unknown`, `manual`, or `provider_reported` decision overrides it. A template with no bundled rate card still requires an explicit pricing decision.
- HTTPS is required for remote origins. Plain HTTP is valid only for a loopback host and only when that profile sets `allow_insecure_loopback = true`.
- The profile remains a distinct statistics dimension. A gateway's hidden backend is unknown unless trusted response metadata identifies it.
- The next request starts a new Provider Epoch and cannot reuse incompatible continuation identity.

The current terminal-neutral Provider Profile wizard edits one schema-driven
Draft across template, base URL, routes, opaque credential reference, supported
dialects, price source, catalog behavior, and loopback policy. It derives the
same immutable normalized Profile snapshot used by Provider Epoch admission and
can explicitly test the candidate before commit. The test performs exactly one
bounded GET to the configured `models` route, disables proxy discovery and
redirects, and uses the origin-bound credential. A 2xx response must be JSON and
fit within 256 KiB; the result accepts at most 1,024 unique, whitespace-free
model IDs of at most 256 bytes each. IDs are sorted for deterministic output.
An ID matching the frozen Profile template and release catalog receives only
that release key; an unknown ID remains unverified and carries no dialect,
capability, endpoint, pricing, or execution authority. Provider-supplied fields
other than `id` are ignored and never enter the result. The check returns a
fixed failure category for an invalid response, never exposes response bodies,
does not commit Config or mutate a Provider Epoch, and is not yet a commit gate.
The rendered wizard, secure credential binding dialog, template picker, and
starter preset workflow remain pending.

## Model Catalog and Presets

The current release-bundled Model Catalog is schema version 1, seed revision
`2026-08-10.2`, observed at `2026-08-10T00:00:00Z`. Every record has a stable
model key, provider-template identity, catalog schema version, seed revision,
and observation time. Model identity, display name, primary and supported
dialects, context limit, capabilities, price reference, and availability are
represented as `{ value, provenance }`, where provenance carries source kind,
source reference, and observation time. Unknown context, capability, and
live-availability facts remain explicit rather than being inferred. DeepSeek
seed records pin a versioned official price reference; other seed prices remain
unknown.

Config Runtime binds release records only to effective Profiles whose catalog
mode includes template data. The terminal-neutral selector searches configured
presets and those bound release candidates. It derives compatibility only from
the frozen Profile's declared support for the record's primary dialect and the
product adapters currently installed for that template/dialect pair; live
availability and Recent remain unknown. User/discovery precedence, lazy refresh,
and starter-preset acceptance remain target behavior. Pricing still resolves
through a Price Schedule rather than becoming an unversioned catalog number.
The installed execution matrix is deliberately closed: adapters accept
`openai` and explicit `openai-compatible` Profiles for Responses and Chat
Completions, and official `deepseek` Profiles for Responses, Chat Completions,
and Messages. `opencode-go` Profiles admit Chat Completions when the selected
model has an exact release-catalog Chat record and Responses only for the exact
GPT-5.6 Luna Responses record.
A Provider Template may still declare additional routes and dialect support as
Config/catalog facts; OpenCode Go Messages plus OpenAI Messages
remain unavailable until an exact adapter is installed. A catalog route or
declared dialect alone never proves wire compatibility. Release-candidate
compatibility remains tied to each record's primary dialect. DeepSeek V4 Pro is
therefore compatible through its primary Chat dialect, while V4 Flash is
compatible through its primary Responses dialect.
The current release seed contains:

| Provider Template | Seed family | Primary dialects |
| --- | --- | --- |
| OpenAI | GPT-5.6 Sol, Terra, Luna | OpenAI Responses |
| DeepSeek | DeepSeek V4 Flash and Pro | Responses where supported; Chat Completions and Anthropic Messages as declared |
| OpenCode Go | Go catalog observed for seed `2026-08-10.2` | Per-model Responses, Chat Completions, or Anthropic Messages |

Release work must refresh this snapshot from the [OpenAI model guide](https://developers.openai.com/api/docs/guides/latest-model), [DeepSeek API updates](https://api-docs.deepseek.com/updates), and [OpenCode Go catalog](https://opencode.ai/docs/go/). A release seed never claims to remain current indefinitely. `greentyper config catalog` emits the read-only bundled snapshot without reading Config, credentials, or the network.

Target Provider discovery runs lazily when the model selector opens or the user requests refresh. It never runs as an idle background task. Discovered records augment seed fields with provenance; a model with no verified dialect remains visible but unavailable until an explicit override supplies one. Remote discovery cannot add credentials, arbitrary endpoints, instructions, or capabilities. The current `test-provider` observation is ephemeral and is not merged into the catalog or selector; persistent discovery, freshness, and precedence are not implemented yet.

Target rendered Provider Profile creation offers versioned starter Model Presets copied from the release snapshot and bound to that profile. The release set covers GPT-5.6 Sol, Terra, and Luna for OpenAI plus DeepSeek V4 Flash and Pro using only the dialects verified for each model at release time. OpenCode Go starter choices come from its heterogeneous per-model catalog. Accepting starters will write ordinary user-owned presets; later catalog refreshes may offer an update but never rewrite them silently. This starter workflow is not implemented yet.

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

A runnable preset must name its Provider Profile, model, and preferred Provider
Dialect. Its route is resolved from that profile. Official DeepSeek Responses
preference keeps V4 Flash on Responses and resolves V4 Pro to Chat Completions
before admission. OpenCode Go admission separately requires the exact model and
dialect pair from the release catalog; it does not infer another dialect. The
effective dialect is frozen in the Provider Epoch. This is not transport retry
or general fallback-chain execution. Presets may also define reasoning effort,
reasoning mode where supported, service tier, output limit, context policy, and
an explicit fallback chain. They never grant tools, approvals, credentials, or
workspace authority.

The current headless execution surface accepts `--preset ID`. It resolves the
exact configured ID, applies its Profile/model/dialect, and freezes optional
`max_output_tokens`, typed reasoning effort, and typed service tier in the next
Turn's Config Epoch. `--preset` and `--dialect` are mutually exclusive, and an
unknown ID fails rather than triggering model-name inference. Responses maps
reasoning to `reasoning.effort`; OpenAI Chat Completions uses
`reasoning_effort`; both OpenAI adapters send `service_tier`. DeepSeek Responses
maps `max_output_tokens` and supported `reasoning.effort`, but rejects service
tier. DeepSeek Chat and Messages map only the output limit and fail before
network I/O when either unsupported policy field is selected. OpenCode Go Chat
maps the output limit to `max_completion_tokens` and likewise rejects both
unsupported policy fields. OpenCode Go Responses maps the output limit to
`max_output_tokens` and also rejects both fields until their gateway semantics
are canonicalized. Initial requests and one
in-process Tool continuation retain the same frozen values; restart replay
reconstructs them without making Tool continuation resumable. Requested Usage
metadata records the same policy. The accepted reasoning values are `none`,
`minimal`, `low`, `medium`, `high`, `xhigh`, and `max`; service tiers are
`auto`, `default`,
`flex`, `scale`, `priority`, and `fast`. Provider/model support is narrower and
model-dependent, so a valid configured value may still receive an explicit
Provider rejection. The client rejects zero or more than 1,048,576 requested
tokens as a cost and latency guard; a Provider or model may enforce a lower
limit. Context mode, fallback execution, current-Agent interactive selection,
and project/default selection remain target behavior.

The target model selector provides Favorites, Recent, Compatible, and All views
with fuzzy search. Each entry shows provider, dialect, known context limit,
capabilities, price freshness, and observed availability. Incompatible entries
remain visible with a reason. Catalog refresh is manual or selector-triggered.

A selection applies to the current Agent on its next Turn by default. The user may instead set the default for new Agents or the project. Running child Agents are not silently changed. Any provider/model/dialect change starts a Provider Epoch. Automatic price- or latency-based routing is outside v1; fallback is explicit and must preserve the required capability contract.

## Statusline

The target statusline is event-driven and adaptive. The current terminal-neutral
projection exposes recovery, provider/model, usage, active-agent count, blocker,
Config, and exact/estimated/unknown Context Pressure facts supplied by the core
projector. Estimated occupancy renders with `~`; a missing limit, used-token
fact, reserve, or accuracy marker stays unknown. Its compact row now applies
a deterministic priority and Unicode-safe truncation at 40, 80, and 160 columns,
and adds a detail row from 120 columns. This is a terminal-neutral layout
contract. The first product `tui` tracer now renders it through a Direct VT diff,
reacts to blocking key and resize events, and can persist the user-scope preset
through the narrow dialog described above. It does not rebuild the frozen
underlying snapshot after that commit, provide other mutable actions, or
establish ConPTY/resource evidence. The target has four
presets:

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

Normal segments consume cached runtime state and update on Events. Clock and
process-resource segments are opt-in and may use scheduled low-frequency updates
only under a measured Performance Contract exception. v1 does not execute
arbitrary shell commands from the statusline.

The implemented terminal-neutral Context snapshot carries projected occupancy,
used tokens, reserved output tokens, exact/estimated provenance, and the 65% / 90%
soft/hard policy. The compact segment renders only occupancy. Automatic Context
View construction and the target comparison with last provider-reported input
remain pending. Cache display uses read and write ratios where reported;
unsupported values display as unknown rather than zero.

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

The current Runtime implements durable Usage Attempts, cached rollups, the
headless JSON projection, schema-owned Price Schedule objects, and immutable
pay-as-you-go Cost Estimates. A schedule selects one Provider Profile and model,
optionally narrows dialect, service tier, and input-context band, and applies over
a half-open UTC effective interval. Its five rates are non-negative integer
currency-microunits per million tokens for uncached input, cached input, cache
write, visible output, and reasoning output:

The bare `stats` command keeps the original complete JSON snapshot. Callers with
large histories may explicitly request aggregate total, current Thread, Team,
rolling, and named-window rollups with `--summary-only`, or walk bounded attempt
pages with `--limit 1..1000` and the returned checksummed
`next_cursor`. Reports carry the Ledger revision and requested instant; stale,
malformed, or cross-instant cursors fail rather than combining snapshots. This
currently bounds report cloning and output size, while Ledger replay remains
bounded by the existing replay limits.

```toml
[price_schedules.openai-sol]
version = "2026-08-10.1"
currency = "USD"
provider = "openai-main"
model = "gpt-5.6-sol"
dialect = "responses"
minimum_context_tokens = 0
effective_from = "2026-08-10T00:00:00Z"
source = "manual"
source_ref = "rate-card-2026-08"

[price_schedules.openai-sol.rates]
input_micros_per_million = 1000000
cached_input_micros_per_million = 500000
cache_write_micros_per_million = 0
output_micros_per_million = 2000000
reasoning_output_micros_per_million = 3000000
```

The resolved schedule book rejects duplicate or overlapping selectors. Config
Epoch creation freezes the book and its fingerprints. Runtime Event schema 8
appends normalized Usage first and its cost evaluation second in one transaction;
replay recomputes the result from that frozen evidence. Missing token classes,
missing selectors, inconsistent accounting, and arithmetic overflow remain
explicit unknown reasons rather than becoming zero or wrapping. Exact and
estimated token evidence aggregate separately, and monetary totals use fixed
12-decimal pico-currency units without floating point.

Editable TOML schedules currently require `source = "manual"` and a Provider
Profile whose pricing decision is also `manual`. A user-editable object cannot
claim trusted template, template-mirror, or provider-reported provenance. The
release bundle currently provides DeepSeek V4 Flash and Pro schedules from the
official rate card. Official Profiles use `template`; custom DeepSeek origins
without an explicit pricing override use `template_mirror`. Both freeze the
rate-card version and source reference, but a mirror grants no credential,
endpoint, or backend identity. Provider-reported charges remain a future
dedicated ingestion path.

Each Cost Estimate records the complete immutable schedule, including version,
currency, provenance, rates, and fingerprint, so historical values are not
recomputed after a price change. Provider-reported charge, estimated
pay-as-you-go cost, and subscription quota value remain separate; only the
pay-as-you-go estimate is implemented today. OpenCode Go quota value is never
labeled as cash paid. Rich terminal-backed cost presentation remains pending.

## Credentials

Credential values live in Windows Credential Manager or DPAPI-protected storage, never TOML, the Event Ledger, checkpoints, diagnostics, command history, or exported configuration. The current product backend uses the logged-in user's Windows Credential Manager set with local-machine persistence across that user's logon sessions. Product CLI operations can bind, replace, test, and forget a credential but cannot reveal its existing value. Other platforms currently fail closed.

Credential scope includes Provider Profile or MCP identity and Provider Origin. Delegation does not propagate credentials implicitly.
