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
  metadata recorded by `jw`, including its recorded creation base. Management
  records `jw` intent; it does not replace JJ's graph or workspace state.
- **Unmanaged workspace** — a JJ workspace without `jw` lifecycle metadata. It
  remains usable by existing workspace commands. Display code may label derived
  facts as inferred, but destructive or publication automation must not treat
  inferred intent as authoritative.
- **Creation base** — the exact commit from which `jw` created or explicitly
  adopted a managed workspace. It is historical metadata; ordinary lifecycle
  operations do not rewrite it. Explicit metadata repair may replace the recorded
  value after validating a new exact base.
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
- **Associated bookmark** — a local bookmark recorded by `jw` for one workspace.
  `jw` records this relationship and asks before deleting the bookmark.
- **Metadata repair** — an explicit correction of an existing managed record's
  creation base and bookmark association. It does not change JJ workspace, commit,
  bookmark, or working-copy state.
- **Workspace link** — a configured source-target relationship owned by the
  default workspace's `.jwlinks.toml` and optional `.jwlinks.local.toml`.
  `source` is inside each receiving workspace; a relative `target` is resolved
  from that receiver. A link is satisfied by a symlink or ordinary path that
  resolves canonically to the target.
- **Workspace-link health** — the doctor result for one configured link in one
  managed workspace: satisfied, missing, skipped, or conflicting. An optional
  missing target is skipped only when the source is absent or is the correct
  dangling link; an occupied source is conflicting.
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
- `jw adopt` is insert-only. `jw repair` requires an existing readable managed
  record and a matching JJ workspace registration at one frozen operation; a usable
  checkout path is not required.
- Metadata repair changes only the recorded creation base and bookmark association.
  Historical fields survive, and a failed or concurrent write leaves the old record
  intact.
- Creation either completes required setup or removes newly created state.
- `jw doctor` checks configured links in every managed workspace. It reports
  missing or conflicting links instead of treating the repository as healthy;
  stale or missing workspace paths are reported separately and link inspection
  is skipped for those paths.
- Link sources stay inside the target workspace.
- Link configuration comes from the default workspace; local entries override
  shared entries with the same source. Relative targets are resolved per
  receiving workspace.
- Removal preserves bookmarks unless the user confirms deletion.
- Publication and cleanup remain separate workflows.
- Core workspace lifecycle and local status remain offline; forge and network
  information are optional enrichment.
- JSON stdout contains only its versioned document; diagnostics use stderr.
- Herdr provenance survives until Herdr close succeeds.
- Shell adapters implement the same switching behavior.

## Milestone-zero command contract

- `jw list`, `jw l`, and `jw ls` keep the historical human output and fast path.
  `--format=json` opts into a schema-versioned repository snapshot; its default
  refresh is `current`, with `none` and `all` available explicitly.
- `jw status [workspace]` selects `@` by default and derives human or JSON output
  from one snapshot. `--refresh=none` does not reconcile a working copy and marks
  its state unknown unless a hazard such as staleness is known.
- `jw doctor` uses a separate versioned report. Every check renders even when
  another fails, and an unhealthy report exits nonzero only after stdout is
  complete.
- Doctor human output uses `PASS`, `WARN`, `FAIL`, and `SKIP` for link health.
  Optional omissions are warnings; missing required targets and conflicts are
  failures. The `workspace-link` diagnostic code is additive within doctor
  schema version 1. `jw status` remains a one-workspace snapshot and does not
  include link health.
- `jw adopt NAME --base REVSET [--bookmark BOOKMARK]` adds managed metadata for
  an existing usable workspace. It never moves revisions or bookmarks and never
  refreshes a working copy. Creation-base and current-revision facts are reported;
  stack analysis remains milestone-one work.
- `jw repair NAME --base REVSET (--bookmark BOOKMARK | --no-bookmark)` updates an
  existing readable record only. `NAME` is literal, the base resolves to exactly
  one revision, and a requested bookmark must already exist locally. It validates
  the workspace and inputs at one frozen operation, preserves historical fields,
  and performs no JJ or working-copy mutation. It has no JSON or `jw status` output
  contract beyond the ordinary human command result.
- New workspace creation resolves an exact base before mutation. Implicit creation
  requires exactly one `parents(@)` revision. A merge working copy therefore needs
  an explicit exact base such as `--at @`.
