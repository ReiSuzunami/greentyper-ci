# Delivery Model

Status: active from 2026-08-13.

This document is the execution rule for turning the roadmap into progress. The
[implementation plan](implementation-plan.md) remains the product roadmap; this
document defines when a piece of work is allowed to count as delivered.

## Current Goal

Maximize the number of distinct, user-visible workflows that reach a
CI-settled state per unit of execution time. Work on one locked Milestone at a
time. Make its smallest usable vertical flow first, add only the checks that
protect its highest-risk boundary, then settle it and move on. A roadmap Phase
is never a reason to keep polishing the current result or to block the next
result. Local code, a passing focused test, a commit, or an in-progress CI run
is not counted progress; only a successful Batch Gate is counted.

## The unit of progress

Progress is counted in **verified user results**, not in changed lines, passed
unit tests, or opened pull requests.

| Term | Meaning | Is it a completion gate? |
| --- | --- | --- |
| Phase | A broad roadmap grouping (Provider, Context, Team, and so on). It is a queue of possible work, not an execution task. | No. A Phase never needs to be “finished” before another result is shipped. |
| Milestone | One sentence describing one user-visible result in one domain. It has one owner, one boundary, and one exit statement. | Yes. This is the only feature-level completion unit. |
| Batch | The implementation package for exactly one Milestone: one-to-five slices (usually two-to-four), with explicit non-goals. | Yes. A Batch is committed, pushed, and counted once. |
| Slice | The smallest implementation step that moves the same user result forward. | Local checkpoint only. |
| Release Gate | The full platform, performance, packaging, and acceptance bar. | Only for a release candidate or an explicitly declared release batch. |

Examples of a Milestone:

- “A user can resume a blocked Provider Turn without repeating an external
  effect.”
- “A user can apply one guarded file change and receive a stale-read refusal
  when the file changed.”

“Improve the Provider phase” and “finish the docs” are not Milestones because
they do not describe one usable result.

### Phase status

Phases are roadmap containers, not work items. A Phase may contain many
Milestones and may remain open while later Phases ship results. “Phase complete”
is a separate, explicit roadmap review against that Phase's exit criteria; it is
never implied by a feature Batch and never runs inside the ordinary feature
loop. A pending Phase exit does not block the next locked Milestone unless the
next Milestone depends on a missing Phase invariant.

### Milestone lock

Before implementation starts, write this five-line lock in the batch note:

```text
Milestone: <one user result>
Domain/Phase: <one roadmap area>
Slices: <one to five concrete implementation slices>
Non-goals: <everything explicitly deferred>
Exit: <the one observable result that makes the milestone usable>
```

The Phase can stay open indefinitely. The Milestone cannot: once its exit is
met, settle the Batch and move to the next Milestone. A new idea belongs in a
later slot unless it blocks the locked exit.

## Hard batch rules

1. Lock one user result before coding. Write the five-line Milestone lock and
   do not change it mid-Batch.
2. Keep the batch in one domain. A discovery, security, documentation, or
   architecture finding does not open a second domain.
3. Stop when the locked result works. Never add a sixth slice or a second user
   result to make the Batch feel complete; put it in the follow-up register.
4. A finding may interrupt the batch only when it is one of these blockers:
   - the advertised flow cannot complete;
   - data, Ledger recovery, or atomicity can be corrupted;
   - an external effect can be repeated or authority can cross an Agent;
   - the changed code does not compile or its critical path test fails; or
   - the change violates the formal Performance Contract.
5. After the flow is usable, add at most one or two blocker-critical checks.
   Use one owner and, at most, one independent review. Do not repeat
   correctness, security, terminal, and documentation reviews in parallel
   unless a blocker is found.
6. Update product/status documentation once, at Batch settlement. During
   implementation, keep discoveries in the follow-up register.
7. Do not start another feature while the active Batch is unsettled. A local
   implementation, a passing targeted test, or an unpushed commit is not
   progress yet.
8. Switch Phase only after the current Batch is settled. Phase switching is
   not a way to leave an unfinished Batch behind.

## Three gates, not one endless gate

### Slice Gate

Run only the checks needed to keep the current slice moving:

- formatter/check for touched crates;
- the changed package's focused test or deterministic fixture;
- `git diff --check`.

The Slice Gate never runs the whole workspace, release packaging, FMDev, or
Target acceptance. It exists to keep implementation moving, not to declare
progress.

### Batch Gate

Run once, after all slices for the one result are usable:

1. the focused checks for every changed surface;
2. one local regression pass appropriate to the risk. A full workspace pass is
   required only for cross-cutting, persistence, or release-sensitive changes;
3. one documentation update and link/format check;
4. removal of temporary files and a clean intended diff;
5. one commit and push; and
6. one workflow run for that exact commit SHA, with every required job green.

Only a successful Batch Gate increments the progress ledger. If CI fails,
repair the current Batch only. Classify the failure before changing code:

- **Code failure:** one repair commit and one replacement workflow run are
  allowed. Do not add a new slice or change the Milestone.
- **Infrastructure/flaky failure:** one rerun of the same commit is allowed.

If the replacement run (or the one infrastructure rerun) fails again, mark the
Batch **Blocked**, record the failing job and reason, and stop. Do not create
more commits merely to chase CI. A Blocked Batch counts zero until explicitly
resumed or waived by the release owner.

### Release Gate

The full Windows/macOS/Linux matrix, crash/fuzz/security coverage, FMDev
30-run evidence, Target evidence, migration coverage, and signed portable
packaging belong here. They run at a declared release candidate or release
batch, not after every small user result. CI smoke samples are correctness
evidence; they are not performance approval.

A Release Gate declaration must name one candidate commit SHA. That SHA is
the unit of release evidence; rerun only when the candidate changes or a
recorded gate failure is repaired. R1 being pending does not block ordinary
feature Milestones. Release Gate evidence cannot be substituted by a feature
Batch's CI smoke run.

## Stop and defer protocol

When a non-blocking issue appears:

1. record it with file/area, impact, and the earliest Milestone it belongs to;
2. state whether it affects the current user result;
3. leave the current implementation unchanged unless it is a blocker; and
4. continue to Batch settlement.

The follow-up register is allowed to contain incomplete documentation,
additional platform evidence, richer UI, performance measurement, and future
authority surfaces. They are not silently folded into the active Batch.

Background work, periodic polling, and resident discovery remain prohibited by
the formal [Performance Contract](performance-contract.md) unless a separately
approved exception changes that contract first.

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

Current official progress is **nine CI-settled feature Batches**. The next slot
is a new feature Milestone selected by a fresh lock; R1 remains a separate
Release Gate and does not block feature coverage.

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
- **Commit/push:** `84ae83f`, pushed to `origin/main` and `ci/main`.
- **Batch CI:** run `31673669506` for commit `84ae83f`; Quality / macOS ARM and
  Windows x64 build both passed.
- **Deferred:** MCP transport/client, arbitrary scripts, user/built-in catalogs,
  content migration, TUI actions, and background discovery.
- **Next slot:** a fresh feature Milestone after CI settlement; R1 remains separate.
