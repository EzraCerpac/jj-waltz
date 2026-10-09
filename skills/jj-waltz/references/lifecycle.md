# Workspace lifecycle

Read this reference for semantic inspection, diagnosis, adoption, removal, or pruning.

Use the relevant `jw <command> --help` before relying on flags.

## Inspect and diagnose

- `jw status [workspace] --refresh none` explains one workspace without refreshing a working copy; it defaults to `@`. Use this form for read-only inspection before lifecycle mutations.
- `jw doctor` diagnoses repository, trunk, metadata, workspace, and configured-link
  consistency without repairing them. It checks links from the default workspace's
  `.jwlinks.toml` plus `.jwlinks.local.toml` in every managed workspace. Relative
  targets are resolved from each receiving workspace; unmanaged workspaces are not
  link-health subjects. Read [links](links.md) for the classifier and remedies.
- Doctor human link results are `PASS`, `WARN`, `FAIL`, or `SKIP`: optional absent
  targets are `WARN`/`SKIP`; missing sources with available targets, required missing
  targets, and occupied sources are `FAIL`; stale or unreadable managed paths are
  `SKIP` for link inspection. `jw doctor --format=json` keeps schema version 1
  and adds the `workspace-link` diagnostic
  code; a complete report is emitted before a failing exit status.
- `jw status` remains a one-workspace snapshot. It does not inspect or report link
  health; use `jw doctor` for the repository-wide check.
- For an invalid creation base or missing associated bookmark, follow the doctor
  remedy with `jw repair`. A missing managed record still calls for `jw adopt`, and
  a corrupt record needs manual restore.

Complete diagnosis when the target workspace and reported hazards are identified without refreshing or otherwise mutating state.

## Divergence recovery

`jw doctor` is read-only. When it reports divergent mutable JJ changes, inspect the
affected change IDs, all visible revisions for each ID, descendants, file conflicts,
bookmarks, and other active work before rewriting anything. Preserve immutable and
ambiguous history for explicit user direction.

On JJ 0.45 or newer, process one inspected change ID at a time with an explicit
revision search space and `jj converge --no-interactive --revision '<revset>'`.
Convergence can rewrite descendants, move local bookmarks, or leave file conflicts;
an automatic command exit is not proof that recovery is complete. If heuristics are
inconclusive, `--no-interactive` leaves the divergence unchanged and reports a
warning. On older supported JJ versions, do not pass this option; use the existing
manual recovery workflow and report that automatic convergence is unavailable.

After each recovery, inspect `jj status`, the affected graph and descendants,
`jj bookmark list`, remaining divergence, file conflicts, `jj op show -p`, and
`jj evolog`. Leave unresolved or ambiguous recovery untouched. Only continue to the
next change ID after the previous one has been checked.

Complete recovery when the requested change IDs have no remaining unexpected
divergence or conflicts, affected descendants and bookmarks are understood, and the
operation diff matches the intended repair.

## Adopt

`jw adopt <name> --base <revset>` optionally records an existing JJ workspace's
lifecycle metadata; it does not move revisions or bookmarks or refresh the working
copy. It records external ownership when actual Git topology is a linked worktree.
This protects lifecycle ownership without claiming that JJ adoption transfers it to
`jw`. Live linked topology is protected even without this record.

Inspect the workspace and resolve the base before adoption. Use `--bookmark` only to record an existing association. Use `--no-bookmark` when no association should be recorded, including when ignoring a stale legacy marker.

Complete adoption when `jw status <name> --refresh none` reports the intended managed metadata and JJ revision/bookmark state is unchanged.

## Repair metadata

`jw repair NAME --base REVSET (--bookmark BOOKMARK | --no-bookmark)` corrects an
existing readable managed record. `NAME` must be literal; do not use `@`, `-`, `^`,
or another routing shortcut. The named workspace must be registered with JJ, and
the replacement base must resolve to exactly one revision at one frozen validation
operation. A requested bookmark must already exist locally. The checkout path may be
stale or unusable; JJ workspace registration is the safety gate.

Repair replaces only the recorded creation base and bookmark association. It preserves
creation time, creation operation ID, and intended remote. The replacement is atomic:
validation, a changed record, or a write failure leaves the old record intact. Repair
does not create a JJ operation, change commits or bookmarks, refresh a working copy,
or create a checkout. Missing records still use `jw adopt`; corrupt records need
manual restore.

Complete repair when the command reports the old and new metadata values and the
frozen validation operation, and a read-only doctor/status inspection shows the
intended record without any JJ revision or bookmark mutation. Repair has no JSON
output contract and does not add link health to `jw status`.

## Remove

Run removal only for requested cleanup. Inspect enough state to establish the
workspace identity, owner, checkout contents, and bookmark policy. Native app linked
Git checkouts remain externally owned even after JJ adoption. `jw` refuses removal,
forgetting (including `--keep-dir`), UI removal, and pruning of those checkouts.
Do not bypass this with native JJ/Git cleanup or a gardener; use the owning app.

- `jw remove <name>` forgets the workspace and deletes its directory by default.
- `--keep-dir` forgets it while preserving the directory; prefer this when file preservation is uncertain.
- Removing the default workspace is refused.
- Deleting the current workspace directory is refused until the user switches away. `--keep-dir` can still forget the current workspace.
- Associated bookmarks prompt by default. Use `--keep-bookmark` or `--delete-bookmark` to make the user's choice explicit for non-interactive work.

Complete removal when `jw list` no longer contains the workspace and the directory and bookmarks match the chosen preservation policy.

## Prune

`jw prune` forgets workspaces whose paths are already missing. It does not share `remove`'s explicit default-workspace guard. Inspect `jw list` and `jj workspace list` first, and run it only when every missing entry is intended for forgetting.

Complete pruning when only the expected missing entries disappeared and the default/current workspaces remain intact.


## Reconcile external state

After the owning app removes both the checkout and Git worktree registration,
`jw reconcile-external NAME` explicitly forgets any stale JJ registration and
removes the matching external metadata. It requires a literal name and readable
external owner record, checks both paths and live Git registrations, and revalidates
before forgetting. An existing checkout, remaining registration, changed JJ path,
or unverifiable state causes refusal. Commits and bookmarks are retained.

Automatic prune and gardener cleanup must leave externally owned records alone,
even after checkout removal. Missing external metadata needs restoration before
explicit reconciliation. Schemas 1 and 2 remain readable; new records use schema 3 with optional external
ownership or explicit jw-created Git provenance. Live topology checks protect old
records without inferring ownership. `jw add --colocate`, `jw switch --colocate`,
and `workspace.colocate = true` opt into creation on JJ 0.46+; `--no-colocate`
overrides config, and existing workspaces are never converted. For a jw-created
colocated workspace, removal validates its provenance token and reciprocal Git
metadata and cleans only that Git registration. `--keep-dir` preserves files but
disconnects the Git link. Changed topology, locks, and missing/damaged provenance
require inspection; native app ownership remains protected. Interrupted cleanup
can retain `.jj/jw-detached-git` or `.git/worktrees/.jw-cleanup-<token>`; inspect
those paths and the retained lifecycle record before manual recovery.
