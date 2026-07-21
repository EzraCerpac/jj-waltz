# Contributing

Thanks for contributing to `jj-waltz`.

## Development

```bash
cargo fmt --all --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test

cargo fmt --manifest-path plugins/herdr/Cargo.toml --all --check
cargo clippy --locked --manifest-path plugins/herdr/Cargo.toml --all-targets --all-features -- -D warnings
cargo test --locked --manifest-path plugins/herdr/Cargo.toml
```

The Herdr plugin is a separate Cargo workspace, so root Cargo commands do not
check it. CI runs root integration tests against pinned JJ 0.39.0 and 0.43.0;
update those explicit compatibility pins instead of following `latest`.

## Scope

Please prefer small, focused changes with tests where practical.

Keep product ownership clear:

- `jw` coordinates workspace-lifecycle jobs; ordinary revision manipulation stays
  in `jj`.
- Pull requests, merge requests, and CI belong to `gh`, `glab`, or another thin
  forge adapter.
- Local workspace management must work offline.
- A documented roadmap concept is not a shipped command. Update `README.md` only
  when `jw --help` and tests support the claim.

## Design principles

- Keep terminal, JSON, Herdr, and future views over one domain model.
- Capture one final JJ operation ID, then derive status without subprocesses.
- Put JJ process execution behind the central adapter; never assemble avoidable
  shell command strings.
- Keep static configuration separate from repository-scoped lifecycle metadata.
- Preserve JSON schema v1: optional additions are compatible; removals, renames,
  required-field changes, or changed enum meanings require a version bump.
- Keep shell integration native and errors actionable.

## JJ compatibility

JJ 0.39.0 is the minimum supported release. Root CI also tests pinned JJ 0.43.0
as the newer compatibility target. Do not follow a floating `latest` in tests.
Raise the minimum only for a documented public capability, and keep JJ output
parsing and version differences inside the adapter.
