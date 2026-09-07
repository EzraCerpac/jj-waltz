# Codex checkout routing

Read this reference when a task starts in Codex, a Git worktree, or a directory where
the Git checkout and JJ workspace may not be the same path.

## Discover the identities

Run `jw context [PATH]` from the task's starting directory. Use the default human
format for a report and `--format=json` when a launcher or other tool must consume
the result. Resolve `PATH` to an existing directory first. If it is nested inside a
checkout, report the nested input and the discovered checkout/workspace roots
separately; do not treat the nested directory as the repository root.

- Git topology identifies the checkout root, worktree Git directory, common Git
  directory, and the exact starting commit. In JSON, expose the full Git object ID
  as nullable `git.head_commit`; an unavailable HEAD is a diagnostic, not a
  substitute revision.
- JJ identity identifies the JJ repository, current workspace, and a verified
  related primary checkout when one can be found.

An ordinary Git-only checkout and a non-repository directory are valid discovery
results. Missing tools, inaccessible paths, and broken metadata are diagnostics that
must be resolved or reported before routing.

## Route a task

Reuse the current JJ workspace only after checking that its registered path is
usable, its current/base revision matches the requested starting commit, and its
working state belongs to this task. When isolation is requested, create or select a
task-shaped JJ workspace with an explicit `--at` revision.

Use `jw path NAME` and `jw status NAME --format=json --refresh=none` to inspect
an existing named workspace, including its recorded creation base.

For a clean Codex Git checkout associated with a JJ project:

1. From the discovered Git checkout root, confirm cleanliness with exactly:
   `git --no-optional-locks status --porcelain=v1 --untracked-files=all`. An empty
   result is required; JJ status does not replace this check. A nested task path
   does not change which root receives this command.
2. Confirm that the exact `git.head_commit` resolves in JJ from the verified primary
   checkout. Use the primary root explicitly and avoid refreshing the working copy:
   `jj --ignore-working-copy --at-op=@ --repository <primary-jj-root> log -r <head-commit>`.
   Require exactly one resolved revision. A detached HEAD is still a starting
   commit; preserve it explicitly.
3. Create a missing task-shaped workspace from that primary checkout with the
   requested name and `--at <head-commit>`. `--at` chooses the base only when
   creating a missing workspace; it does not relocate or rebase an existing named
   workspace. Before reusing an existing name, inspect its path and current/base
   revision and verify that it is suitable. Do not claim that `--at` changed it.
4. Set an explicit working directory to the selected JJ workspace for every task
   command. Report that path before editing, testing, or inspecting task state.

`jw adopt` records an existing JJ workspace as managed; it does not initialize JJ
inside a Git worktree. Keep adoption separate from Codex routing.

If the Codex checkout has edits, or the exact starting commit cannot be resolved in
JJ, stop automatic routing. Explain the required transfer or repair and wait for the
user's direction; do not copy files, reset the checkout, or silently choose another
base.

The Codex-created Git worktree is externally owned. Use JJ output from the selected
workspace as authoritative when Codex's native Git panel points somewhere else, and
never remove the Codex worktree through `jw` cleanup. Running a command in another
directory does not retarget Codex's native Git panel.

Complete routing when the chosen JJ workspace, exact base commit, explicit command
directory, and ownership boundary have all been reported and verified.
