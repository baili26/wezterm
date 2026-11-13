---
tags:
  - keyword
---
# `keyword_highlight_enabled`

Whether [keyword highlighting](keyword_rules.md) is enabled when a new window
is created. Defaults to `true`.

This is a personal-fork feature; it is not part of upstream wezterm.

The value is applied when a window is created.  While running, the setting can
be toggled per window with
[`ToggleKeywordHighlight`](../keyassignment/ToggleKeywordHighlight.md)
(`CTRL+SHIFT+H` by default); reloading the configuration resets each window
back to this value.

```lua
local wezterm = require 'wezterm'
local config = wezterm.config_builder()

-- Start with highlighting off; toggle it on with CTRL+SHIFT+H when wanted
config.keyword_highlight_enabled = false

return config
```
