# Delivery Model

Status: active from 2026-08-13.

This document is the execution rule for turning the roadmap into progress. The
[implementation plan](implementation-plan.md) remains the product roadmap; this
document defines when a piece of work is allowed to count as delivered.

## Current Goal

Maximize verified user-flow coverage per unit of execution time. Work on one
locked Milestone at a time, make its user result usable first, add only the one
or two checks that protect its highest-risk boundary, and settle it once.
Count progress only after that Batch's commit, push, and CI result exist.
Everything else is a follow-up item, not a reason to widen the active work.

## The unit of progress

Progress is counted in **verified user results**, not in changed lines, passed
unit tests, or opened pull requests.

| Term | Meaning | Is it a completion gate? |
| --- | --- | --- |
| Phase | A broad roadmap grouping (Provider, Context, Team, and so on). It is a queue of possible work, not an execution task. | No. A Phase never needs to be “finished” before another result is shipped. |
| Milestone | One sentence describing one user-visible result in one domain. It has one owner, one boundary, and one exit statement. | Yes. This is the only feature-level completion unit. |
| Batch | The implementation package for exactly one Milestone: at most three-to-five slices, with explicit non-goals. | Yes. A Batch is committed, pushed, and counted once. |
| Slice | The smallest implementation step that moves the same user result forward. | Local checkpoint only. |
| Release Gate | The full platform, performance, packaging, and acceptance bar. | Only for a release candidate or an explicitly declared release batch. |

Examples of a Milestone:

- “A user can resume a blocked Provider Turn without repeating an external
  effect.”
- “A user can apply one guarded file change and receive a stale-read refusal
  when the file changed.”

“Improve the Provider phase” and “finish the docs” are not Milestones because
they do not describe one usable result.

### Milestone lock

Before implementation starts, write this five-line lock in the batch note:

```text
Milestone: <one user result>
Domain/Phase: <one roadmap area>
Slices: <three to five concrete implementation slices>
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
   Do not repeat correctness, security, terminal, and documentation reviews in
   parallel unless a blocker is found.
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
6. one CI run for that commit.

Only a successful Batch Gate increments the progress ledger. If CI fails,
repair the current Batch only. An unrelated flaky or infrastructure failure is
recorded and retried once; it does not justify adding a feature, reopening a
settled Milestone, or starting another Phase.

### Release Gate

The full Windows/macOS/Linux matrix, crash/fuzz/security coverage, FMDev
30-run evidence, Target evidence, migration coverage, and signed portable
packaging belong here. They run at a declared release candidate or release
batch, not after every small user result. CI smoke samples are correctness
evidence; they are not performance approval.

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
| B8 | List a project Skill by stable content hash and run it once through the existing approval/capability-bounded `local.echo` path, with durable reuse on repeat. | Active | Three-to-five slices: bounded manifest discovery; hash/source-safe projection; explicit approval; fixed capability execution; durable call reuse. Deferred: MCP transport/client, arbitrary scripts, background polling, TUI, and general Skill catalog/authority. |
| B9 | Install, recover, measure, and pass the declared release acceptance. | Pending | Release Gate only; never used to block a small feature Batch. |

Current official progress is **seven CI-settled feature Batches**. B8 is the
only active Batch and is not counted until its Batch Gate succeeds. No B9 work
starts before B8 is settled.

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
