# Workspace links

Read this reference only for `.jwlinks.toml`, `.jwlinks.local.toml`, shared ignored directories, or link failures.

## Invariants

- The default workspace owns both config files, even when `jw links apply` runs elsewhere.
- `.jwlinks.local.toml` overrides a `.jwlinks.toml` entry with the same `source`.
- `source` is the link path inside the receiving workspace. It must stay inside that workspace; absolute paths and `..` are rejected.
- A relative `target` is resolved from the receiving workspace root.
- A missing target fails when `required = true`. An optional missing target is
  skipped only when the source is absent or is a symlink to that expected,
  currently dangling target.
- When the target exists, a missing source is a missing link. An existing symlink
  or ordinary path is satisfied when it resolves canonically to the expected
  target. A private directory, file, or symlink to another target is always a
  conflict, including when the configured target is optional and absent.
- Link creation is preflighted and rolls back links and directories created by the failed operation.
- `jw add` and `jw switch` apply configured links automatically. Use `--no-links` only to bypass broken or intentionally unwanted link configuration for that operation.
- `jw doctor` checks every managed workspace. A stale or missing managed path is
  reported by the workspace checks and its link checks are `SKIP`; unmanaged
  workspaces are outside this check.

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
4. Inspect the receiving `source` with `ls -ld`; compare canonical paths when
   both paths exist. Preserve a dangling symlink's configured target text so a
   correct optional omission is distinct from a conflict.
5. For doctor output, map satisfied to `PASS`, optional omission to `WARN`/`SKIP`,
   required missing links and conflicts to `FAIL`, and unreadable/stale receiving
   roots to `SKIP`.
6. Fix the underlying config, target, or conflict before rerunning `jw links apply`
   or `jw switch <name>`. Do not overwrite a conflicting source automatically.
7. Verify that the receiving source and configured target resolve to the same
   canonical path, whether the source is an ordinary path or a symlink.

Complete link work when every required link resolves to its configured target, optional omissions are explained, and no conflicting path was overwritten.
