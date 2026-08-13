# Delivery Model

Status: active from 2026-08-13.

This document is the execution rule for turning the roadmap into progress. The
[implementation plan](implementation-plan.md) remains the product roadmap; this
document defines when a piece of work is allowed to count as delivered.

## Current Goal

Maximize **verified usable user-flow coverage per unit of time**. Ship the next
small result first; tidy, document, and certify once at the Batch boundary.
Do not pursue “complete the roadmap”, “complete a Phase”, or “make every edge
case uncontroversial” inside one task. Those are separate planning or Release
Gate activities.

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

Use this four-line note after each Slice; do not turn it into a review meeting:

```text
Slice: <what a user can do now>
Breadth: <new usable domain/flow, or none>
Depth: <local / recoverable / externally verified>
Next: <the smallest remaining step for the same Milestone>
```

The note may be approximate. It exists to show feature coverage moving, not to
manufacture a precise percentage. Official progress is updated only once at
Batch settlement.

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

Every Phase is labelled `Planned`, `Active`, `Deferred`, or `Closed`.
`Active` means only that it may supply the next Milestone; it does not require
all listed roadmap items to be implemented. `Closed` is recorded only at a
dedicated Phase review with its own exit evidence. Phase review and Release
Gate work never enter a normal feature Batch.

### Milestone lock

Before implementation starts, write this five-line lock in the batch note:

```text
Milestone: <one user result>
Domain/Phase: <one roadmap area>
Slices: <one to five concrete implementation slices>
Non-goals: <everything explicitly deferred>
Exit: <the one observable result that makes the milestone usable>
```

Also record `Status: Locked | Building | Ready | Settling | Settled | Blocked`.
`Ready`
means the user result works and its blocker checks pass; it is not progress
the Batch; changing the user result starts a new Milestone.

The Phase can stay open indefinitely. The Milestone cannot: once its exit is
met, settle the Batch and move to the next Milestone. A new idea belongs in a
later slot unless it blocks the locked exit.

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
8. Switch Phase only after the current Batch is settled. Phase switching is
   not a way to leave an unfinished Batch behind.
9. A Phase may remain Active forever while Milestones settle. Do not wait for a
   Phase exit review before selecting an independent result from another Phase.
10. A Milestone lock cannot grow. If new work creates a second user result,
    split it into a later Milestone; if it is only polish or evidence, defer it.

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

A Release Gate declaration must name one candidate commit SHA. That SHA is
the unit of release evidence; run it once for that candidate and rerun only
when the candidate changes or a recorded gate failure is repaired. R1 being
pending does not block ordinary feature Milestones. Release Gate evidence
cannot be substituted by a feature Batch's CI smoke run.

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
project Skills, Workspace, and local App Server each have at least one usable
flow. MCP transport and complete Packaging/Acceptance do not. This is **7 of 9
roadmap domains with a usable base flow**, not 78% release readiness.

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
| B18 | Remove one clean registered Git worktree and optionally delete its already-merged branch through Unix CLI. | Locked | Not counted until Batch settlement; force deletion and automatic cleanup deferred. |

Current official progress is **seventeen CI-settled feature Batches**. R1 remains
a separate Release Gate and does not block feature coverage.

## Active milestone

```text
Batch: B18
Status: Locked
Milestone: A Unix CLI user can remove one clean registered Git worktree and,
  only when explicitly requested, delete its already-merged branch.
Domain/Phase: Workspaces (Phase 7 / Unix CLI)
Slices: add explicit branch-delete option; perform safe worktree removal then
  non-force merged-branch deletion; return redacted result and preserve branch
  on refusal; add one dirty/unmerged fail-closed check.
Non-goals: force deletion, current/root branch deletion, detached/prunable
  cleanup, automatic/background cleanup, merge or conflict resolution, Windows
  Git adapters, TUI/App Server surfaces, and Release Gate evidence.
Exit: clean registered worktree removal succeeds; branch remains by default and
  is deleted only with the explicit option after Git confirms safe deletion;
  dirty, locked, root, current, unmerged, detached, and prunable targets fail
  without mutation.
Coverage target: one new Workspace mutation flow; local success plus one
  fail-closed boundary. Stop when this exit is observable.
```

## Last settled milestone

```text
Batch: B16
Status: Settled
Milestone: A CLI user can create one minimal Model Preset from an existing
  Provider Profile by supplying a bounded Preset ID, model ID, and supported
  dialect, with dry-run or atomic commit output.
Domain/Phase: Config and Provider selection (Phase 1/4)
Slices: add `config model add`; reuse ConfigRuntime validation/CAS commit;
  verify dry-run, commit, and reopen through `config presets`.
Non-goals: default selection, starter installation/update, discovery or network
  probes, credentials, TUI/App Server surfaces, fallback execution, Workspace,
  and Release Gate evidence.
Exit: `config model add` previews without writing, commits a valid tuple, and
  reopened `config presets` reports the new preset; invalid/duplicate input
  fails closed without a Config or Ledger side effect.
Evidence: commit `bcde3ef`; CI `31700985043` passed macOS ARM and Windows x64.
Critical checks: `cargo test --workspace --locked`; workspace format/check/test/
  clippy/diff gate; focused model-add flow with dry-run, commit, reopen, and
  duplicate rejection.
Deferred: default selection, starter installation/update, discovery/network
  probes, credentials, TUI/App Server surfaces, fallback execution, Workspace,
  and Release Gate evidence.
```

## Previous settled milestone

```text
Batch: B15
Status: Settled
Milestone: A CLI user can list configured Model Presets, see the effective
  default, and inspect each preset's provider, model, dialect, policy, and
  fallback chain without mutating state.
Domain/Phase: Config and Provider selection (Phase 1/4)
Slices: add a read-only config presets command; serialize bounded non-secret
  preset projections; exercise one configured-preset listing flow.
Non-goals: preset editing, starter installation/update, Provider discovery or
  network probes, credential operations, TUI/App Server surfaces, fallback
  execution, and Release Gate evidence.
Exit: `greentyper config presets` returns deterministic JSON containing the
  effective default and every valid configured preset, including fallback IDs,
  with unchanged Config and Ledger state.
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
