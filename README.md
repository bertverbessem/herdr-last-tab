# herdr-last-tab

Toggle between the current and previously focused tab within a [Herdr](https://herdr.dev) workspace — the `last-window` experience from tmux.

Tab history is tracked **per workspace**, so switching workspaces won't mess up your tab toggle.

## Install

Add to your `plugins.txt`:

```
bertverbessem/herdr-last-tab
```

Then bind the toggle action in `config.toml`:

```toml
[[keys.command]]
key = "ctrl+comma"
type = "plugin_action"
command = "bertverbessem.last-tab.toggle"
description = "last tab"
```

## How it works

The plugin subscribes to `tab.focused` and `tab.closed` events. Each time you switch tabs, it remembers the previous one. The toggle action switches back to it.

## Credits

Based on [third774/herdr-last-workspace](https://github.com/third774/herdr-last-workspace) — thanks @third774 for the clean plugin architecture that made this a smooth adaptation.

Scaffolded by [Claude Code](https://docs.anthropic.com/en/docs/claude-code) (claude-opus-4-6).
