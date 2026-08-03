# Domain context

`jj-waltz` makes Jujutsu workspaces feel like named places a developer can create,
enter, inspect, link, and remove safely.

## Terms

- **Workspace** — a named Jujutsu working copy with its own filesystem root.
- **Default workspace** — the repository's base workspace, selected by `^` or
  `default`. Removal is never allowed.
- **Current workspace** — the workspace containing the command's current root,
  selected by `@`.
- **Previous workspace** — the last workspace visited from the current one,
  selected by `-`.
- **Workspace inventory** — one consistent view of all workspace names, roots,
  and current/default/previous roles for one command.
- **Creation workflow** — ordered policy, workspace creation, bookmark creation,
  link application, rollback, and switch-state recording.
- **Removal plan** — workspace, directory action, and associated bookmarks known
  before destructive work begins.
- **Associated bookmark** — a bookmark created by `jw` for one workspace. `jw`
  records this relationship and asks before deleting the bookmark.
- **Workspace link** — a validated symlink from a workspace-relative source to a
  shared target, configured by `.jwlinks.toml` and optional local overrides.
- **Shell adapter** — shell-specific syntax that gives the shared `jw` command
  policy native completion and parent-shell directory changes.
- **Herdr container** — a Herdr workspace or tab associated with one JJ workspace
  through a provenance marker.

## Invariants

- One command uses one workspace inventory wherever a consistent view matters.
- Creation either completes required setup or removes newly created state.
- Link sources stay inside the target workspace.
- Removal preserves bookmarks unless the user confirms deletion.
- Herdr provenance survives until Herdr close succeeds.
- Shell adapters implement the same switching behavior.
