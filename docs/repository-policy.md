# Repository Policy

## Repositories

| Repository | Visibility | Authority | Purpose |
| --- | --- | --- | --- |
| [`ReiSuzunami/greentyper`](https://github.com/ReiSuzunami/greentyper) | Private | Canonical | Development, review, decisions, issues, and release history |
| [`ReiSuzunami/greentyper-ci`](https://github.com/ReiSuzunami/greentyper-ci) | Public and temporary | None | Hosted CI and short-lived build artifacts |

The canonical commit SHA is the identity of every mirrored build. The public mirror may contain only a fast-forward prefix of canonical `main`; it must never contain a unique commit or be merged back into the canonical repository.

## Local Remotes

- `origin` points to the private canonical repository and is the default push remote.
- `ci` points to the temporary public mirror.
- Normal development pushes only to `origin`.
- A public sync is always explicit: first push the reviewed commit to `origin`, then run `CONFIRM_PUBLIC_MIRROR=1 scripts/sync-ci-public.sh`.

The sync script rejects a dirty worktree, a canonical remote that differs from local `HEAD`, a non-`main` branch, or public history that is not an ancestor of the canonical commit.

## Public Boundary

Before every public sync, review the complete outgoing commit range. Source and synthetic fixtures are allowed. Credentials, production configuration, private prompts, user content, diagnostic captures, machine identifiers, and unredacted provider traffic are forbidden.

The public mirror has no Issues, Discussions, Wiki, releases, packages, deployment environments, repository secrets, or independent branches. Contributions and security reports belong to the canonical project, never to the mirror.

## CI Ownership

During the mirror phase, GitHub-hosted CI executes only when `github.repository` is `ReiSuzunami/greentyper-ci`. The canonical private repository keeps Actions disabled, avoiding duplicate work and private-runner billing. Workflows use read-only repository permissions, pinned actions, no privileged event triggers, and no secrets.

GitHub-hosted results prove build and correctness on clean Windows and macOS hosts. They do not replace FMDev regression evidence or the asynchronous Target Machine Acceptance Run defined by the Performance Contract.

## Publication Transition

For this transition, the Release Owner is `ReiSuzunami`. The approval to create the two repositories does not authorize their later deletion or visibility changes. After feature implementation is substantially complete, `ReiSuzunami` must give a new, explicit approval for both destructive state changes: deleting the temporary public mirror and changing the canonical repository from private to public. Approval is considered only after:

1. The explicitly declared Release Gate for one candidate commit SHA passes
   (or has a Release Owner-approved waiver recorded with its evidence).
2. The complete Git history and source tree pass secret and privacy review.
3. Canonical CI is enabled and proven at the exact final mirror commit.
4. The final mirror SHA, build evidence, and artifact hashes are recorded.
5. Issues, security reporting, contribution policy, and release automation are ready in the canonical repository.

That fresh approval cannot be inferred from an earlier repository, CI, release, or automation instruction. After approval, freeze the mirror, verify both `main` refs match, move CI ownership to the canonical repository, delete the mirror, and only then make the canonical repository public. No automation in this repository may perform either visibility change or deletion.
