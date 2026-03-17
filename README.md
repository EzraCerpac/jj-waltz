# jj-waltz

`jj-waltz` is a Jujutsu workspace switcher with the short binary name `jw`.
It is built for fast parallel development, reliable shell integration, and clean distribution.

Fish is the recommended shell for the best `jw` experience, including the richest completions and native directory-switching integration.

## Why

Jujutsu workspaces are powerful, but the raw workflow is still more manual than it needs to be.
`jj-waltz` makes switching feel intentional: create or jump in one command, preserve your current subdirectory, and integrate cleanly with your shell.

This project is directly inspired by [Worktrunk](https://github.com/max-sixty/worktrunk), which set a high bar for ergonomic worktree tooling in Git-centric workflows. `jj-waltz` brings a similar design philosophy to JJ-native workspace management.

## Features

- `jw switch <name>` creates or switches to a JJ workspace
- `jw s <name>` short alias for the main workflow
- preserve the current subdirectory when switching between sibling workspaces
- shortcuts for current, previous, and default workspaces: `@`, `-`, `^`
- `jw list`, `jw path`, `jw remove`, `jw prune`, `jw root`, `jw current`
- `--execute` support for jumping into editors or agents after switching
- shell integration for `fish`, `zsh`, `bash`, `elvish`, and `powershell`
- generated shell completions from the CLI definition

## Install

### Homebrew

Install from the public tap:

```bash
brew tap ezracerpac/homebrew-tap
brew install jj-waltz
```

### Cargo

```bash
cargo install --git https://github.com/EzraCerpac/jj-waltz --locked
```

## Shell setup

Initialize your shell so `jw switch` can change the current shell directory:

```bash
# bash
eval "$(jw shell init bash)"

# zsh
eval "$(jw shell init zsh)"

# fish
jw shell init fish | source
```

To generate completions manually:

```bash
jw shell completions fish
jw shell completions zsh
jw shell completions bash
```

## Quick start

```bash
jw switch feature-api
jw switch -x opencode feature-ui
jw switch default
jw switch -
jw list
```

## AI usage note

This project supports AI-assisted development workflows, and portions of its implementation and documentation may be created with AI assistance. All shipped behavior is intended to be human-reviewed, tested, and maintained to production standards.

## Status

`jj-waltz` is under active development. The core workflow is already functional, and the project is being hardened into a complete standalone CLI with robust testing, release automation, and public distribution.

## License

MIT
