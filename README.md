# jj-waltz

`jj-waltz` is a Jujutsu workspace switcher with the short binary name `jw`.
It is built for fast parallel development, reliable shell integration, and clean distribution.

Fish is the recommended shell for the best `jw` experience, including the richest completions and native directory-switching integration.

## Why

Jujutsu workspaces are powerful, but the raw workflow is still more manual than it needs to be.
`jj-waltz` makes switching feel intentional: create or jump in one command, preserve your current subdirectory, and integrate cleanly with your shell.

This project is directly inspired by [Worktrunk](https://github.com/max-sixty/worktrunk),
which set a high bar for ergonomic worktree tooling. Worktrunk is a quality
benchmark, not this project's command specification: `jj-waltz` stays focused on
JJ-native workspace lifecycle and cross-workspace coordination.

The responsibility split is deliberate:

- `jj` owns revision-graph work, bookmarks, fetch/push, and operation-log recovery.
- `jw` owns workspace creation, navigation, links, lifecycle metadata, and safe
  cross-workspace coordination.
- Forge tools such as `gh` and `glab` own pull requests, merge requests, and CI.

Core workspace operations remain offline. Optional forge information must degrade
to a warning rather than make local workspace management fail.

## Features

- `jw add <name>...` creates one or more JJ workspaces without switching
- `jw switch <name>` creates or switches to a JJ workspace
- `jw switch <name>...` creates any missing workspaces and switches to the last one
- `jw s <name>` short alias for the main workflow
- `jw ^` and `jw -` switch to the default and previous workspaces
- preserve the current subdirectory when switching between sibling workspaces
- shortcuts for current, previous, and default workspaces: `@`, `-`, `^`
- `jw list` (`jw l`, `jw ls`) keeps its compact legacy output; `--format=json` emits a frozen repository snapshot
- `jw status [workspace]` explains one workspace from the same snapshot contract
- `jw doctor` reports repository, trunk, metadata, and workspace consistency checks
- `jw adopt <name> --base <revset>` records an existing workspace as managed without rewriting JJ state
- `jw path`, `jw remove <name>...`, `jw prune`, `jw root`, and `jw current`
- `--execute` support for jumping into editors or agents after switching
- optional automatic bookmark creation for new workspaces
- optional workspace links via `.jwlinks.toml` for sharing large ignored directories
- shell integration for `fish`, `zsh`, `bash`, `elvish`, and `powershell`
- generated shell completions from the CLI definition

## Install

### Homebrew

Install from the public tap:

```bash
brew install EzraCerpac/tap/jj-waltz
```

### Cargo

```bash
cargo install --git https://github.com/EzraCerpac/jj-waltz --locked
```

### Herdr plugin

Install the bundled Herdr UI to create and remove `jw` workspaces from Herdr:

```bash
herdr plugin install EzraCerpac/jj-waltz/plugins/herdr
```

The plugin builds the `jw` binary from the same repository revision and delegates all
workspace paths, links, bookmarks, and removal checks to it. See
[`plugins/herdr`](plugins/herdr) for keybindings and local development.

## Workspace links

If you keep large ignored data in one workspace and want it accessible from others,
define links in the default workspace's `.jwlinks.toml`:

```toml
[[link]]
source = "data"
target = "../ezra-cerpac/data"
required = true
```

When you run `jw switch`, `jw` creates symlinks in the target workspace unless you pass
`--no-links`. You can also run `jw links apply` manually from any workspace; it still
uses the default workspace's configuration. Relative targets resolve from the workspace
receiving the links.

Sources must stay inside the receiving workspace. Absolute sources and parent traversal
such as `../outside` are rejected. All rules are checked before `jw` changes the filesystem,
so a later conflict does not leave earlier links behind.

For machine-specific overrides, add `.jwlinks.local.toml` (recommended to keep ignored).

## Shell setup

Initialize your shell so `jw switch`, `jw ^`, and `jw -` can change the
current shell directory:

```bash
# bash
eval "$(jw shell init bash)"

# zsh
eval "$(jw shell init zsh)"

# fish
jw shell init fish | source

# elvish
eval (jw shell init elvish | slurp)

# PowerShell
jw shell init powershell | Out-String | Invoke-Expression
```

Without shell initialization, the raw `jw` binary can only print the target path
or status; it cannot change the directory of the parent shell process.

To generate completions manually:

```bash
jw shell completions fish
jw shell completions zsh
jw shell completions bash
jw shell completions elvish
jw shell completions powershell
```

## Quick start

```bash
jw switch feature-api
jw add frontend tests docs
jw switch frontend tests docs
jw switch -x opencode feature-ui
jw ^
jw -
jw ls
jw list --format=json
jw status @ --format=json --refresh=none
jw doctor
jw remove frontend tests
```

## Removing workspaces

When a workspace has a bookmark created by `jw`, `jw remove` asks before deleting
that bookmark. The safe default is to keep it. Scripts can choose explicitly:

```bash
jw remove --delete-bookmark feature-api
jw remove --keep-bookmark feature-api
```

Removal is planned before mutation, so default/current-workspace checks and bookmark
choices happen before the workspace is forgotten. `--keep-dir` forgets the workspace
without deleting its directory.

## Config

`jw` reads user config from `$XDG_CONFIG_HOME/jj-waltz/config.toml`, or
`~/.config/jj-waltz/config.toml` when `XDG_CONFIG_HOME` is not set.

To create a bookmark automatically whenever `jw switch` creates a workspace:

```toml
[workspace]
create_bookmark = true
bookmark_template = "{workspace}"
```

`bookmark_template` defaults to `{workspace}`. The `{workspace}` token is replaced
with the resolved workspace name, so templates like `wip/{workspace}` are valid.

For one command, `jw switch --bookmark custom-name feature-a` overrides the config,
and `jw switch --no-bookmark feature-a` suppresses configured bookmark creation.
Explicit `--bookmark` is single-workspace only; for batch `add` or `switch`, use
`bookmark_template`.

Status compares against one configured trunk revision. It defaults to JJ's
`trunk()` revset:

```toml
[trunk]
revset = "trunk()"
```

The revset must resolve to exactly one revision. Workspace creation also resolves
one exact base before changing anything. With no `--at`, `jw` uses the sole
`parents(@)` revision, preserving sibling-workspace behavior. An empty merge
working copy has multiple parents, so implicit creation stops before mutation;
use `--at @` when creating directly from that merge is intentional.

## Semantic status and JSON

```bash
# Historical human list; does not load trunk or managed metadata.
jw list

# Versioned snapshot. List JSON refreshes current workspace by default.
jw list --format=json --refresh=current
jw list --format=json --refresh=none
jw list --format=json --refresh=all

# One workspace. Tokens @, -, ^, and default are supported.
jw status @
jw status feature-api --format=json --refresh=none

# Complete diagnostics. An unhealthy report is still written before exit 1.
jw doctor --format=json
```

`list` and `status` JSON use schema version 1 and include frozen JJ operation and
resolved trunk IDs. Known missing values stay explicit `null`; JSON contains no
ANSI escapes. `doctor` uses its own schema-versioned report because it must remain
valid even when trunk or metadata is broken.

`jw adopt NAME --base REVSET [--bookmark BOOKMARK]` records lifecycle intent for
an existing workspace. It does not move revisions or bookmarks and does not
refresh the working copy. Milestone 0 reports the exact base and current revision;
workspace-stack analysis is deferred to milestone 1.

## Semantic contracts

[`CONTEXT.md`](CONTEXT.md) defines workspace, snapshot, trunk, metadata, stack,
publication, integration, cleanup, and refresh vocabulary. The
[architecture notes](docs/architecture.md) define dependency direction, metadata
storage, and JSON compatibility. Defined roadmap concepts do not imply an
unlisted command is available; the feature list and `jw --help` are the current
command surface.

`jw` supports JJ 0.39 and newer within its tested compatibility window. CI pins
the oldest supported release, 0.39.0, and the newer compatibility target, 0.43.0,
instead of following a moving `latest` label.

## AI usage note

This project supports AI-assisted development workflows, and portions of its implementation and documentation may be created with AI assistance. All shipped behavior is intended to be human-reviewed, tested, and maintained to production standards.

## Status

`jj-waltz` is under active development. The core workflow is already functional, and the project is being hardened into a complete standalone CLI with robust testing, release automation, and public distribution.

## License

MIT
