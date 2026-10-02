---
name: jj-waltz
description: Use when the user mentions jw, jj-waltz, or .jwlinks.toml; asks to manage jj workspaces or worktrees; or requests parallel work in separate workspaces.
---

# jj-waltz

Use `jw` when the user requests JJ workspace lifecycle or navigation work. Its
lifecycle management is optional. Keep native app tasks in the checkout the app
created, use its actual Git or JJ capability, and give each checkout one writer.
An orchestrator inspecting a worker checkout stays read-only. Do not create another
JJ workspace merely to route an agent; no mandatory inspection ritual or adoption
step is required for ordinary coding.

## Workflow

### 1. Inspect

Identify the JJ repository, current directory, requested workspace names, and whether the task creates, switches, diagnoses, adopts, links, or removes workspaces.

Before a mutation, inspect the relevant state with `jw list`, `jw current`, `jw path <name>`, or `jw status <name> --refresh none`. Use `jw --help` and the relevant subcommand help for current syntax instead of relying on a cached command list.

When installed behavior may differ from the repository or documentation, inspect any shell wrapper and resolve the executable beneath it before making version-specific claims. Use the current shell's executable-only lookup: `type -P jw` in Bash, `whence -p jw` in Zsh, or `command -s jw` in Fish. Then verify the resolved path and version:

```bash
resolved_jw="$(whence -p jw)"  # zsh; use the lookup above for bash or fish
realpath "$resolved_jw"
"$resolved_jw" --version
```

When using `jw` and checkout identity is unclear, `jw context [PATH]` can inspect it
read-only. Use `--format=json` when another tool consumes the result. Keep Git
checkout topology and JJ workspace identity separate. For a requested lifecycle
change to a native app checkout, [`references/codex.md`](references/codex.md)
explains ownership and capability checks.

Check `jw --help` before relying on `context`; an older installed jw may need an
authorized build or upgrade even when the source skill is current.

Complete inspection when the executable identity, target workspaces, current state, and intended mutation are unambiguous.

### 2. Route and act

Choose only the branch the request needs. Load only the reference linked by that branch; treat `evals/` and unrelated references as test data, not runtime instructions.

#### Create, switch, or execute

- `jw add <name>...` creates workspaces without switching.
- `jw switch <name>...` creates missing workspaces and selects the last name.
- Without `--at`, creation requires `parents(@)` to resolve to exactly one revision. For a merge working copy, choose the intended base explicitly with `--at`.
- `--bookmark` applies to one workspace. For batch creation, use the configured `bookmark_template` or omit bookmarks.
- Prefer `jw switch --execute <command> <name>` when a tool or agent should run inside the target. The command runs there without changing the parent shell's directory.

Use `jw` for requested JJ workspace lifecycle actions and ordinary `jj` commands
for revision history, bookmarks, and remotes. The native app owns its created Git
checkouts, including those already adopted into JJ.

#### Codex checkout routing

When the task starts from a Codex-owned Git checkout, or when Git and JJ point at
different directories, read [`references/codex.md`](references/codex.md). It describes
actual checkout capabilities, one writer per checkout, and the external ownership
boundary. Keep work in the app-created checkout.

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

When the user explicitly requests separate JJ workspaces, use one writer per
workspace, reuse a clearly matching workspace, and keep tightly coupled or sequential
work together. For native app tasks, use its authorized checkouts and keep the
orchestrator read-only in worker checkouts. Follow the host's policy for agents and
parallel execution; workspace creation does not grant permission to spawn agents.

Complete this stage when the requested branch either succeeds or stops with the exact unresolved state and no unintended workspace mutation.

### 3. Verify

Verify the boundary the user will rely on:

- creation or switching: confirm `jw list` and `jw path <name>`; use `jw current` only after shell-integrated navigation or from a command executed inside the target workspace;
- executed tools: confirm their exit status and working directory;
- shell navigation: confirm from the initialized interactive shell, not from the child binary alone;
- links or lifecycle work: use the completion checks in the branch reference.

Report the resulting workspace names and paths, what changed, and any shell, executable-version, directory, link, or bookmark state that remains unverified.

Complete the task only when observed state matches the requested workspace outcome.
