# Architecture

`jj-waltz` is organized into four runtime modules and one binary entrypoint.

- `src/main.rs`: minimal process entrypoint; delegates to `cli::run`.
- `src/cli.rs`: argument parsing, output shaping (text/JSON), and command routing.
- `src/workspace.rs`: JJ workspace semantics (switching, listing, path resolution, pruning).
- `src/links.rs`: `.jwlinks.toml` parsing and symlink reconciliation logic.
- `src/shell.rs`: shell init scripts and completion output.

## Design boundaries

- `cli` is the IO boundary and should stay light on domain logic.
- `workspace` and `links` should expose deterministic functions suitable for direct tests.
- Shell-specific behavior should remain isolated in `shell` to avoid platform branching in command flow.

## Test strategy

- Unit tests: validation and pure helpers in `workspace` / `links`.
- Integration tests: end-to-end command behavior in `tests/cli.rs`.
- Stress tests: large link rule sets in `tests/links_stress.rs`.
