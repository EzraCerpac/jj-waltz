# Changelog

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
