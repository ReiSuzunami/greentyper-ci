# Delivery Model

Status: active from 2026-08-13.

This document is the execution rule for turning the roadmap into progress. The
[implementation plan](implementation-plan.md) remains the product roadmap; this
document defines when a piece of work is allowed to count as delivered. When
roadmap prose uses “batch” descriptively, it does not create a delivery Batch;
only a numbered ledger row and its settlement record count.

## Current Goal

Maximize **settled usable user-flow coverage per unit of time**. Lock one small
user result, make it observable, freeze it, and settle it. Do not pursue
roadmap completion, Phase completion, or edge-case unanimity inside a feature
Batch. Those belong to separate Phase or Release reviews.

Progress has three explicit tracks:

```text
Implemented: user result is observable locally; provisional only.
Settled: Batch Gate is complete; counts toward feature progress.
Released: declared Release Gate passed; release evidence only.
```

Never report these as one number. A result can be `Implemented` while its Batch
waits for settlement; it cannot be counted as `Settled` until the Batch reaches
its terminal success state.

The only active execution unit is **one Batch = one user result in one domain**.
Lock it, make it usable, stop, then settle it once. Phase is a roadmap label;
Milestone is the immutable user-result decision; Batch is the implementation and
settlement package. Local code, focused checks, reviews, commits, and running CI
are provisional. **Only a green Batch settlement increments progress.**

This is a hard stop against scope drift: when the locked result works, stop
adding functionality. Put adjacent ideas, extra review findings, and non-
blocking test gaps in the follow-up register, then settle. Do not reopen the
Batch because another domain has a more interesting issue.

The default loop is deliberately non-interleaved:

1. **Select:** choose the shortest independent user result, not the largest
   open Phase item.
2. **Lock:** write one Milestone lock with one domain, one exit, one to five
   slices, and explicit non-goals.
3. **Cover:** implement only that result. Skip tests, polish, architecture
   cleanup, and adjacent domains unless they block the exit.
4. **Settle:** when the exit is visible, freeze code, add at most one happy-path
   and one blocker check, update docs once, clean, push, and run one CI workflow
   for the exact feature SHA.

No “temporary” CI loop exists between steps. No new feature starts while the
locked Batch is Building, Ready, or Settling.

Do not alternate between passes. Non-blocking findings go to the follow-up
register and stay out of the current Batch.

### Convergence and step signals

Every active Batch carries one current signal. A signal is an instruction, not
status prose; it determines the only allowed next action.

| Signal | Emit when | Only allowed next action |
| --- | --- | --- |
| `LOCK` | User, entry, result, blocker boundary, and non-goals are fixed. | Start the first Slice. |
| `STEP` | Slice produced observable evidence. | Continue the smallest next Slice in same Milestone. |
| `SPLIT` | Second result/domain, sixth Slice, or two consecutive Slices without new user evidence. | Freeze current scope; create a later Milestone. |
| `FREEZE` | Milestone Exit is observable. | Stop feature work; enter Ready. |
| `SETTLE` | Critical checks pass and scope is frozen. | Perform one Batch settlement. |
| `WAIT_EXTERNAL` | Exact-SHA CI or an explicitly external dependency is running. | Wait only; no code, scope, polling, or Phase switch. |
| `REPAIR_ONCE` | CI reports one confirmed code failure. | One repair commit, same Milestone, then one replacement run. |
| `RERUN_ONCE` | CI reports one infrastructure/flaky failure. | Rerun same SHA once. |
| `CLOSE` | Settlement evidence is green and recorded. | Mark Batch `Settled`; select next Milestone. |
| `FAIL` | Repair/rerun budget is exhausted or exit cannot be met. | Mark Batch `Failed` or `Cancelled`; release slot. |

No signal may be skipped silently. `Blocked` is a holding state, never a
terminal state: it must become `REPAIR_ONCE`, `RERUN_ONCE`, `FAIL`, or an
explicitly resumed Batch.

Each Slice note must append the signal and remaining budget:

```text
Signal: STEP
Budget: slices 2/5; new checks 1/2; code repairs 0/1; CI runs 0/1
```

When `FREEZE` is emitted, remaining feature budget is discarded. New ideas go
to the follow-up register, even if they are small.

## The unit of progress

Progress is counted in **verified user results**, not in changed lines, passed
unit tests, or opened pull requests.

| Term | Meaning | Is it a completion gate? |
| --- | --- | --- |
| Phase | Broad roadmap grouping (Provider, Context, Team, and so on). It owns a queue of Milestones and separate exit criteria. | Never. An open Phase does not block another settled Milestone. |
| Milestone | One immutable sentence describing one user-visible result in one domain. It has one owner, one boundary, and one observable exit. | Yes. This is the product decision unit. |
| Batch | Delivery package for exactly one Milestone: one to five slices, normally two to four, never padded. | Yes. A Batch settles once and is counted once. |
| Slice | Small implementation step toward the same locked Milestone; it carries only the check needed to keep moving. | No. Slice success is local evidence only. |
| Release Gate | Full platform, performance, packaging, and acceptance bar for a release candidate. | Only when explicitly declared; never an ordinary feature tax. |

Examples of a Milestone:

- “A user can resume a blocked Provider Turn without repeating an external
  effect.”
- “A user can apply one guarded file change and receive a stale-read refusal
  when the file changed.”

“Improve the Provider phase” and “finish the docs” are not Milestones because
they do not describe one usable result.

Phase does not equal Milestone. A Phase may contain many Milestones and remain
`Active` while several user results ship. Milestone does not equal Batch. The
Milestone is the immutable user-result lock; the Batch is the short
implementation and settlement cycle for that lock. No feature ledger row may
be phrased as “finish a Phase”. If a roadmap item would produce two user
results, split it into two Milestones before coding.

### Fast coverage protocol

Use this required four-line note after each Slice; do not turn it into a review
meeting. Store it in the active Batch note or the follow-up register:

```text
Slice: <what a user can do now>
Breadth: <new usable domain/flow, or none>
Depth: <local / recoverable / externally verified>
Next: <the smallest remaining step for the same Milestone>
```

The note may be approximate, but `Slice` must name observable evidence (command,
screen, API response, or durable state). It exists to show feature coverage
moving, not to manufacture a precise percentage. Official progress is updated
only once at Batch settlement.

Coverage is reported in two numbers, not one invented roadmap percentage:

- **Depth:** how much of the locked user result works (local, recoverable, or
  externally verified).
- **Breadth:** how many roadmap domains have at least one settled end-to-end
  flow.

Code volume, test count, review count, and CI count are never coverage metrics.

### Development versus settlement

Keep these modes separate:

| Mode | Allowed work | Stop condition |
| --- | --- | --- |
| Building | Implement slices in the locked domain; run only Slice Gate checks. It is acceptable to leave non-blocking polish for settlement. | The one user result is observable. |
| Ready | Freeze feature scope; add at most one happy-path and one blocker check. | No blocker remains for the locked exit. |
| Settling | Update docs once, run the selected local gate once, clean, commit, push, and run one exact-SHA CI workflow. | CI is green, or Batch is marked Blocked after the allowed repair/rerun. |
| Settled | Record result, depth, breadth, evidence, and deferred work. | Move to the next Milestone. |

CI is a settlement action, not a development loop. Do not trigger CI for every
Slice, review finding, or documentation edit. Do not rerun unchanged checks
whose inputs and environment have not changed. Repeated PASS reviews do not
create progress; one owner and one independent review are enough unless a
blocker is found.

### Phase status and exit

Phases are roadmap containers, not work items. A Phase may contain many
Milestones and may remain open while later Phases ship results. “Phase complete”
is a separate, explicit roadmap review against that Phase's exit criteria; it is
never implied by a feature Batch and never runs inside the ordinary feature
loop. A pending Phase exit does not block the next locked Milestone unless the
next Milestone depends on a missing Phase invariant.

Every Phase is labelled `Planned`, `Active`, `Coverage Complete`, `Deferred`,
or `Closed`. `Active` means only that it may supply the next Milestone. Before
activation, Phase must list a finite `Required Milestones` set, one optional
integration smoke, and explicit deferred work. `Coverage Complete` means every
required Milestone is `Settled` and the smoke passes. `Closed` is recorded only
at a dedicated Phase review with its own evidence. Phase review and Release
Gate work never enter a normal feature Batch.

Phase closure is finite, not aspirational. A later discovery never reopens a
closed Phase; it creates a new Milestone in a new Phase review cycle.

### Milestone lock

Before implementation starts, write this five-line lock in the batch note:

```text
Milestone: <one user result>
Domain/Phase: <one roadmap area>
Slices: <one to five concrete implementation slices>
Non-goals: <everything explicitly deferred>
Critical checks: <one or two blocker-critical checks selected before coding>
Exit: <the one observable result that makes the milestone usable>
```

Also record Milestone and Batch states separately:

```text
Milestone status: Locked | Building | Ready | Achieved | Cancelled | Superseded
Batch status: Open | Settling | Blocked | Settled | Failed | Cancelled
Signal: LOCK | STEP | SPLIT | FREEZE | SETTLE | WAIT_EXTERNAL |
        REPAIR_ONCE | RERUN_ONCE | CLOSE | FAIL
Budget: slices 0/5; new checks 0/2; code repairs 0/1; CI runs 0/1
```

`Ready` means the user result works and the locked critical checks pass. It is
not progress. `Achieved` means the Milestone Exit is met and feature work is
frozen. `Settled` belongs only to the Batch. The lock cannot be edited to add a
new result; changing the user result starts a new Milestone.

The Phase may remain `Active` while its finite Milestones settle. The Milestone
cannot remain open after its Exit is met: emit `FREEZE`, mark it `Achieved`,
settle its Batch, and move to the next Milestone. A new idea belongs in a later
slot unless it blocks the locked Exit.

Milestone and Batch state machines:

```text
Milestone: Locked → Building → Ready → Achieved
Milestone terminal alternatives: Cancelled | Superseded

Batch: Open → Settling → Settled
Batch holding state: Blocked
Batch terminal alternatives: Failed | Cancelled
```

`Blocked` never counts and never releases the slot by itself. It must resolve to
`Settled`, `Failed`, or `Cancelled`.

## Hard batch rules

1. Lock one user result before coding. Write the Milestone lock and do not
   change it mid-Batch.
2. Keep the batch in one domain. A discovery, security, documentation, or
   architecture finding does not open a second domain.
3. Stop as soon as the exit is observable. Do not add a sixth slice, a second
   user result, or “completeness” work; register it for a later Milestone. If
   one slice grows beyond roughly 800 changed lines, crosses domains, or
   produces a second user result, split it before continuing.
4. A finding may interrupt the batch only when it is one of these blockers:
   - the advertised flow cannot complete;
   - data, Ledger recovery, or atomicity can be corrupted;
   - an external effect can be repeated or authority can cross an Agent;
   - the changed code does not compile or its critical path test fails; or
   - the change violates the formal Performance Contract.
5. During the coverage pass, the default new-test budget is zero. Run a check
   only when code would otherwise be unobservable, or when persistence,
   security, credentials, billing, recovery, authority, or a formal contract
   could be violated. At settlement, add at most one happy-path and one
   fail-closed check. Skip redundant tests and repeated reviews; use one owner
   and at most one independent review unless a blocker is found. “More tests”
   is not an exit criterion by itself.
6. Update product/status documentation once, at Batch settlement. During
   implementation, keep discoveries in the follow-up register.
7. Do not start another feature while the active Batch is unsettled. A local
   implementation, a passing targeted test, or an unpushed commit is not
   progress yet.
8. Do not switch Phase while current Batch is `Building`, `Ready`, or
   `Settling`. A Phase switch requires `Settled`, `Failed`, or `Cancelled`.
9. A Phase must have finite Required Milestones. It may remain `Active` while
   those Milestones settle, but it must eventually become `Coverage Complete`
   or `Deferred`; it is not an infinite work queue.
10. A Milestone lock cannot grow. If new work creates a second user result,
    split it into a later Milestone; if it is only polish or evidence, defer it.

### Fixed budgets

Budgets are hard stop signals, not estimates:

- maximum five Slices per Batch;
- maximum two new settlement checks total (one happy path, one blocker);
- maximum one code repair commit after CI failure;
- maximum one same-SHA infrastructure rerun;
- maximum one feature CI workflow for the final SHA.

Critical checks selected in the lock consume the same two-check settlement
budget when they are newly added. Existing checks may be reused without adding
budget. Budget exhaustion emits `SPLIT` or `FAIL`; it never authorizes another
review cycle.

## Three gates, not one endless gate

### Slice Gate

Run only the checks needed to keep the current slice moving. Slice Gate is a
development checkpoint, not progress credit:

- formatter/check for touched crates;
- the changed package's focused test or deterministic fixture only when the
  slice changes executable behavior or a blocker boundary;
- `git diff --check`.

The Slice Gate never runs the whole workspace, release packaging, FMDev, or
Target acceptance. It exists to keep implementation moving, not to declare
progress.

### Batch Gate

Run exactly once, after all slices for the one result are usable. Do not run
the full gate after every slice, review, or intermediate discovery:

1. the one or two blocker-critical checks selected in the Milestone lock;
2. one local regression pass appropriate to the risk. Ordinary UI, projection,
   and read-only surface work uses format/check plus the critical smoke only.
   Persistence, security, credentials, billing, recovery, authority, and
   cross-platform changes use the full relevant local gate. A full workspace
   test/clippy pass is a release-style choice, not a default per-Batch tax;
   docs-only changes use the relevant link/format check;
3. one documentation update and link/format check;
4. removal of temporary files and a clean intended diff;
5. one commit and push to configured remotes; and
6. one workflow run for that exact commit SHA, with every required job green.

“One CI run” means one workflow run for one commit, not one job. Every required
job must pass; a green smoke step alone is not a Batch Gate. CI is settlement
evidence, not a development feedback loop.

The checks named in the Milestone lock are the blocker budget for the Batch;
they are not an invitation to add more tests. At settlement, add at most one
new happy-path check and one new fail-closed check. The selected local
regression pass is one existing, risk-appropriate command, not a second test
expansion cycle.

For the current CI workflow, required jobs are `Quality / macOS ARM` and
`Windows x64 build`. Their acceptance, transport, terminal, allocator, and
package steps are part of those jobs. A green smoke step alone is not a Batch
Gate.

Only a successful Batch Gate increments the progress ledger. If CI fails,
repair the current Batch only. Classify the failure before changing code:

- **Code failure:** one repair commit and one replacement workflow run are
  allowed. Do not add a new slice or change the Milestone.
- **Infrastructure/flaky failure:** one rerun of the same commit is allowed;
  do not create a code commit just to rerun CI.

If the replacement run (or the one infrastructure rerun) fails again, mark the
Batch **Blocked**, record the failing job and reason, and stop. Do not create
more commits merely to chase CI. A Blocked Batch counts zero until explicitly
resumed or waived by the release owner.

Documentation-only governance or status corrections do not create a feature
Batch and do not increment progress. They receive a link/format/diff check and
may use a `[skip ci]` commit; they do not trigger a feature CI run.

### Release Gate

The full Windows/macOS/Linux matrix, crash/fuzz/security coverage, FMDev
30-run evidence, Target evidence, migration coverage, and signed portable
packaging belong here. They run at a declared release candidate or release
batch, not after every small user result. CI smoke samples are correctness
evidence; they are not performance approval.

A Release Gate declaration must record, at minimum:

- candidate commit SHA;
- required gate/job list;
- evidence links or artifact names;
- owner and status (`Pass`, `Blocked`, or `Waived`); and
- waiver authority and reason when status is `Waived`.

That SHA is the unit of release evidence; run it once for that candidate and
rerun only when the candidate changes or a recorded gate failure is repaired.
R1 being pending does not block ordinary feature Milestones. Release Gate
evidence cannot be substituted by a feature Batch's CI smoke run.

## Stop and defer protocol

When a non-blocking issue appears:

1. record it with file/area, impact, and the earliest Milestone it belongs to;
2. state whether it affects the current user result;
3. leave the current implementation unchanged unless it is a blocker; and
4. continue to Batch settlement.

The follow-up register is allowed to contain incomplete documentation,
additional platform evidence, richer UI, performance measurement, and future
authority surfaces. They are not silently folded into the active Batch.

### CI stop rule

CI is a Batch settlement action, not a development loop. No CI run is required
while a Batch is `Building` or `Ready`. After one code repair or one same-SHA
infrastructure rerun, a second failure makes the Batch `Blocked`; record the
job, SHA, and reason, stop changing scope, and wait for an explicit decision.
Select the next Milestone only after the current Batch is `Settled` or formally
marked `Blocked`.

Background work, periodic polling, and resident discovery remain prohibited by
the formal [Performance Contract](performance-contract.md) unless a separately
approved exception changes that contract first.

## Coverage reporting

Every settled Batch reports depth and breadth separately:

- **Depth:** the exact user result made usable;
- **Breadth:** roadmap domains with at least one usable end-to-end flow;
- **Progress:** settled feature-Batch count and next shortest independent user
  result; and
- **Release readiness:** reported separately, never inferred from feature
  breadth.

Use a rough coverage band or domain count, not a false precise percentage. This
estimate is updated once at Batch closeout, never after each slice.

Current breadth: Provider/Model, Config/Credentials, Agent/Team, Context/Usage,
project Skills, Workspace, local App Server, and bounded MCP discovery each have
at least one usable flow. Complete Packaging/Acceptance does not. This is **8 of
9 roadmap domains with a usable base flow**, not 89% release readiness.

## Current delivery ledger

The roadmap phases are intentionally broad. The execution ledger below is the
smaller list used for progress accounting.

| Result slot | User result | State | Counting rule |
| --- | --- | --- | --- |
| B0 | Complete one configured Provider Turn. | Baseline shipped | Reference capability; not a new Batch in this ledger. |
| B1 | Recover a blocked/prepared Provider Turn without duplicate delivery. | Settled | Counted after commit, push, and CI evidence. |
| B2 | Use the non-secret App Server Config/Agent control surface safely. | Settled | Counted after commit, push, and CI evidence. |
| B3 | Inspect, checkpoint, reduce, and recover Context without leaking Item text. | Settled | Counted after commit, push, and CI evidence. |
| B4 | Requeue an eligible failed/blocked Team child through parent authority. | Settled | Team-only mutation; Provider retry remains a separate result. |
| B5 | Apply one guarded Workspace file change or reject a stale read set. | Settled | Unix guarded path; Windows/Git worktrees remain separate results. |
| B6 | Read the bounded Context handoff through the App Server. | Settled | Commit `36de81a`; CI `31664070914` passed macOS ARM and Windows x64. |
| B7 | Allocate isolated Git worktrees and report merge/conflict outcomes. | Settled | Unix `workspace allocate` + read-only `workspace merge-check`; automatic merge, cleanup, and Windows adapters deferred. Functional commit `2388721`; Batch CI `31667302886` passed macOS ARM and Windows x64. |
| B8 | List a project Skill by stable content hash and run it once through the existing approval/capability-bounded `local.echo` path, with durable reuse on repeat. | Settled | Project-only, fixed `local.echo` path; MCP transport/client, arbitrary scripts, background polling, TUI, and general Skill catalog/authority deferred. Commit `67b36b0`; Batch CI `31670709539` passed macOS ARM and Windows x64. |
| B9 | List and explicitly run a project Skill through App Server, with durable reuse on repeat. | Settled | Startup-directory root; fixed `local.echo` only; no MCP, scripts, background discovery, or capability grant. Commit `84ae83f`; CI `31673669506` passed macOS ARM and Windows x64. |
| R1 | Install, recover, measure, and pass the declared release acceptance. | Pending | Release Gate only; never used to block a small feature Batch. |
| B10 | Recover an early-failed active child Agent Turn through App Server, then deliver and acknowledge exactly once. | Settled | Commit `4931e8a`; CI `31679750127` passed macOS ARM and Windows x64. |
| B11 | List registered Git worktrees through CLI without exposing absolute paths or changing repository state. | Settled | Commit `a684065`; CI `31682587546` passed macOS ARM and Windows x64. |
| B12 | Remove one clean, registered Unix Git worktree through CLI while preserving its branch. | Settled | Commit `e80fc1d`; CI `31686469234` passed macOS ARM and Windows x64. |
| B13 | Explicitly merge one clean, conflict-free Unix Git branch through CLI while preserving the source branch. | Settled | Commit `947bf7a`; CI `31689627871` passed macOS ARM and Windows x64. |
| B14 | Delegate a child Agent with an explicit bounded Workspace capability and observe the grant in CLI status. | Settled | Commit `5fc24da`; Batch CI `31693096637` passed macOS ARM and Windows x64; capability snapshot only; Workspace execution binding remains deferred. |
| B15 | List configured Model Presets, effective default, policy, and fallback IDs through a read-only CLI projection. | Settled | Commit `7190da4`; CI `31696507296` passed macOS ARM and Windows x64. |
| B16 | Create one minimal Model Preset from an existing Provider Profile through CLI. | Settled | Commit `bcde3ef`; CI `31700985043` passed macOS ARM and Windows x64. |
| B17 | Refresh a configured Provider discovery snapshot, inspect the current catalog, and accept one verified discovered model as a Model Preset through CLI. | Settled | Commit `b6c75e2`; CI `31705477912` passed macOS ARM and Windows x64. |
| B18 | Remove one clean registered Git worktree and optionally delete its already-merged branch through Unix CLI. | Settled | Commit `813c580`; CI `31709432794` passed Quality / macOS ARM and Windows x64. |
| B19 | Publish one bounded Context checkpoint through App Server and receive a redacted summary. | Settled | Commit `988438d`; CI `31714088503` passed Quality / macOS ARM and Windows x64. |
| B20 | Connect to one explicitly selected local stdio MCP server and list its bounded tools through CLI. | Settled | Commit `919c4ed`; CI `31720409216` passed Windows x64 and Quality / macOS ARM. Discovery only; no shell, execution, or background connection. |

Current official progress is **twenty CI-settled feature Batches**. R1 remains
a separate Release Gate and does not block feature coverage. Implemented,
Settled, and Released counts remain separate.

## Last execution slot

```text
Batch: B20
Status: Settled
Milestone: A CLI user can explicitly launch one local stdio MCP server and list
  its bounded tool names and schemas.
Domain/Phase: Skills and MCP (Phase 5 / CLI discovery)
Slices: parse one exact-command MCP CLI request; perform bounded current-protocol
  stdio discovery and `tools/list`; return a deterministic bounded projection.
Non-goals: tool calls, approval/effect execution, persistent server Config,
  credentials, remote HTTP, legacy MCP, shared transports, prompts/resources,
  TUI/App Server surfaces, background polling, and automatic reconnect.
Critical checks: one compliant fixture returns its tool list; one malformed or
  oversized or hung server response fails closed without creating Config or
  Ledgers.
Exit: `greentyper mcp tools ...` prints bounded JSON for one explicitly selected
  server and exits; invalid servers terminate within the fixed timeout.
```

B19 and B20 are settled below. Select a fresh independent Milestone; adjacent
MCP execution, Context, Agent, Provider, Workspace, and Release Gate ideas stay
deferred until explicitly locked.

### B20 Slice notes

```text
Slice: CLI accepts one explicit MCP stdio command after `--`.
Breadth: MCP becomes a reachable product surface.
Depth: local command selection only; no server process yet.
Next: launch one bounded stdio session and negotiate current MCP discovery.

Slice: CLI negotiates the current protocol and returns sorted bounded tool descriptors.
Breadth: Skills/MCP now has one usable discovery flow.
Depth: one foreground stdio session; no execution, persistence, or authority.
Next: fail closed on malformed, oversized, and hung servers.

Slice: invalid servers terminate within the fixed timeout and create no product state.
Breadth: none; same locked MCP discovery result.
Depth: reusable Unix process-group / Windows Job containment plus bounded framing.
Next: closeout record complete; select a fresh independent Milestone.
```

### B20 closeout

- **Depth:** CLI accepts one explicitly selected local stdio MCP server, performs
  bounded current-protocol initialization and `tools/list`, and returns sorted
  bounded tool descriptors. Malformed, oversized, or hung servers fail closed
  within the fixed timeout.
- **Breadth:** bounded MCP discovery adds one usable MCP flow; overall usable
  base is now 8 of 9 roadmap domains. Tool execution, approval, and remote MCP
  remain separate results.
- **Critical checks:** compliant discovery fixture passed; malformed,
  oversized, and hung-server fixtures failed closed without Config or Ledger
  creation. Local format/check/clippy/diff gates passed before settlement.
- **Commit/push:** `919c4edae61b145f195ee7ad36289ea0dc9cb6d8`, pushed to
  `origin/main` and `ci/main`.
- **Batch CI:** run `31720409216` for the exact SHA passed Windows x64 and
  Quality / macOS ARM.
- **Deferred:** MCP tool calls, approval/effect execution, persistent server
  Config, credential binding, remote transports, shared transports, prompts,
  resources, elicitation, TUI/App Server surfaces, and background discovery.
- **Next:** select one fresh independent Milestone; do not reopen B20.

### B19 Slice notes

```text
Slice: `context.reduce` accepts bounded optional raw-byte and raw-item limits.
Breadth: App Server gains its first explicit Context mutation.
Depth: local typed request and fixed invalid-value response.
Next: route the request through the existing Context Safe Barrier.

Slice: App Server reuses product preflight plus Runtime prepare/publish.
Breadth: none; same locked Context result.
Depth: durable checkpoint publication with existing CAS/recovery semantics.
Next: return a redacted bounded result and prove fail-closed behavior.

Slice: response exposes only head/count/byte/token facts; busy Runtime refuses.
Breadth: complete App Server Context reduction flow.
Depth: recoverable local flow; Runtime bytes change only on success and all
  relevant bytes remain identical on refusal.
Next: closeout record is complete; select a fresh independent Milestone.
```

### B19 closeout

- **Depth:** App Server accepts one bounded `context.reduce` request, runs the
  existing Context preflight and Safe-Barrier prepare/publish path, and returns
  only bounded checkpoint metadata. Busy or incomplete state returns a fixed
  error without Runtime/Team/Tool/Config writes.
- **Breadth:** Context/App Server mutation coverage expanded; overall usable
  base remains 7 of 9 roadmap domains. This is deeper Context coverage, not a
  new roadmap domain.
- **Critical checks:** focused `context.reduce` integration check passed; the
  App Server suite (33 tests), format/check/clippy/diff gates passed locally.
- **Commit/push:** `988438dcc807dec24cc76d946dd2a753d39ac7ab`, pushed to
  `origin/main` and `ci/main`.
- **Batch CI:** run `31714088503` for the exact SHA above passed Quality /
  macOS ARM and Windows x64 build.
- **Deferred:** provider-native compaction, semantic Memory, external Artifact
  storage, TUI/background reduction, cross-Ledger transactions, credentials,
  new Agent/Provider authority, and Release Gate work.
- **Next:** choose one fresh independent Milestone; do not reopen B19.

### B18 closeout

- **Depth:** local Unix CLI success, default branch preservation, explicit
  merged-branch deletion, and unmerged fail-closed refusal.
- **Breadth:** Workspace mutation coverage expanded; overall usable base remains
  7 of 9 roadmap domains, with MCP and complete Release packaging pending.
- **Critical checks:** `cargo test -p greentyper --test workspace_cli --locked`
  passed (3 tests); full local format/check/test/clippy/diff gate passed.
- **Commit/push:** `813c580`, pushed to `origin/main` and `ci/main`.
- **Batch CI:** run `31709432794` for exact SHA
  `813c580919c18c0430d7c38909d4119e2b3508d8`; Quality / macOS ARM and Windows
  x64 build both passed.
- **Deferred:** force deletion, current/root branch deletion, detached/prunable
  cleanup, automatic/background cleanup, Windows Git adapters, TUI/App Server
  surfaces, conflict resolution, and Release Gate evidence.
- **Next slot:** choose one fresh independent Milestone; do not reopen B18.

## Last settled Batch

```text
Batch: B20
Status: Settled
Milestone: A CLI user can explicitly launch one local stdio MCP server and list
  its bounded tool names and schemas.
Domain/Phase: Skills and MCP (Phase 5 / CLI discovery)
Evidence: commit `919c4edae61b145f195ee7ad36289ea0dc9cb6d8`; CI `31720409216`
passed Windows x64 and Quality / macOS ARM.
Next: select one fresh independent Milestone; do not reopen B20.
```

## Previous settled Batch

```text
Batch: B19
Status: Settled
Milestone: An App Server client can submit one bounded Context reduction and
  receive a redacted durable checkpoint summary.
Domain/Phase: Context (Phase 6 / App Server adapter)
Evidence: commit `988438dcc807dec24cc76d946dd2a753d39ac7ab`; CI `31714088503`
passed Quality / macOS ARM and Windows x64 build.
Next: select one fresh independent Milestone; do not reopen B19.
```

Previous settled result:

```text
Batch: B14
Status: Settled
Milestone: A CLI user can delegate a child Agent with an explicit bounded
  workspace_read or workspace_write capability and observe that grant in Agent
  status.
Domain/Phase: Agent Team (Phase 7)
Exit: delegate with --capability workspace_read, acknowledge the Team
  operation, then agent status reports capability_count = 1.
Evidence: commit 5fc24da; CI 31693096637.
```

## Batch closeout record

Every settled Batch gets one short record containing:

- user result and included slices;
- one or two critical checks and their outcome;
- commit, pushed branch, and CI run;
- known deferred items; and
- the next available result slot.

If any of those fields is missing, the work may be usable locally, but it is
not yet counted as released progress.

### B7 — Git worktree allocation and merge preflight

- **User result:** a user can allocate two isolated Unix Git worktrees and run
  a read-only merge preflight that reports `mergeable` or `conflict` with
  bounded relative paths.
- **Slices delivered:** bounded repository/reference validation; isolated
  `workspace allocate`; non-mutating `workspace merge-check`; CLI JSON output
  and focused integration coverage.
- **Critical checks:** `cargo test -p greentyper --test workspace_cli` (3
  passed); full local workspace format/check/test/clippy gate passed.
- **Functional commits:** `91fd2bd` and cross-platform lint fix `2388721`.
  Both were pushed to `origin/main` and `ci/main`.
- **CI evidence:** run `31667302886` passed Quality / macOS ARM and Windows x64
  build, including workspace tests, lint, release builds, transport, terminal,
  allocator, and acceptance pipelines.
- **Deferred:** automatic merge, cleanup/leases, Windows Git/reparse-point
  adapters, MCP integration, and TUI actions. These do not block B7.
- **Next slot:** B8, one isolated Skill/MCP user result.

### B8 — Project Skill through the bounded local tool path

- **User result:** a user can list a project Skill with a stable content hash,
  explicitly approve one fixed `local.echo` invocation, and repeat it without
  creating a duplicate durable Tool effect.
- **Milestone lock:**
  - **Domain/Phase:** Skills (Phase 5), project-only.
  - **Slices:** bounded manifest discovery; hash/source-safe projection;
    explicit approval; fixed capability execution; durable call reuse.
  - **Non-goals:** MCP transport/client, arbitrary scripts, background or
    periodic discovery, TUI integration, and general Skill catalog/authority.
  - **Exit:** `skill list` reports the project manifest and hash; `skill run`
    requires `--approve`, succeeds through `local.echo`, and a repeat reuses
    the durable call.
- **Critical checks:** focused `skill_cli` integration suite (3 passed);
  full local Batch Gate passed: formatter, workspace check, workspace tests,
  workspace clippy, and diff check.
- **Functional commit:** `67b36b0`, pushed to `origin/main` and `ci/main`.
- **CI evidence:** run `31670709539` passed Quality / macOS ARM and Windows
  x64 build, including workspace tests, lint, release builds, transport,
  terminal, allocator, and acceptance pipelines.
- **Deferred:** MCP transport/client, arbitrary scripts, background polling,
  TUI actions, and general Skill catalog/authority. These are later
  Milestones, not B8 defects.
- **Next slot:** a fresh feature Milestone after B9 settlement; R1 remains separate.

### B9 — App Server project Skill flow

- **User result:** a client can list project Skill metadata, reject an
  unapproved run without state creation, explicitly run one fixed `local.echo`
  Skill, and repeat it without duplicating the durable Tool effect.
- **Milestone lock:**
  - **Domain/Phase:** Skills (Phase 5), App Server surface only.
  - **Slices:** fixed startup project root; bounded `skill.list`; explicit
    approval/error boundary; executor-factory-backed `skill.run`; durable
    repeat reuse.
  - **Non-goals:** MCP transport/client, arbitrary scripts, user/built-in Skill
    catalogs, TUI integration, background or periodic discovery, and any Skill
    capability grant.
  - **Exit:** one App Server stream completes list → reject → approve/run →
    repeat/reuse with no private path leakage.
- **Critical checks:** `cargo test -p greentyper --test app_server` (32 passed)
  and `cargo test -p greentyper --test skill_cli` (3 passed); focused flow
  covers list, approval refusal, successful execution, repeat reuse, and no
  path disclosure.
- **Functional change:** App Server dispatch and executor seam added to the
  existing project Skill implementation.
- **Commit/push:** `84ae83f`, pushed to `origin/main` and `ci/main`.
- **Batch CI:** run `31673669506` for commit `84ae83f`; Quality / macOS ARM and
  Windows x64 build both passed.
- **Deferred:** MCP transport/client, arbitrary scripts, user/built-in catalogs,
  content migration, TUI actions, and background discovery.
- **Next slot:** a fresh feature Milestone after CI settlement; R1 remains separate.

### B10 — App Server child Provider recovery (settled)

- **Status:** `Settled`.
- **User result:** a client can recover an early-failed active child Agent Turn,
  with exact owner validation, retrieve the prepared output, and acknowledge it
  without repeating the external effect.
- **Milestone lock:**
  - **Domain/Phase:** Agent/Provider Recovery (Phase 1/2 runtime surface),
    App Server only.
  - **Slices:** child fixture with inherited Preset and an early blocked
    loopback Provider; owner-checked `agent.retry`; `runtime.resume` → `runtime.delivery`
    → `runtime.acknowledge`; one wrong-owner or duplicate-recovery blocker check.
  - **Non-goals:** automatic retry, TUI approval/recovery, remote App Server
    transport, multi-tool recovery, Windows/Release Gate evidence, and unrelated
    docs or roadmap cleanup.
  - **Exit:** one deterministic App Server stream reaches `resume_required`,
    then `prepared`/delivery, then `ready`; wrong owner or repeat recovery is
    rejected without a second effect.
- **Batch rule:** keep implementation and checks inside this result. Register
  all other findings in the follow-up register; settle docs, local gate, commit,
  push, and CI once at Batch end.
- **Critical checks:** exact child-recovery App Server test passed; full local
  format, locked workspace check, workspace tests, clippy, and diff check passed.
- **Commit/push:** `4931e8a`, pushed to `origin/main` and `ci/main`.
- **Batch CI:** run `31679750127` for commit `4931e8a`; Quality / macOS ARM and
  Windows x64 build both passed.
- **Deferred:** automatic retry, TUI approval/recovery, remote App Server
  transport, multi-tool recovery, and Release Gate evidence.
- **Next slot:** a fresh feature Milestone; R1 remains separate.

### B11 — Redacted Git worktree list (settled)

- **Status:** `Settled`.
- **User result:** a Unix CLI user can list every registered Git worktree with
  stable Workspace facts, commit, branch, and detached/locked/prunable state,
  without receiving an absolute path or mutating repository state.
- **Slices delivered:** bounded Git porcelain projection; redacted list model;
  `workspace list --root PATH` parser, runner, and help; one real CLI flow.
- **Critical check:** `cargo test -p greentyper --test workspace_cli --locked`
  passed (3 tests), including three-worktree listing and path redaction.
- **Commit/push:** `a684065`, pushed to `origin/main` and `ci/main`.
- **Batch CI:** run `31682587546` for commit `a684065`; Quality / macOS ARM and
  Windows x64 build both passed.
- **Deferred:** cleanup, automatic merge, dirty-state expansion, Windows Git
  adapters, TUI/App Server surfaces, and Release Gate evidence.
- **Next slot:** a fresh feature Milestone; R1 remains separate.

### B12 — Safe Git worktree removal (settled)

- **Status:** `Settled`.
- **User result:** a Unix CLI user can remove one registered, clean, non-root
  Git worktree while preserving its branch.
- **Milestone lock:**
  - **Domain/Phase:** Workspaces (Phase 7), Unix CLI only.
  - **Slices:** bounded registered-target lookup; dirty/locked/prunable/
    detached/root rejection; non-force removal with branch-preservation check.
  - **Non-goals:** force removal, branch deletion, detached/prunable cleanup,
    automatic merge, Windows Git adapters, TUI/App Server surfaces, and
    Release Gate evidence.
  - **Exit:** dirty target is rejected without mutation; clean target is
    removed, no longer listed, and its branch still resolves.
- **Critical check:** `cargo test -p greentyper --test workspace_cli --locked`
  passed (3 tests), including dirty rejection, clean removal, list absence,
  branch preservation, and redaction.
- **Batch Gate:** passed: full local format/check/test/clippy/diff/clean gate,
  commit and push to both remotes, and one CI workflow run with both required
  jobs green.
- **Commit/push:** `e80fc1d`, pushed to `origin/main` and `ci/main`.
- **Batch CI:** run `31686469234` passed Quality / macOS ARM and Windows x64
  build, including workspace tests, lint, release builds, transport, terminal,
  allocator, and acceptance pipelines.
- **Deferred:** automatic merge, detached/prunable cleanup, Windows Git
  adapters, TUI/App Server surfaces, and Release Gate evidence.

### B13 — Explicit Git branch merge (settled)

- **Status:** `Settled`.
- **User result:** a Unix CLI user can explicitly merge one clean,
  conflict-free source branch into the checked-out target branch while keeping
  the source branch and its commit intact.
- **Milestone lock:**
  - **Domain/Phase:** Workspaces (Phase 7), Unix CLI only.
  - **Slices:** clean checked-out target validation; merge-tree preflight and
    reference recheck; explicit non-fast-forward merge with source preservation.
  - **Non-goals:** automatic/background merge, conflict resolution, force
    merge, branch deletion, Windows Git adapters, TUI/App Server surfaces, and
    Release Gate evidence.
  - **Exit:** a clean merge returns a redacted merge result; a conflict or
    dirty target is rejected without changing the target branch.
- **Critical checks:** `cargo test -p greentyper --test workspace_cli --locked`
  passed (3 tests), including clean merge success, source preservation, and
  conflict rejection with unchanged target HEAD; full local workspace gate
  passed.
- **Commit/push:** `947bf7a`, pushed to `origin/main` and `ci/main`.
- **Batch CI:** run `31689627871` passed Quality / macOS ARM and Windows x64
  build, including workspace tests, lint, release builds, transport, terminal,
  allocator, and acceptance pipelines.
- **Deferred:** conflict resolution, automatic/background scheduling, branch
  deletion, Windows Git adapters, TUI/App Server surfaces, and Release Gate
  evidence.

### B14 — Explicit Agent Workspace capability (settled)

- **Status:** `Settled`.
- **User result:** a CLI user can delegate a child Agent with repeated bounded
  `--capability workspace_read|workspace_write` options, acknowledge the Team
  operation, and see the child capability count in `agent status`.
- **Slices delivered:** strict capability parsing; duplicate/unknown capability
  rejection; ProductDriver snapshot propagation; root capability superset;
  CLI delegate → acknowledge → status flow.
- **Critical checks:** full workspace check/test/clippy gate passed; parser
  capability check passed; real CLI flow reached `capability_count = 1` after
  acknowledgement.
- **Commit/push:** `5fc24da`, pushed to `origin/main` and `ci/main`.
- **Deferred:** Workspace capability execution binding, generic Tool/MCP
  capability syntax, TUI/App Server surfaces, Windows adapters, and Release
  Gate evidence.
- **Next slot:** choose one new independent user result; B14 is already counted.

### B15 — Read-only configured Model Preset listing (settled)

- **User result:** a CLI user can list the effective default and every valid
  configured Model Preset, including provider, model, dialect, policy, and
  fallback IDs, without changing Config or Ledger state.
- **Slices delivered:** read-only `config presets` command; bounded non-secret
  JSON projection; deterministic configured-preset integration flow.
- **Critical checks:** `cargo test --workspace --locked` passed; workspace
  format/check/test/clippy and `git diff --check` passed.
- **Commit/push:** `7190da4`, pushed to `origin/main` and `ci/main`.
- **Batch CI:** run `31696507296` passed Quality / macOS ARM and Windows x64,
  including workspace tests, lint, release builds, transport, terminal,
  allocator, and acceptance pipelines.
- **Deferred:** preset editing, starter updates, Provider discovery/probes,
  credential operations, TUI/App Server surfaces, fallback execution, and R1
  Release Gate evidence.
- **Next slot:** choose one fresh independent Milestone; do not reopen B15.

### B16 — Minimal Model Preset creation through CLI (settled)

- **Status:** `Settled`.
- **User result:** a CLI user can create one minimal Model Preset from an
  existing Provider Profile by supplying Preset ID, model, and supported
  dialect; dry-run previews, commit writes atomically, and reopen lists it.
- **Slices delivered:** `config model add` parser/help; ConfigRuntime-backed
  three-field draft/CAS commit; dry-run/commit/reopen/duplicate integration
  flow.
- **Critical checks:** focused `config_cli` flow passed; full workspace
  format/check/test/clippy/diff gate passed.
- **Commit/push:** `bcde3ef`, pushed to `origin/main` and `ci/main`.
- **Batch CI:** run `31700985043` passed Quality / macOS ARM and Windows x64.
- **Deferred:** defaults, starter/discovery/network/credential flows, TUI/App
  Server, fallback execution, Workspace, and R1 Release Gate evidence.
- **Next slot:** choose one fresh independent Milestone; do not reopen B16.

### B17 — Provider discovery acceptance through CLI (settled)

- **Status:** `Settled`.
- **User result:** a CLI user can refresh a configured discovery-enabled
  Provider, inspect a current catalog, accept one verified discovered model as
  a normal Model Preset, and reopen Config to see the exact tuple.
- **Milestone lock:**
  - **Domain/Phase:** Provider discovery and Config (Phase 4 / selection).
  - **Slices:** successful fingerprint-bound refresh; current catalog merge;
    discovered-model acceptance with dry-run and atomic commit; reopen and
    verify provider/model/dialect.
  - **Non-goals:** background or periodic discovery, TUI/App Server discovery,
    starter automation, Provider inference, credential UI, fallback execution,
    and Release Gate evidence.
  - **Exit:** successful refresh persists a current observation; catalog marks
    the discovered model available/acceptable; dry-run leaves Config/state
    bytes unchanged; commit writes one ordinary Preset; reopen reports the
    exact tuple. Stale/failed observations remain fail-closed.
- **Critical checks:** focused CLI/core discovery tests passed; full local
  format, locked workspace check/test/clippy, and diff checks passed.
- **Feature change:** acceptance test now proves dry-run zero-write before the
  existing atomic commit/reopen path.
- **Commit/push:** `b6c75e2`, pushed to `origin/main` and `ci/main`.
- **Batch CI:** run `31705477912` for commit `b6c75e2`; Quality / macOS ARM and
  Windows x64 build both passed.
- **Deferred:** true external-provider success fixture at process boundary,
  background discovery, TUI/App Server surfaces, starter automation,
  fallback execution, and Release Gate evidence. These do not block B17.
- **Next slot:** choose one fresh independent Milestone; do not reopen B17.
