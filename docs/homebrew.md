# Homebrew

`jj-waltz` is configured for Homebrew publishing through `cargo-dist`.

## Planned tap

- tap: `EzraCerpac/homebrew-tap`
- formula: `jj-waltz`
- binary: `jw`

## Release model

1. Push a version tag like `v0.1.0`
2. GitHub Actions builds release artifacts
3. `cargo-dist` publishes archives and installer metadata
4. Homebrew formula updates are published to the configured tap

## Notes

- keep the project metadata in `Cargo.toml` accurate
- ensure the GitHub repository and Homebrew tap names match the published locations
- after the first public release, test with `brew install EzraCerpac/homebrew-tap/jj-waltz`
