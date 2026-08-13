# Agent Orchestration

## Decision

Agent orchestration lives in one deep `Agent Team Runtime` module inside `greentyper-core`. Callers submit intent through `TeamRuntime::dispatch`; they do not activate Agents, resolve dependencies, reserve child budgets, order events, or wake Dormant Agents themselves.

The policy interface remains synchronous and process-local. It proves canonical policy and deterministic recovery without choosing an async runtime, Provider transport, Tool executor, or Git adapter. A separate provisional `DurableTeamRuntime` now plugs the same transactions into the Phase 1 file Ledger.

## Interface

```rust
let mut team = TeamRuntime::new(max_active_agents)?;
let commit = team.dispatch(command)?; // Agent commands carry an AgentSession
let view = team.snapshot();
let events = team.event_log();
let recovered = TeamRuntime::recover(max_active_agents, events.iter().cloned())?;

let mut durable = DurableTeamRuntime::open(team_ledger_path, max_active_agents)?;
let commit = durable.dispatch(command)?; // CommitDurability::Synchronous
let inspection = DurableTeamRuntime::inspect(team_ledger_path, max_active_agents)?;
let read_only = inspection.snapshot(); // shared lock; no create, truncate, or repair

let (mut kernel, recovered) = RuntimeKernel::open_with_team(
    runtime_ledger_path,
    team_ledger_path,
    max_active_agents,
)?;
let root_operation = kernel.dispatch_team(TeamCommand::AdmitRoot {
    task,
    budget,
    capabilities,
})?;
kernel.acknowledge_team_operation(root_operation.operation)?;
let rebound = recovered.into_sessions(); // complete per-open set; no ID lookup
let operation = kernel.dispatch_team(command)?;
kernel.acknowledge_team_operation(operation.operation)?;
```

- `TeamRuntime::dispatch` validates one typed `TeamCommand`, constructs one atomic in-memory event transaction, appends it, and only then replaces the Runtime Fold.
- `DurableTeamRuntime::dispatch` prepares the same validated transaction, checks the locked Ledger Head, synchronously appends it, receives a checksum-bound receipt, and only then publishes the Runtime Fold. Planning, encoding, Head, or append failure leaves the projection and identifiers unchanged; ambiguous durability poisons the writer and requires reopen rather than blind retry.
- `AgentSession` is process-local execution authority issued by this Runtime. Canonical Agent IDs remain inspectable but cannot authorize commands, old sessions fail after recovery, and the public Team interface intentionally exposes no ID-to-session rebind. `RuntimeKernel::open_with_team` owns the adapter writer and returns one `KernelTeamRecovery` containing the complete non-terminal Session set derived from validated replay; it never accepts an Agent ID to mint authority.
- `RuntimeKernel::dispatch_team` is the Kernel's single Team write interface, including typed root admission. It allocates a monotonic non-authorizing `TeamOperationId`, commits that identity in the same checksummed transaction as the command Events, and does not publish a Team Fold before its synchronous receipt. A committed operation blocks later Team commands until `acknowledge_team_operation` durably records acknowledgement in the same Team Ledger; dispatch and acknowledgement both require a ready Provider Runtime and no pending Tool reconciliation, and duplicate acknowledgement is a no-op.
- `snapshot` returns an immutable, deterministic projection ordered by canonical identifiers.
- `event_log` exposes the current canonical events for tests and projection inspection.
- `recover` accepts only contiguous, complete transactions and rebuilds the same projection or fails closed. Raising the caller-supplied Active Agent limit preserves an older valid scheduling projection during replay; the next Team command reconciles new capacity under the new limit, while lowering the limit below any historically active count fails closed.
- `DurableTeamRuntime::inspect` reuses the private versioned decoder and Team
  recovery fold behind a shared read-only Ledger lock. It returns an immutable
  projection, Head, operation records, and incomplete-tail byte count without
  creating, truncating, repairing, or minting Sessions. Checksum, schema, state,
  path, and lock failures remain errors.

Plain `TeamRuntime` commits remain `CommitDurability::Volatile` and cannot drive a user-visible acknowledgement. `DurableTeamRuntime` returns `CommitDurability::Synchronous` only after the dedicated Team Ledger has flushed a complete checksummed transaction. The core Runtime Kernel now owns that adapter when opened with a dedicated Team Ledger, gates root admission, persists operation identity and acknowledgement records, exposes pending operation status through `KernelTeamSnapshot`, and issues one consumable recovery bundle per open for Active, Dormant, and Blocked owners while excluding terminal Agents. Sessions inside the bundle remain ordinary copyable process-local capabilities. Standalone adapter recovery still invalidates every old Session. Operation IDs remain inspection and reconciliation identities, never Agent authority. The product CLI and local stdio App Server expose bounded Delegation, messaging, Completion Capsule, failure, cancellation, explicit Team-operation acknowledgement, and one exact Active-Agent Provider Turn. A request's Agent ID may select only a matching Session already present in the one validated recovery bundle; it cannot mint, persist, or reuse Session authority. Delegation may persist one bounded Config-owned default Preset ID on the child Agent, but never the Preset definition, credential, Provider authority, or parent Session. A committed mutation blocks later Team commands until its operation is acknowledged, and the redacted list/status projection exposes pending operation IDs for recovery. The product limit is two Active Agents. Ordinary headless Provider Turns continue under the root; `agent.turn` resolves the selected Agent's inherited ID, freezes Agent-scoped Config/Provider Epochs, and keeps recovery bound to the persisted Turn or Tool-call owner. The Direct VT `/agent` browser omits messages, terminal reasons, Completion Capsules, capability/scope labels, and Sessions. Its `A` menu explicitly delegates, messages, completes, fails, cancels, or acknowledges through rebound Session authority; restart and failed acknowledgement preserve the pending recovery path. Workspace Coordinator now exposes bounded local facts, shared read-only/exclusive read-write Lease acquisition, and digest-based Read Set capture/revalidation outside Team Events. Unix Git worktree allocation and read-only merge outcomes are available through the product adapter; automatic merge/cleanup, Windows adapters, and broader Tool catalogs remain pending.

## Command Flow

```mermaid
flowchart LR
    C["Typed TeamCommand"] --> V["Validate invariants"]
    V --> E["OperationCommitted + command Events"]
    E --> L["Append, flush, sync"]
    L --> F["Publish deterministic Fold"]
    F --> R["Return operation receipt"]
    R --> A["Explicit OperationAcknowledged transaction"]
```

Every Team transaction carries a monotonic transaction ID, Event sequence, zero-based position, and total Event count. Team recovery rejects gaps, reordered positions, mixed transaction IDs, changed transaction sizes, incomplete tails, invalid transitions, and non-quiescent scheduler state. The separate Phase 1 file Ledger reports and repairs one checksummed torn final frame only on explicit writer recovery; read-only inspection never mutates it.

## State Model

| Task state | Agent state | Meaning |
| --- | --- | --- |
| `Pending` | `Dormant` | Waiting for Task dependencies |
| `Ready` | `Dormant` | Runnable, but no Active slot is available |
| `Running` | `Active` | Consuming one bounded execution slot |
| `Blocked` | `Blocked` | A dependency failed, was cancelled, or became blocked; an Active parent may explicitly requeue an eligible child after the dependency is clear, or `Cancel` it |
| `Succeeded` | `Succeeded` | Completion Capsule accepted |
| `Failed` | `Failed` | Explicit terminal failure recorded |
| `Cancelled` | `Cancelled` | Explicit terminal cancellation recorded |

Root admission and Delegation automatically reconcile scheduling. When a terminal transition releases a slot, the lowest canonical Ready Task activates in the same transaction. No timer, polling loop, or background thread exists.

## Invariants

1. One `TeamRuntime` has at most one root Agent.
2. Every Task has exactly one current Owner, represented by exactly one Agent in this slice.
3. A new Task may depend only on existing Tasks; canonical IDs are contiguous, so the explicit Task graph is acyclic by construction.
4. A delegated Task cannot depend on its parent or any ancestor Task. That would create an implicit cycle because ancestors cannot finish while descendants remain non-terminal.
5. Child scope and Capability Snapshot are exact set subsets of the parent's values.
6. Child budgets are reserved monotonically from the parent's unreserved token and tool-call budget. This conservative slice does not refund unused reservations.
7. Only a valid `AgentSession` for an Active Agent may Delegate, send messages, complete, fail, or requeue a direct child. Dormant and Blocked Agents consume no Active slot; Blocked Agents may be explicitly cancelled. Requeue is parent-authorized Team scheduling only and never replays Provider or Tool effects.
8. A parent cannot complete, fail, or cancel while a child remains non-terminal.
9. Failed, cancelled, or blocked dependencies synchronously block waiting dependents.
10. Task titles, scope labels, dependency lists, Capability Snapshots, tool names, messages, terminal reasons, and Completion Capsules are bounded before entering the Event Ledger. Completion Capsule list counts are bounded separately so empty strings cannot evade byte accounting; larger future payloads belong in Artifacts.
11. A delegated child may carry one bounded inherited Model Preset ID. The ID is immutable Agent metadata, grants no authority, and is not rewritten when Config defaults change.

## Commands and Events

`AdmitRoot`, `Delegate`, `SendMessage`, `Complete`, `Fail`, `Cancel`, and parent-authorized `Retry` are the command families in this slice. `Retry` (surfaced as Team `requeue`) accepts only an Active parent and a direct `Blocked`, `Failed`, or `Cancelled` child with no outstanding children and no still-blocking dependency; it resets Task/Agent scheduling to `Pending`/`Dormant` and lets normal reconciliation schedule it again. Delegation has a compatibility form without a Preset and a product form carrying one optional validated inherited Preset ID. Agent commands carry an opaque `AgentSession`, while Events retain the stable Agent ID. They emit canonical ownership, lifecycle, coordination, inherited-Preset identity, and Completion Capsule Events. Direct state mutation is private to the fold implementation.

Provider output, Tool effects, approvals, Workspace Leases, Read Sets, merge outcomes, Config Epochs, Provider Epochs, and Context Checkpoints remain deliberately absent from the Team Event model. Tool identity, approval, and effect recovery now live in a separate deep Tool Runtime because their authority and retry rules differ; they are not generic Team fields.

## Dependency Strategy

- Canonical Task, Agent, budget, capability, Event, and fold logic is in-process and uses only the Rust standard library.
- The in-memory Event Ledger remains the volatile policy-test implementation.
- `DurableTeamRuntime` is an external adapter over the provisional Phase 1 file Ledger. It uses a dedicated Team Ledger, Team Event schema 4 for Team requeue plus optional inherited-Preset identity, historical schema-1/schema-2 replay with no inherited ID and schema-3 replay with inherited identity, a versioned bounded codec for all 21 Team Event kinds, exclusive writer ownership, synchronous receipts, complete-prefix replay, and fail-closed schema/checksum/state validation.
- The adapter is not the final storage choice or migration contract. The Kernel ownership seam is implemented in core, while candidate selection and production migration remain separate decisions.
- Provider Runtime, Tool Runtime, and Workspace Coordinator retain separate interfaces because their retry, authority, and effect-ordering rules differ.
- The product and acceptance binaries continue to depend inward on `greentyper-core`; the core never depends on them.

## Next Slices

1. Harden the configured remote Responses path with live inference conformance,
   broader TLS platform evidence, configurable proxy policy, and explicit
   reconnect/retry rules; broaden the fixed public `local.echo` path only after
   caller-selected process policy and complete Windows Job evidence can fail
   closed.
2. Extend the current Kernel-owned ProductDriver and CLI receipt/approval path
   into TUI/App Server presentation, durable resumable Tool-result references,
   and richer reconciliation without exposing Agent Session authority.
3. The Unix product adapter now adds Git worktree allocation, explicit
   merge/conflict preflight outcomes, and one explicit clean-branch merge on
   top of the existing Workspace Lease and Read Set writer gate. It does not
   resolve conflicts or schedule merges in the background. Add audited Windows
   reparse-safe Lease/Read Set/Git adapters before enabling those operations
   there.
4. Extend byte-offset process termination from the Team Ledger transaction seam to every product Provider/Tool delivery and acknowledgement boundary.
5. Exercise Performance Contract workload P3 with two Active Agents on the Target Machine and four on FMDev; measure Dormant increment rather than assuming it.
