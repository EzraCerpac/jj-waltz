# jj-waltz

`jj-waltz` is a Jujutsu workspace switcher with the short binary name `jw`.
It is built for fast parallel development, reliable shell integration, and clean distribution.

Fish is the recommended shell for the best `jw` experience, including the richest completions and native directory-switching integration.

## Why

Jujutsu workspaces are powerful, but the raw workflow is still more manual than it needs to be.
`jj-waltz` makes switching feel intentional: create or jump in one command, preserve your current subdirectory, and integrate cleanly with your shell.

This project is directly inspired by [Worktrunk](https://github.com/max-sixty/worktrunk), which set a high bar for ergonomic worktree tooling in Git-centric workflows. `jj-waltz` brings a similar design philosophy to JJ-native workspace management.

## Features

- `jw add <name>...` creates one or more JJ workspaces without switching
- `jw switch <name>` creates or switches to a JJ workspace
- `jw switch <name>...` creates any missing workspaces and switches to the last one
- `jw s <name>` short alias for the main workflow
- `jw ^` and `jw -` switch to the default and previous workspaces
- preserve the current subdirectory when switching between sibling workspaces
- shortcuts for current, previous, and default workspaces: `@`, `-`, `^`
- `jw list` (`jw l`, `jw ls`), `jw path`, `jw remove <name>...`, `jw prune`, `jw root`, `jw current`
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
define links in `.jwlinks.toml`:

```toml
[[link]]
source = "data"
target = "../ezra-cerpac/data"
required = true
```

When you run `jw switch`, `jw` creates symlinks in the target workspace unless you pass
`--no-links`. You can also run `jw links apply` manually.

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
jw remove frontend tests
```

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

## AI usage note

This project supports AI-assisted development workflows, and portions of its implementation and documentation may be created with AI assistance. All shipped behavior is intended to be human-reviewed, tested, and maintained to production standards.

## Status

`jj-waltz` is under active development. The core workflow is already functional, and the project is being hardened into a complete standalone CLI with robust testing, release automation, and public distribution.

## License

MIT
