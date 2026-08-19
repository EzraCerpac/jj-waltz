---
name: jj-waltz
description: Use when the user mentions jw, jj-waltz, or .jwlinks.toml; asks to manage jj workspaces or worktrees; or requests parallel work in separate workspaces.
---

# jj-waltz

Use `jw` for explicit JJ workspace lifecycle and navigation work. A generic parallel-agent task stays in its existing checkout unless the user requests separate workspaces.

## Workflow

### 1. Inspect

Identify the JJ repository, current directory, requested workspace names, and whether the task creates, switches, diagnoses, adopts, links, or removes workspaces.

Before a mutation, inspect the relevant state with `jw list`, `jw current`, `jw path <name>`, or `jw status <name>`. Use `jw --help` and the relevant subcommand help for current syntax instead of relying on a cached command list.

When installed behavior may differ from the repository or documentation, verify all three before making version-specific claims:

```bash
command -v jw
realpath "$(command -v jw)"
jw --version
```

Complete inspection when the executable identity, target workspaces, current state, and intended mutation are unambiguous.

### 2. Route and act

Choose only the branch the request needs. Load only the reference linked by that branch; treat `evals/` and unrelated references as test data, not runtime instructions.

#### Create, switch, or execute

- `jw add <name>...` creates workspaces without switching.
- `jw switch <name>...` creates missing workspaces and selects the last name.
- Without `--at`, creation requires `parents(@)` to resolve to exactly one revision. For a merge working copy, choose the intended base explicitly with `--at`.
- `--bookmark` applies to one workspace. For batch creation, use the configured `bookmark_template` or omit bookmarks.
- Prefer `jw switch --execute <command> <name>` when a tool or agent should run inside the target. The command runs there without changing the parent shell's directory.

Use `jw` rather than Git worktrees for JJ workspace lifecycle. Use ordinary `jj` commands for revision history, bookmarks outside `jw` lifecycle choices, and remotes.

#### Shell navigation

The binary cannot change its parent shell. Shell integration turns the path returned by `jw switch` into a directory change; `--execute` intentionally bypasses that behavior.

Use the user's shell:

```bash
eval "$(jw shell init zsh)"     # zsh
eval "$(jw shell init bash)"    # bash
jw shell init fish | source     # fish
```

After adding shell init, restart or re-source the shell before retrying the switch.

#### Workspace identity

Use `jw current`, `jw root`, and `jw path <token>` instead of inferring identity from directory names. `@` is current, `-` is previous, and `^` or `default` resolves the default workspace.

Switching from a subdirectory carries that relative path to a sibling workspace when it exists; otherwise the destination is the target workspace root.

#### Links

For `.jwlinks.toml`, shared ignored directories, missing targets, or link conflicts, read [`references/links.md`](references/links.md) before changing configuration or retrying link creation.

#### Status, adoption, cleanup, or recovery

For `status`, `doctor`, `adopt`, `remove`, or `prune`, read [`references/lifecycle.md`](references/lifecycle.md) before acting. It contains the mutation gates and removal safeguards.

#### Explicit parallel workspaces

When the user requests parallel JJ workspaces, use one workspace per substantial independent task, reuse a clearly matching workspace, and keep tightly coupled or sequential work together. Prefer short task-shaped names and `jw switch --execute` when launching work. Follow the host's policy for agents and parallel execution; workspace creation does not grant permission to spawn agents.

Complete this stage when the requested branch either succeeds or stops with the exact unresolved state and no unintended workspace mutation.

### 3. Verify

Verify the boundary the user will rely on:

- creation or switching: confirm `jw list`, `jw current`, and `jw path <name>`;
- executed tools: confirm their exit status and working directory;
- shell navigation: confirm from the initialized interactive shell, not from the child binary alone;
- links or lifecycle work: use the completion checks in the branch reference.

Report the resulting workspace names and paths, what changed, and any shell, executable-version, directory, link, or bookmark state that remains unverified.

Complete the task only when observed state matches the requested workspace outcome.
