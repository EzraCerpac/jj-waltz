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

## Design principles

- jj-first behavior
- shell integration that feels native
- clear, actionable error messages
