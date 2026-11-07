# Changelog

## Unreleased

- Read every workspace root with one `jj workspace list` process on JJ releases
  whose templates expose `WorkspaceRef.root()`, instead of one `jj workspace root`
  process per workspace. With ~180 workspaces this takes `jw add` and `jw remove`
  from about 13 seconds to about one. Older JJ keeps per-workspace queries, trying
  `default` first when locating the primary workspace.
- No command starts more JJ processes because a repository has more workspaces
  (JJ 0.44+; 0.39 keeps per-workspace root queries). With 188 workspaces, opening
  `jw` (the manager) goes from 200 JJ processes and ~13 seconds to 10 processes and
  under half a second; `jw list --format=json`, `jw status`, and `jw doctor` go from
  18–25 seconds to about half a second. A new test enforces flat process counts.
  - Workspace roots are read from one listing in every snapshot, adoption, the
    manager's nested-workspace check, and doctor.
  - Diff statistics, which read file contents, are computed only for refreshed,
    modified working copies; the per-workspace facts query no longer computes them.
    The manager reuses the snapshot's facts instead of querying them again.
  - Trunk ancestry is answered by JJ for the inspected revisions only, instead of
    loading all history, so the manager's cost no longer grows with history.
    Divergence now uses JJ's `divergent` rather than duplicates within that history.
  - Creation bases, bookmark targets, and doctor's checks are batched. Ownership
    checks share one read of the Git backend and linked registrations, and inspect a
    checkout's Git topology with one `git` process instead of three.
  - The manager revalidates a removal batch with one capture instead of one per row,
    and prunes missing workspaces with one `jj workspace forget`.
- `jw remove` returns without waiting for large checkouts to be deleted. Removal
  snapshots the target working copy (preserving its tracked and new files, and
  refusing a stale one), forgets it, renames the directory to a hidden sibling, and
  deletes it in a detached process. With a 3.4 GB `node_modules` this takes removal
  from ~4.7 seconds to ~0.2. Snapshotting now also protects dirty targets on JJ
  older than 0.46. Native `jj workspace remove` is no longer used, because it
  deletes synchronously.
- `jw prune` validates every missing workspace before forgetting any of them; one
  refused workspace now leaves every workspace registered instead of pruning those
  listed before it.
- Explicit unsupported workspace colocation now advises upgrading to JJ 0.46+
  or using `--no-colocate`; automatic inheritance still falls back to JJ-only.
- Speed up workspace-name shell completion: it now makes a single JJ call that
  skips the working-copy snapshot instead of one snapshotting call per workspace.
  Candidates no longer note which workspace is current, previous, or default.

## Version 0.8.0 (2026-10-09)

**Breaking change: new workspace colocation defaults.**

### Breaking change and migration

- When no explicit CLI or jw configuration override is present, new workspaces
  inherit colocation from a verified colocated primary workspace when its effective
  JJ `git.colocate` setting is true and JJ supports workspace colocation. Previously,
  new workspaces defaulted to JJ-only. Creation from registered JJ-only or colocated
  siblings now uses the same primary policy.
- To retain the previous JJ-only creation behavior, set `workspace.colocate = false`
  in jw configuration or pass `--no-colocate` for one command. Explicit CLI
  true/false overrides explicit jw configuration; omitted configuration inherits.
  Existing workspaces are not converted. Automatic CoW creation remains JJ-only;
  explicitly combining CoW and colocation remains an actionable error.
- Older supported JJ versions and unverified or non-Git primary topology continue
  to create JJ-only workspaces. No repositories are migrated or adopted automatically.

### Features and fixes

- Call Linux's exclusive rename syscall directly instead of requiring the
  `renameat2` libc wrapper, preserving atomic refusal to overwrite occupied
  destinations on both GNU and static musl builds.
- Build and exercise the static musl binary in PR CI before publication.
- Add opt-in copy-on-write workspace creation with `jw add --cow`,
  `jw switch --cow`, or `workspace.copy_on_write = true`. New workspaces clone
  the current workspace's tracked files on APFS, Btrfs, or XFS and match a full
  checkout of the creation base within the source workspace's sparse patterns.
  Unsupported filesystems, Watchman, and rejected copied working-copy state fall
  back to ordinary JJ workspace creation while preserving those patterns;
  `--no-cow` opts out for one command. Use a private, exclusively created directory
  for the reflink capability probe. Thanks to @gergesh for the contribution (#48).
- Add opt-in Git worktree creation with `jw add --colocate`, `jw switch --colocate`,
  and `workspace.colocate = true` on JJ 0.46+. `--no-colocate` overrides config.
  Track jw-created Git worktree ownership and clean only its registration; external
  checkouts and unrelated stale registrations remain protected.
- Reject combining copy-on-write and Git colocation, including when enabled in
  config; use `--no-cow` or `--no-colocate` to override one option.
- Protect externally owned Git checkouts and registrations during workspace
  removal, pruning, and rollback (#49).
- Explain whole-directory link conflicts with existing directories, including
  tracked files, and suggest a local self-target override or sharing only absent
  untracked paths while preserving refusal and workspace rollback.
- Correct Windows exclusive ownership operations, metadata initialization, path
  alias handling, and PowerShell switching and command execution.
- Test JJ 0.39.0, 0.44.0, 0.45.1, and 0.46.0 on Linux, macOS, and Windows,
  requiring real copy-on-write success on macOS. Update root and Herdr dependencies
  while retaining Rust 1.88 as the minimum supported version.

## Version 0.7.0 (2026-09-25)

- Add a persistent workspace manager with `jw ui` and bare `jw` in a terminal,
  with search, filters, multi-selection, workspace creation, and shell switching.
- Show bookmark integration separately from workspace work using local trunk
  ancestry, with explicit unknown, conflict, and unchecked states.
- Preview bulk removal and missing-workspace pruning, protect current/default
  workspaces, and require a choice before deleting files not recorded by JJ.
- Keep unmanaged bookmarks and bookmarks shared with surviving workspaces;
  report partial failures without blocking independent removals or retries.
- Add semantic terminal colors and a mouse/text-selection toggle (`m`), with
  readable default foreground text across terminal themes.
- Support configurable bare-command behavior and retain existing CLI/JSON commands.
- Raise the minimum Rust version to 1.88.

## Version 0.6.1 (2026-09-07)

- Preserve whitespace in checkout and workspace paths reported by `jw context`.
- Ignore inherited Git discovery ceilings when inspecting checkout paths.
- Skip the JJ routing-hint test when JJ is unavailable.

## Version 0.6.0 (2026-09-07)

- Add read-only `jw context [PATH] --format human|json` checkout discovery and
  verified Git-worktree-to-JJ-primary association, with Codex routing guidance.
- Exercise the compatibility matrix against JJ 0.45.1 and teach `jw doctor` to
  recommend non-interactive `jj converge` for divergent changes on JJ 0.45 and
  newer while retaining the manual recovery guidance for older supported JJ.

## Version 0.5.0 (2026-08-21)

- Make `jw doctor` inspect every configured workspace link for every managed
  workspace, reporting satisfied, missing, skipped, conflicting, and unreadable
  rules without hiding later diagnostics.
- Add `jw repair NAME --base REVSET (--bookmark BOOKMARK | --no-bookmark)` for
  safe, atomic repair of an existing workspace's managed base and bookmark
  metadata without changing JJ commits, bookmarks, operations, or checkouts.

## Version 0.4.1 (2026-08-15)

- Keep `jw doctor` machine-readable when jj-waltz configuration cannot be read
  or parsed, with a failed configuration diagnostic and skipped trunk check.
- Let `jw adopt --no-bookmark` ignore stale legacy bookmark markers while
  preserving the existing explicit bookmark association behavior.
- Correct architecture documentation to name JJ 0.44.0 as the newer tested
  compatibility target.

## Version 0.4.0 (2026-08-15)

- Add semantic `list --format=json`, `status`, `doctor`, and `adopt` commands
  backed by frozen, schema-versioned snapshots and repository-scoped lifecycle
  metadata.
- Diagnose corrupt, stale, and missing workspace state without silently replacing
  metadata or changing JJ history.
- Make workspace removal report partial progress and clean managed metadata even
  when directory deletion cannot finish immediately.
- Support legacy JJ repositories whose default workspace has no recorded path,
  restoring `jw list`, workspace creation, and the Herdr integration (#30).
- Test the compatibility window against JJ 0.39.0 and JJ 0.44.0.

## Version 0.3.1 (2026-07-12)

- Add a static x86_64 Linux release for older-glibc systems such as CerpacNAS.

## Version 0.3.0 (2026-07-12)

- Make workspace creation and switching transactional, including rollback of
  workspaces, bookmarks, links, directories, and switch state after failures.
- Plan removals before mutation and safely prompt for associated bookmark deletion,
  with explicit flags for scripts.
- Read link rules from the default workspace, reject escaping sources, and preflight
  every rule before changing the filesystem.
- Share one switching and completion policy across Bash, Elvish, Fish, PowerShell,
  and Zsh.
- Harden Herdr removal ordering and validate root, Herdr, and multi-version JJ builds
  independently in CI.

## Version 0.2.1 (2026-06-22)

- Automatically ignore machine-local workspace link configuration.
