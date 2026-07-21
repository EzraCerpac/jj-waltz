# Domain context

`jj-waltz` makes Jujutsu workspaces feel like named places a developer can create,
enter, inspect, link, and remove safely.

This glossary defines the shared vocabulary used by the CLI, JSON, Herdr, and
future workspace-lifecycle workflows. A defined term is a semantic contract, not
by itself a claim that a command exposing that concept already exists.

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
- **Repository snapshot** — the immutable repository, trunk, and workspace facts
  used by one command. It records the final JJ operation ID after any requested
  working-copy refresh. Status is derived from this snapshot; renderers never
  launch fresh JJ queries.
- **Trunk revset** — the configured JJ revset used as the comparison line,
  defaulting to `trunk()`. It must resolve to exactly one revision. It is not a
  writable bookmark, and resolving trunk never grants permission to move one.
- **Managed workspace** — a JJ workspace with repository-scoped lifecycle
  metadata recorded by `jw`, including its immutable creation base. Management
  records `jw` intent; it does not replace JJ's graph or workspace state.
- **Unmanaged workspace** — a JJ workspace without `jw` lifecycle metadata. It
  remains usable by existing workspace commands. Display code may label derived
  facts as inferred, but destructive or publication automation must not treat
  inferred intent as authoritative.
- **Creation base** — the exact commit from which `jw` created or explicitly
  adopted a managed workspace. It is historical metadata and is not rewritten
  when the workspace stack later moves.
- **Working-copy revision (WC revision)** — the literal revision associated with
  a workspace's `@`. It may be empty and therefore may differ from the revision
  containing work intended for publication.
- **Publish tip** — the revision selected for a publication plan. The default
  semantic rule is non-empty `@`, otherwise the sole parent of an empty `@`.
  Empty merge revisions, conflicts, or divergent changes make selection
  ambiguous or unsafe and must be reported rather than hidden.
- **Workspace stack** — revisions reachable from the publish tip but not the
  recorded creation base, excluding revisions already reachable from resolved
  trunk. A safe stack is connected and has one publish head; merges, shared
  revisions, conflicts, divergence, or missing bases are explicit hazards.
- **Publication anchor** — the non-trunk mechanism used to share work: an
  associated bookmark, a generated bookmark, or no bookmark yet. Bookmark
  association is `jw` metadata, not a current-branch inference.
- **Integration** — evidence describing how a publish tip relates to resolved
  trunk, such as the same revision or an ancestor of trunk. Tree equivalence and
  forge-reported merge state are weaker evidence and must not silently become
  ancestry proof.
- **Cleanup** — a separately planned retirement of workspace registration,
  directory, metadata, and any explicitly selected bookmark state. Integration
  does not imply cleanup, and directory removal does not imply bookmark removal.
- **Refresh** — allowing JJ to snapshot selected working-copy files before the
  final operation ID is captured. Refresh changes the freshness of input facts;
  it is separate from pure in-memory status derivation. When refresh is skipped,
  possibly stale rows must say so.
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

- One status or planning command derives from one final repository snapshot and
  names its JJ operation ID.
- Trunk resolves to exactly one revision; no ordinary `jw` operation moves a
  trunk bookmark.
- Refresh completes before snapshot capture. Status derivation performs no
  process execution or filesystem mutation.
- Managed metadata records only `jw` lifecycle intent. Unmanaged inference stays
  visibly inferred.
- Creation either completes required setup or removes newly created state.
- Link sources stay inside the target workspace.
- Removal preserves bookmarks unless the user confirms deletion.
- Publication and cleanup remain separate workflows.
- Core workspace lifecycle and local status remain offline; forge and network
  information are optional enrichment.
- JSON stdout contains only its versioned document; diagnostics use stderr.
- Herdr provenance survives until Herdr close succeeds.
- Shell adapters implement the same switching behavior.
