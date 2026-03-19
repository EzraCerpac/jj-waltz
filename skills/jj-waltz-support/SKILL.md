---
name: jj-waltz-support
description: Help users use and troubleshoot jj-waltz (`jw`) in real Jujutsu repositories. Use this whenever the user mentions `jw`, `jj-waltz`, switching JJ workspaces, shell init or completions, `.jwlinks.toml`, `.jwlinks.local.toml`, workspace aliases like `@`, `-`, or `^`, `--execute`, or when `jw switch` is not changing directories, links are conflicting, or workspace paths/cleanup are confusing. Prefer this skill even if the user only describes the symptom and does not name `jj-waltz`.
---

# jj-waltz Support

Support users of `jj-waltz` by diagnosing the repo and shell state first, then giving precise commands and a short explanation of why those commands matter.

This skill is specific to this repository's behavior. Do not invent capabilities beyond what `jw` actually supports here.

## What `jw` supports

Ground your answers in the real command surface:

- `jw switch <name>` and alias `jw s <name>`
- `jw list`
- `jw path <name>`
- `jw remove [name]`
- `jw prune`
- `jw root`
- `jw current`
- `jw shell init <shell>`
- `jw shell completions <shell>`
- `jw links apply`

Important options and tokens:

- `--at <revset>` creates a new workspace at a revset
- `--bookmark <name>` creates a bookmark in a new workspace
- `--execute <command>` runs a command after switching instead of changing the current shell directory
- `--no-links` skips link application during `switch`
- `@` means current workspace
- `-` means previous workspace
- `^` and `default` resolve to the default workspace

## Default approach

Start with diagnosis, not generic advice.

1. Confirm the user's goal and symptom in one sentence.
2. Inspect the local environment if the task depends on current repo state.
3. Give the smallest set of commands that would verify or fix the issue.
4. State the expected result so the user knows what success looks like.

Keep the response concise and support-oriented. Prefer actionable help over tutorial prose.

## What to inspect

When the user is troubleshooting and local context is available, inspect the most relevant state before answering:

- current working directory
- current shell if shell integration is involved
- `jw --help` or a relevant subcommand help page if behavior is unclear
- JJ workspace state with `jj workspace list` or `jw list`
- `jw current`, `jw root`, or `jw path <name>` for path confusion
- `.jwlinks.toml` and `.jwlinks.local.toml` for link issues
- whether the user is invoking `jw switch` directly or through shell init

Do not mutate repo-tracked files unless the user explicitly asks for that. For support cases, inspect first and then recommend the next command.

## Support playbooks

### Shell integration

If the user says `jw switch` does not change directories, explain the actual model:

- The binary prints or resolves the target path.
- Shell integration is what makes the current shell `cd` into that path.
- `jw switch` with `--execute` intentionally does not behave like shell-driven `cd`.

Prefer fish examples first because this project recommends fish, but switch to the user's shell when they mention one:

```bash
# zsh
eval "$(jw shell init zsh)"

# bash
eval "$(jw shell init bash)"

# fish
jw shell init fish | source
```

If shell init is already present, check whether the user started a new shell or re-sourced their config.

### Switching and workspace identity

When the user is confused about where they are or where `jw` will send them:

- Use `jw current` to identify the current workspace.
- Use `jw root` to print the current workspace root.
- Use `jw path <name>` to show the resolved path for a workspace token.
- Explain `@`, `-`, `^`, and `default` directly instead of leaving them implicit.

Preserved subdirectory behavior matters. If a user switches between sibling workspaces while inside a subdirectory, `jw` tries to carry that relative subdirectory across. If that subdirectory does not exist in the target workspace, the effective destination falls back to the workspace root.

### Creating and jumping into workspaces

For creation flows:

- `jw switch <name>` creates the workspace if it does not already exist.
- `jw switch --at <revset> <name>` creates it at a revset.
- `jw switch --bookmark <bookmark> <name>` creates a bookmark in the new workspace.
- `jw switch -x <command> <name>` runs a command after switching instead of relying on shell `cd`.

If the user needs an editor or agent launch, prefer the built-in `--execute` workflow over ad hoc chained shell commands.

### Link configuration

Use link troubleshooting whenever the user mentions shared ignored directories, data directories, caches, or `.jwlinks.toml`.

Ground your advice in the actual behavior:

- `.jwlinks.toml` and `.jwlinks.local.toml` are both supported.
- `.jwlinks.local.toml` can override entries from `.jwlinks.toml` with the same `source`.
- Relative `target` paths are interpreted from the workspace root.
- `required = true` turns a missing target into an error.
- A missing optional target is skipped, not linked.
- Existing matching symlinks or paths count as already satisfied.
- An existing different symlink or path is a conflict.

Use this config shape when showing examples:

```toml
[[link]]
source = "data"
target = "../repo/data"
required = true
```

For link problems, suggest checks in this order:

1. inspect the relevant `.jwlinks.toml` or `.jwlinks.local.toml`
2. verify whether the target exists
3. inspect the source path inside the workspace to see whether it is absent, already correct, or conflicting
4. rerun `jw links apply` or `jw switch <name>` once the underlying issue is fixed

### Listing, cleanup, and recovery

When users need to understand or clean up workspace state:

- `jw list` shows known workspaces and marks current, previous, and default entries
- `jw remove <name>` forgets a workspace and deletes its directory by default
- `jw remove --keep-dir <name>` forgets it but leaves the directory in place
- `jw prune` forgets missing workspaces

Warn about the important safeguards:

- removing `default` is refused
- deleting the current workspace directory is refused until the user switches away first

## Response format

Use this shape unless the user asks for something else:

1. One-line diagnosis or framing.
2. A short command block with the exact next commands.
3. One or two lines describing the expected result.

If the user asked a conceptual question rather than a broken-state question, skip the diagnosis line and answer directly with the relevant commands and explanation.

## Guardrails

- Do not describe `jw switch` as changing the shell directory by itself. That requires shell init.
- Do not tell users `.jwlinks.toml` targets are relative to the config file location; here they are resolved from the workspace root.
- Do not recommend destructive cleanup when a simpler inspection command would answer the question.
- Do not assume `default` is always a literal workspace name; it is also a supported token.
- Do not overexplain JJ broadly unless the user asked for JJ background.

## Examples

**Example 1**

User: `jw switch feature-ui creates the workspace but my zsh prompt stays in the same directory`

Response shape:

- explain that shell init is missing or not loaded
- give `eval "$(jw shell init zsh)"` and a re-source step if appropriate
- tell the user to retry `jw switch feature-ui`

**Example 2**

User: `i want every workspace to share my ignored data dir and jw says link conflict`

Response shape:

- inspect `.jwlinks.toml`
- explain the difference between a missing target and an existing conflicting path
- give commands to inspect the target and conflicting workspace path before retrying `jw links apply`

**Example 3**

User: `what does jw switch - do, and how do i get back to the default workspace?`

Response shape:

- explain `-`, `@`, `^`, and `default`
- show `jw current`, `jw switch -`, and `jw switch default`
