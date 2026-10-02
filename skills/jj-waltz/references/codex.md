# Native app checkout ownership

A native app owns the checkout it creates and its Git worktree registration. Keep
agent work in that checkout. Do not create a second JJ workspace just to route an
agent, and do not remove the app's checkout through `jw`, native JJ forget/remove,
Git worktree cleanup, or a gardener.

Use `jw context [PATH]` when checkout identity or capability is unclear. It reports
Git checkout topology and JJ workspace identity separately. Inspect the actual
checkout: an ordinary Git checkout can use Git, while an adopted checkout can use
JJ through its existing shared repository. A nearby JJ primary checkout does not
make the current Git-only checkout a JJ workspace. Use the capability that is
present; adoption or a particular command ritual is not required to start work.

If JJ adoption is explicitly requested, check the installed binary's help and
compatibility first. `jw adopt` records lifecycle intent for a workspace that already
exists in JJ; it does not initialize or adopt a Git worktree into JJ. Some JJ builds
provide native Git worktree adoption. Adoption keeps the original checkout and its
external lifecycle owner; it does not authorize a second workspace or cleanup.

Use the checkout's exact requested starting commit and an explicit command working
directory. Inspect existing changes before edits and preserve work that belongs to
someone else. Give each checkout one writer. An orchestrator may inspect history,
status, results, and artifacts read-only while a worker writes; it must not refresh,
commit, modify files, or move bookmarks in that worker's checkout. Put independent
writers in separately authorized app checkouts, with independent mutable outputs.

`jw` lifecycle management is optional. Use it only for a requested JJ workspace
lifecycle action. Live linked Git topology is protected even without `jw` metadata.
Recording lifecycle metadata with `jw adopt` also persists the external owner so
missing or damaged checkout metadata cannot silently transfer ownership to `jw`.
`jw add` creates an ordinary JJ checkout, opting out of Git worktree colocation on
builds that support that option.

For cleanup, let the owning app remove both the checkout and Git registration.
After verifying both are gone, explicitly run `jw reconcile-external NAME` to forget
any leftover JJ registration and remove its external lifecycle record. This command
retains commits and bookmarks. `--keep-dir` does not make native JJ forgetting safe:
some builds remove Git registration as a side effect, and undo does not restore it.
