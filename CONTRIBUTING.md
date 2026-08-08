# Contributing

GreenTyper is in private design and implementation. External contributions are not accepted yet.

`ReiSuzunami/greentyper` is the canonical repository. `ReiSuzunami/greentyper-ci` is a temporary public build mirror: do not open pull requests, issues, or security reports there, and never develop directly against its branches.

Canonical changes should remain narrowly scoped, preserve the documented architecture, and include evidence proportional to risk. Before review, run:

```text
cargo fmt --all -- --check
cargo check --workspace --all-targets --locked
cargo test --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
```

Commit messages use Conventional Commits. Pull requests must explain why the change is needed, identify affected contracts, list verification performed, and call out performance, migration, security, and recovery risks.
