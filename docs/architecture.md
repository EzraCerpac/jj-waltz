# Architecture

`jj-waltz` keeps command syntax at adapters and puts behavior behind deep module
interfaces. Callers should not reproduce Jujutsu command grammar, lifecycle order,
link validation, or shell switching policy.

## Module map

- `cli` is the terminal adapter. It parses flags, asks questions, and renders
  outcomes.
- `lifecycle` owns creation policy and ordered add/switch workflows. It loads user
  config once, applies links, records successful switches, and rolls back failed
  creations.
- `workspace` owns Jujutsu workspace discovery and mutation. Its inventory gives
  commands one consistent view; removal is planned before execution.
- `links` owns config merging, path confinement, preflight, and filesystem changes.
- `shell` owns shared shell behavior. Each shell adapter varies syntax, not policy.
- `plugins/herdr` is a separate Cargo workspace and a UI adapter over `jw`. Its
  removal workflow closes Herdr before deleting provenance.

```text
CLI adapter ─────┐
                 ├── lifecycle ── workspace inventory/mutation ── jj
Herdr adapter ───┘       │
                         └── link planning/application ────────── filesystem

Clap command model ── shell policy ── Fish/Zsh/Bash/Elvish/PowerShell adapters
```

## Failure order

Creation preflights what it can, creates the workspace, applies required setup,
then records switching state. Failure removes any workspace, bookmark, links, and
directories created by that operation; cleanup failures are attached to the
original error.

Removal builds a plan before mutation. CLI callers choose whether associated
bookmarks should be deleted. Herdr removal keeps its provenance marker until the
container close reports success; a stale marker is safer than lost recovery data.

## Test surfaces

```bash
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --locked

cd plugins/herdr
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --locked
```

CI runs root integration tests against the supported minimum JJ version and the
newer pinned compatibility version. Herdr is checked separately because it is a
separate Cargo workspace.
