# Workspace links

Read this reference only for `.jwlinks.toml`, `.jwlinks.local.toml`, shared ignored directories, or link failures.

## Invariants

- The default workspace owns both config files, even when `jw links apply` runs elsewhere.
- `.jwlinks.local.toml` overrides a `.jwlinks.toml` entry with the same `source`.
- `source` is the link path inside the receiving workspace. It must stay inside that workspace; absolute paths and `..` are rejected.
- A relative `target` is resolved from the receiving workspace root.
- A missing target fails when `required = true`; an optional missing target is skipped.
- An existing symlink or ordinary path is satisfied when it resolves to the expected target. Only a path that resolves elsewhere is a conflict.
- Link creation is preflighted and rolls back links and directories created by the failed operation.
- `jw add` and `jw switch` apply configured links automatically. Use `--no-links` only to bypass broken or intentionally unwanted link configuration for that operation.

Example:

```toml
[[link]]
source = "data"
target = "../repo/data"
required = true
```

## Diagnose and verify

1. Resolve the default workspace and inspect both config files there.
2. Resolve the receiving workspace root and the target path from that root.
3. Check whether the target exists.
4. Inspect the receiving `source` with `ls -ld`; when both paths exist, compare their canonical paths so an ordinary path and a symlink can both be classified as absent, satisfied, or conflicting.
5. Fix the underlying config, target, or conflict before rerunning `jw links apply` or `jw switch <name>`.
6. Verify that the receiving source and configured target resolve to the same canonical path, whether the source is an ordinary path or a symlink.

Complete link work when every required link resolves to its configured target, optional omissions are explained, and no conflicting path was overwritten.
