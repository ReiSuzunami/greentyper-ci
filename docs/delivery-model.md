# Delivery Model

Status: active from 2026-08-13.

This document is the execution rule for turning the roadmap into progress. The
[implementation plan](implementation-plan.md) remains the product roadmap; this
document defines when a piece of work is allowed to count as delivered.

## Current Goal

Maximize verified user-flow coverage. Deliver one user result per Batch, keep
the scope fixed, make it usable first, add only one or two critical checks,
then settle the Batch once. Count progress only after the intended commit,
push, and CI result exist. Record non-blocking gaps for later; do not let them
expand the active Batch or reopen a completed Milestone.

## The unit of progress

Progress is counted in **verified user results**, not in changed lines, passed
unit tests, or opened pull requests.

| Term | Meaning | Is it a completion gate? |
| --- | --- | --- |
| Phase | A broad product area in the roadmap (Provider, Context, Team, and so on). | No. A Phase may contain several unrelated results. |
| Milestone | One sentence describing one user-visible result. | Yes. It has one owner, one boundary, and one exit statement. |
| Batch | Three to five vertical slices that together deliver one Milestone. | Yes. A Batch is the normal unit that is committed, pushed, and counted. |
| Slice | The smallest implementation step that moves the same user result forward. | Local checkpoint only. |
| Release Gate | The full platform, performance, packaging, and acceptance bar. | Only for a release candidate or an explicitly declared release batch. |

Examples of a Milestone:

- “A user can resume a blocked Provider Turn without repeating an external
  effect.”
- “A user can apply one guarded file change and receive a stale-read refusal
  when the file changed.”

“Improve the Provider phase” and “finish the docs” are not Milestones because
they do not describe one usable result.

## Hard batch rules

1. Lock one user result before coding. Write the result, the three-to-five
   slices, and the explicit non-goals in the batch note.
2. Keep the batch in one domain. A discovery, security, documentation, or
   architecture finding does not open a second domain.
3. Stop after the result works. Do not add a sixth slice to make the result
   aesthetically complete. Put the remaining issue in the follow-up register.
4. A finding may interrupt the batch only when it is one of these blockers:
   - the advertised flow cannot complete;
   - data, Ledger recovery, or atomicity can be corrupted;
   - an external effect can be repeated or authority can cross an Agent;
   - the changed code does not compile or its critical path test fails; or
   - the change violates the formal Performance Contract.
5. After the flow is usable, add at most one or two tests for its highest-risk
   boundary. Do not repeat correctness, security, terminal, and documentation
   reviews in parallel unless a blocker is found.
6. Update documentation once, at Batch settlement. During implementation,
   keep notes in the batch description rather than repeatedly rewriting the
   whole roadmap.
7. Do not start another feature while the active Batch is unsettled. A local
   implementation, a passing targeted test, or an unpushed commit is not
   progress yet.

## Three gates, not one endless gate

### Slice Gate

Run only the checks needed to keep the current slice moving:

- formatter/check for touched crates;
- the changed package's focused test or deterministic fixture;
- `git diff --check`.

The Slice Gate does not run the whole workspace, release packaging, FMDev, or
Target acceptance.

### Batch Gate

Run once, after all slices for the one result are usable:

1. the focused tests for every changed surface;
2. one local regression pass appropriate to the risk (full workspace only for
   cross-cutting or persistence changes);
3. one documentation update and link/format check;
4. removal of temporary files and a clean intended diff;
5. one commit and push; and
6. one CI run for that commit.

Only a successful Batch Gate increments the progress ledger. If CI fails,
repair the current batch only. An unrelated flaky or infrastructure failure is
recorded and retried once; it does not justify adding a feature or reopening
the roadmap.

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
| B7 | Allocate isolated Git worktrees and report merge/conflict outcomes. | Pending | Do not begin until B6 is settled and B7 is explicitly locked. |
| B8 | Run a useful Skill/MCP flow with isolated capabilities and recoverable results. | Pending | Discovery, transport, and authority are one separate Milestone. |
| B9 | Install, recover, measure, and pass the declared release acceptance. | Pending | Release Gate only; never used to block a small feature Batch. |

Current official progress is **six settled feature Batches**. B7 is the next
available result and must be explicitly locked before implementation starts.

## Batch closeout record

Every settled Batch gets one short record containing:

- user result and included slices;
- one or two critical checks and their outcome;
- commit, pushed branch, and CI run;
- known deferred items; and
- the next available result slot.

If any of those fields is missing, the work may be usable locally, but it is
not yet counted as released progress.
