# Changelog

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
