#!/usr/bin/env bash

set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

if [[ "${CONFIRM_PUBLIC_MIRROR:-}" != "1" ]]; then
    echo "Refusing public sync: set CONFIRM_PUBLIC_MIRROR=1 after reviewing the outgoing history." >&2
    exit 1
fi

if [[ "$(git branch --show-current)" != "main" ]]; then
    echo "Refusing public sync: current branch must be main." >&2
    exit 1
fi

if [[ -n "$(git status --porcelain)" ]]; then
    echo "Refusing public sync: worktree must be clean." >&2
    exit 1
fi

expected_origin_url="https://github.com/ReiSuzunami/greentyper.git"
expected_ci_url="https://github.com/ReiSuzunami/greentyper-ci.git"
for remote_spec in "origin:$expected_origin_url" "ci:$expected_ci_url"; do
    remote="${remote_spec%%:*}"
    expected_url="${remote_spec#*:}"
    if ! git remote get-url "$remote" >/dev/null 2>&1; then
        echo "Refusing public sync: missing $remote remote." >&2
        exit 1
    fi

    fetch_url="$(git remote get-url "$remote")"
    push_url="$(git remote get-url --push "$remote")"
    if [[ "$fetch_url" != "$expected_url" || "$push_url" != "$expected_url" ]]; then
        echo "Refusing public sync: $remote fetch and push URLs must both equal $expected_url." >&2
        exit 1
    fi
done

local_head="$(git rev-parse HEAD)"
origin_head="$(git ls-remote origin refs/heads/main | awk '{print $1}')"
if [[ -z "$origin_head" || "$origin_head" != "$local_head" ]]; then
    echo "Refusing public sync: origin/main must equal local HEAD ($local_head)." >&2
    exit 1
fi

ci_head="$(git ls-remote ci refs/heads/main | awk '{print $1}')"
if [[ -n "$ci_head" ]]; then
    git fetch --quiet ci main
    if ! git merge-base --is-ancestor "$ci_head" "$local_head"; then
        echo "Refusing public sync: ci/main contains history outside canonical main." >&2
        exit 1
    fi
fi

echo "Publishing canonical commit $local_head to the temporary public CI mirror."
git push ci HEAD:main

published_head="$(git ls-remote ci refs/heads/main | awk '{print $1}')"
if [[ "$published_head" != "$local_head" ]]; then
    echo "Public sync verification failed: ci/main is $published_head." >&2
    exit 1
fi

echo "Verified ci/main at $published_head."
