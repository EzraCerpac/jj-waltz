# Workspace lifecycle

Read this reference for semantic inspection, diagnosis, adoption, removal, or pruning.

Use the relevant `jw <command> --help` before relying on flags.

## Inspect and diagnose

- `jw status [workspace]` explains one workspace; it defaults to `@`. Use it before lifecycle mutations.
- `jw doctor` diagnoses repository, trunk, metadata, and workspace consistency without repairing them. `jw doctor --format=json` still emits a diagnostic report when configuration loading fails.

Complete diagnosis when the target workspace and reported hazards are identified without mutating state.

## Adopt

`jw adopt <name> --base <revset>` records an existing JJ workspace as managed. It records lifecycle metadata; it does not move revisions, move or create bookmarks, or refresh the working copy.

Inspect the workspace and resolve the base before adoption. Use `--bookmark` only to record an existing association. Use `--no-bookmark` when no association should be recorded, including when ignoring a stale legacy marker.

Complete adoption when `jw status <name>` reports the intended managed metadata and JJ revision/bookmark state is unchanged.

## Remove

Run removal only for an explicitly requested cleanup. Before acting, inspect `jw list`, `jw current`, `jw status <name>`, `jw path <name>`, JJ status from the target path, and associated bookmarks.

- `jw remove <name>` forgets the workspace and deletes its directory by default.
- `--keep-dir` forgets it while preserving the directory; prefer this when file preservation is uncertain.
- Removing the default workspace is refused.
- Deleting the current workspace directory is refused until the user switches away. `--keep-dir` can still forget the current workspace.
- Associated bookmarks prompt by default. Use `--keep-bookmark` or `--delete-bookmark` to make the user's choice explicit for non-interactive work.

Complete removal when `jw list` no longer contains the workspace and the directory and bookmarks match the chosen preservation policy.

## Prune

`jw prune` forgets workspaces whose paths are already missing. It does not share `remove`'s explicit default-workspace guard. Inspect `jw list` and `jj workspace list` first, and run it only when every missing entry is intended for forgetting.

Complete pruning when only the expected missing entries disappeared and the default/current workspaces remain intact.
