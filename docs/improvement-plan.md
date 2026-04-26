# Improvement plan

## 1) Improve reliability around shelling out to `jj`
- Centralize command execution with a small adapter that captures command, cwd, exit status, stderr, and typed error codes.
- Add a retry/backoff policy for transient failures (locked working copy, interrupted process) and keep hard failures fast.
- Add unit tests that assert user-facing error text for common failure paths.

## 2) Expand compatibility and correctness coverage
- Add cross-platform tests for symlink behavior in `links` (Windows junction/file-link edge cases).
- Add property-style tests for workspace token resolution (`@`, `-`, `^`) and name validation.
- Add tests for nested subdirectory preservation during `switch --print-path` and `switch -x ...` workflows.

## 3) Strengthen CI and release confidence
- Keep lint/test/build split, and add one smoke job that runs CLI end-to-end on each OS matrix target.
- Cache/download `jj` per platform version and pin with checksum verification.
- Add optional coverage reporting for `src/workspace.rs` and `src/links.rs` hot paths.

## 4) Reduce maintenance load in completions
- Generate baseline completions from clap and apply small post-processing patches only where custom behavior is required.
- Add snapshot tests so changes to command flags/subcommands fail loudly when completions drift.

## 5) Improve user-facing ergonomics
- Add `jw doctor` to validate shell init setup, detect missing `jj`, and confirm workspace root assumptions.
- Add optional structured output (`--json`) for `list`, `path`, and `current` to support scripts safely.
- Document failure recovery recipes (broken symlink, removed workspace dir, stale previous-workspace pointer).

## 6) Documentation and onboarding
- Add a short architecture page explaining module boundaries (`cli`, `workspace`, `links`, `shell`).
- Add a contributor test matrix and local quick-check command in `CONTRIBUTING.md`.
- Add one “advanced workflows” section (agents/editors with `-x`) in `README.md`.
