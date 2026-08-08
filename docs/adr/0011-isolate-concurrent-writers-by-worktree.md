# Isolate concurrent Writers by worktree

Multiple read-only Agents may inspect one worktree, but each writable worktree has one exclusive Workspace Lease and parallel Writers use separate worktrees. Every Writer revalidates its Read Set before mutation, accepting worktree and merge overhead to prevent stale overwrites and make conflicting changes explicit.
