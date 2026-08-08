# GreenTyper

GreenTyper is a Windows-first, Rust-based coding-agent runtime built to remain responsive on constrained Windows laptops while providing provider portability, cache-efficient long-running work, and first-class agent teamwork.

## Language

**GreenTyper**:
The product as a whole: a recoverable coding-agent runtime with its own identity and Codex-compatible boundaries.
_Avoid_: Rust Codex clone, Codex TUI rewrite, CLI wrapper

**Agent**:
An autonomous participant that owns a task, an isolated working context, and an explicitly granted capability set.
_Avoid_: Bot, worker process, model call

**Agent Team**:
A coordinated set of Agents that divide work, exchange evidence, and retain explicit ownership of tasks and workspace changes.
_Avoid_: Swarm, pool, parallel prompts

**Task**:
A bounded unit of work in the Team's dependency graph with one current Owner and an explicit terminal outcome.
_Avoid_: Prompt, Turn, todo text

**Task Owner**:
The single Agent accountable for advancing a Task and producing its result or blocker.
_Avoid_: Collaborator, Writer, Coordinator

**Workspace Lease**:
The exclusive right of one Agent to write a specific worktree for a bounded Task while other access remains read-only.
_Avoid_: File lock, Agent ownership, repository permission

**Read Set**:
The versioned collection of workspace evidence an Agent relied on and must revalidate before writing when concurrent changes occur.
_Avoid_: Open files, cache, changed files

**Completion Capsule**:
The structured result through which an Agent returns conclusions, evidence, changes, tests, decisions, blockers, artifacts, and residual risks.
_Avoid_: Summary, chat message, transcript

**Artifact**:
A durable referenced object holding large or non-contextual content outside normal Context Views while remaining linked to its originating Events.
_Avoid_: Attachment, cache entry, tool output

**Active Agent**:
An Agent currently consuming a share of GreenTyper's bounded execution capacity.
_Avoid_: Spawned process, registered Agent, Agent record

**Dormant Agent**:
A checkpointed Agent that retains its identity, task ownership, and resumable state without consuming active execution capacity.
_Avoid_: Terminated Agent, hidden process, idle thread

**Thread**:
A recoverable and forkable line of user-visible work containing ordered Turns.
_Avoid_: Chat, process, Provider session

**Turn**:
One admitted unit of intent carried from input through Agent and tool activity to a terminal outcome.
_Avoid_: Request, message, model response

**Item**:
A typed domain record inside a Turn, such as input, reasoning, output, tool activity, approval, or coordination state.
_Avoid_: Message, Provider event, log line

**Provider Profile**:
A configured inference-service instance containing a user-selectable endpoint, credential reference, and declared connection behavior, created from a Provider Template or an explicit custom definition.
_Avoid_: Model Preset, credential, API URL

**Provider Origin**:
The scheme, host, and port identifying the network authority to which one Provider Profile's credentials and pricing assumptions are explicitly bound.
_Avoid_: Base URL, route path, Provider Template

**Provider Template**:
A built-in, non-secret starting definition for a known upstream service, including its official endpoint defaults, authentication shape, discovery behavior, and permitted Provider Dialects.
_Avoid_: Provider Profile, credential bundle, model list

**Model Catalog**:
The provenance-aware set of Model Descriptors resolved from release-bundled seeds, lazy provider discovery, and explicit user overrides.
_Avoid_: Model Preset, Provider Profile, model selector

**Model Descriptor**:
A provider-specific model identity and its known capabilities, limits, supported inference settings, dialect routes, and price schedules, with source and freshness attached.
_Avoid_: Model Preset, model name, Provider Profile

**Model Preset**:
A named runnable choice combining one Provider Profile, one Model Descriptor, and user-selected inference defaults such as reasoning effort, service tier, output limit, context policy, and fallback order.
_Avoid_: Model Catalog, Provider Profile, permission set

**Provider Dialect**:
The wire-level request, event, tool-call, and continuation semantics selected for a Model Preset within a Provider Profile.
_Avoid_: Provider, transport, API format

**Transport**:
The connection mechanism used to carry a Provider Dialect, independent of that dialect's meaning.
_Avoid_: Provider, protocol

**Context Mode**:
The strategy a Provider Profile uses to continue work, such as provider-native continuation or canonical full replay.
_Avoid_: Transport, Provider Dialect, Compaction

**Provider Epoch**:
A span of Turns sharing one resolved Provider Profile, Model Preset, frozen capability contract, and continuation namespace.
_Avoid_: Provider session, Thread, connection

**Event Ledger**:
The authoritative chronological record of user input, Agent output, tool activity, approvals, workspace effects, and coordination events.
_Avoid_: Chat history, summary, Memory

**Durability Boundary**:
The point before an externally visible effect or acknowledgement at which its governing Event Ledger records must already be durable.
_Avoid_: Flush interval, checkpoint, eventual persistence

**Tool Ledger**:
The authoritative record of tool-call identity, arguments, approval, execution state, and outcome used to prevent duplicate effects across retries and recovery.
_Avoid_: Tool output, console log, Event Ledger replacement

**Context View**:
The bounded projection of relevant information shown to a model for one Turn; it is derived and replaceable, never the source of truth.
_Avoid_: Transcript, prompt, Memory

**Context Pressure**:
The Provider-specific projected occupancy of a model window after all context inputs and output reserve are included.
_Avoid_: Message tokens, transcript length, byte size

**Safe Barrier**:
An Event Ledger boundary with no in-flight operation whose state or effects cannot be replayed or reconciled deterministically.
_Avoid_: Turn boundary, timer, token threshold

**Runtime Fold**:
The deterministic projection of active Tasks, tool state, approvals, workspace facts, Skill state, and coordination state from authoritative records.
_Avoid_: Summary, Memory, Context View

**Context Checkpoint**:
A validated recovery representation of prior work at a safe Event Ledger boundary, sufficient to continue without pretending to be the original history.
_Avoid_: Summary, compressed chat, Memory

**Compaction**:
The process of reducing a Context View while preserving the Event Ledger, runtime state, user-visible history, and Durable Memory as separate records.
_Avoid_: Delete history, summarize everything

**Durable Memory**:
Evidence-linked knowledge retained across Turns or Threads and revalidated before use; it can inform judgment but cannot grant capabilities or bypass approval.
_Avoid_: Transcript, Context Checkpoint, hidden instruction

**Memory Candidate**:
A provisional typed observation awaiting user confirmation or evidence verification before it may become Durable Memory.
_Avoid_: Durable Memory, summary claim, retrieved context

**Memory Provenance**:
The source, evidence, time, scope, trust class, and revision lineage attached to a Memory record.
_Avoid_: Confidence score, citation text, audit log

**Skill**:
A versioned, reviewable workflow that describes how an Agent should perform a class of work and may include supporting resources.
_Avoid_: Tool, capability, prompt snippet

**Skill Invocation**:
A pinned execution of one Skill identity, version, and content hash with explicit phase, parameters, and state.
_Avoid_: Skill activation, script process, prompt inclusion

**Extension Surface**:
The supported boundary for adding GreenTyper behavior through Skills, MCP, or Provider Profiles without loading third-party code into the runtime.
_Avoid_: Plugin ABI, core hook, dynamic library

**Capability Snapshot**:
The immutable set of tools and external operations an Agent may access for one Turn after policy, approval, Skill needs, and connection state are resolved.
_Avoid_: MCP catalog, permissions, tool list

**Toolset Epoch**:
A deterministic revision of the directly exposed and gateway-reachable tool catalog from which a Capability Snapshot is created.
_Avoid_: Provider Epoch, MCP connection, tool cache

**Elicitation**:
An attributed request from an external capability for user input required before its suspended operation can continue.
_Avoid_: Approval, model question, notification

**Delegation**:
An explicit transfer of Task scope, resource budget, and a non-expanding subset of capabilities from one Agent to another.
_Avoid_: Capability inheritance, Agent spawn, shared credentials

**Target Machine**:
The constrained Windows laptop profile on which GreenTyper's release performance is judged.
_Avoid_: Developer Mac, hosted CI runner, fastest supported machine

**Target Load**:
The representative background pressure already present on the Target Machine before GreenTyper starts, including normal developer applications and security software.
_Avoid_: Clean boot, synthetic idle, GreenTyper-only usage

**Validation Host**:
A fixed Windows environment used for correctness and relative regression measurements but not assumed to represent the Target Machine's absolute performance.
_Avoid_: Target Machine, release proof, hosted CI runner

**Acceptance Run**:
An asynchronous execution of a versioned measurement bundle on the Target Machine that returns its environment fingerprint and Performance Contract evidence.
_Avoid_: Hosted CI job, informal usage, microbenchmark

**Performance Contract**:
A repeatable set of resource and latency limits that GreenTyper must meet on the Target Machine under named workloads.
_Avoid_: Aspirational benchmark, isolated microbenchmark, best-case result

**Compatibility Budget**:
The deliberately limited set of platforms and external behaviors GreenTyper preserves after correctness, safety, and the Performance Contract are protected.
_Avoid_: Universal compatibility, accidental breakage, feature parity

**Config Epoch**:
The immutable, fully resolved configuration visible to one Turn after built-in, user, project, and command-line layers are applied in order.
_Avoid_: Config file, live settings, Provider Epoch

**Approval Grant**:
A bounded user authorization tied to one Agent, operation identity, arguments, resource scope, and expiry.
_Avoid_: Permission, blanket trust, Capability Snapshot

**Sandbox Profile**:
The fail-closed limits on filesystem, network, and process authority applied to one tool execution independently of its Approval Grant.
_Avoid_: Approval, tool capability, child process

**Diagnostic Bundle**:
A locally generated, redacted package of runtime evidence that leaves the machine only through an explicit user action.
_Avoid_: Telemetry, crash upload, Event Ledger export

**Usage Record**:
An immutable, provider-attributed account of one inference attempt's model, dialect, inference settings, token classes, cache accounting, and cost evidence, preserving unsupported values as unknown.
_Avoid_: Status line value, invoice, aggregate counter

**Usage Rollup**:
A rebuildable time-windowed projection of Usage Records grouped by dimensions such as Provider Profile, Model Descriptor, reasoning effort, and service tier.
_Avoid_: Usage Record, telemetry, billing statement

**Usage Window**:
A named recurring half-open local-time interval with an explicit time zone and day set that selects Usage Records by inference start time without changing their original timestamps or cost evidence.
_Avoid_: Billing period, rolling window, data retention

**Cost Estimate**:
A currency amount derived from a Usage Record and a versioned Price Schedule, distinct from a provider-reported charge or subscription quota value.
_Avoid_: Invoice, account balance, Usage Rollup

**Price Schedule**:
A provenance-linked set of effective-dated rates for one Provider Profile and Model Descriptor across token classes, context bands, and service tiers.
_Avoid_: Current price, invoice, Model Catalog

**Cache Accounting**:
The normalized attribution of provider-reported input cache reads and writes while retaining uncached input, visible output, reasoning output, and unknown categories separately.
_Avoid_: Context Checkpoint, local cache size, fabricated cache ratio

**Config Object**:
A schema-defined unit of GreenTyper configuration with a stable identity, supported scopes, effective provenance, validation rules, and application timing.
_Avoid_: TOML section, form field, Config Epoch

**Config Schema**:
The authoritative, machine-readable description of all Config Objects shared by serialization, validation, interactive editors, and headless control surfaces.
_Avoid_: TOML schema, form definition, documentation page

**Config Draft**:
A staged set of changes targeting one configuration layer that has not become effective until it is validated and committed atomically.
_Avoid_: Unsaved form, Config Epoch, partial write

**Command Path**:
A hierarchical sequence of slash-command tokens that identifies an action while keeping nested actions out of the root Slash Panel.
_Avoid_: Flat command name, fuzzy-search query, config key
