# jj-waltz for Herdr

Herdr UI for creating and removing Jujutsu workspaces through `jw`.
The plugin bundles the `jw` binary from the same repository revision, so workspace
paths, links, bookmarks, and safety checks stay owned by jj-waltz.
It stores only a Herdr container ID-to-checkout marker so tab removal closes the
right container; no JJ workspace state is duplicated.

## Install

Requirements: `cargo`, `jj`, and Herdr 0.7.0 or newer.

```sh
herdr plugin install EzraCerpac/jj-waltz/plugins/herdr
```

For local development:

```sh
sh plugins/herdr/scripts/build.sh
herdr plugin link plugins/herdr
```

## Keybindings

Add any bindings you want to the Herdr config:

```toml
[[keys.command]]
key = "prefix+a"
type = "plugin_action"
command = "ezracerpac.jj-waltz.new"
description = "new jw workspace"

[[keys.command]]
key = "prefix+shift+a"
type = "plugin_action"
command = "ezracerpac.jj-waltz.new-tab"
description = "new jw workspace in tab"

[[keys.command]]
key = "prefix+d"
type = "plugin_action"
command = "ezracerpac.jj-waltz.remove"
description = "remove jw workspace"
```

`remove` asks for confirmation, refuses the default JJ workspace, delegates deletion
to `jw remove`, then closes the containing Herdr workspace or tab. Bookmarks follow
normal `jw remove` behavior.
