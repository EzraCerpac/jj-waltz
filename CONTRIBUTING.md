# Contributing

Thanks for contributing to `jj-waltz`.

## Local quick check

```bash
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test -q
```

## Test matrix to consider

- Linux/macOS/Windows shell behavior (`bash`, `zsh`, `fish`, `powershell`).
- `jj` installed vs unavailable (`doctor`, error messages, command exits).
- Link workflows (`.jwlinks.toml` + optional `.jwlinks.local.toml`).

## Scope and design principles

- Prefer small, focused changes with tests where practical.
- Keep `cli` orchestration-focused and push behavior into domain modules.
- Preserve jj-first behavior and actionable error messages.
