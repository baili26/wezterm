---
tags:
  - keyword
---
# `ToggleKeywordHighlight`

Toggles [keyword highlighting](../../config/keyword_rules.md) for the current
window.

This is a personal-fork feature; it is not part of upstream wezterm.

The state is tracked per window and starts from
[`keyword_highlight_enabled`](../../config/keyword_highlight_enabled.md) when
the window is created.  Reloading the configuration resets it back to that
config value.

```lua
local wezterm = require 'wezterm'
local act = wezterm.action
local config = wezterm.config_builder()

config.keys = {
  -- This is the built-in default binding, shown here for reference
  { key = 'H', mods = 'CTRL|SHIFT', action = act.ToggleKeywordHighlight },
}

return config
```

{{since('20261113-local', inline=True)}}
