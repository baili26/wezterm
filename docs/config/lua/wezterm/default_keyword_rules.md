---
tags:
  - keyword
---
# `wezterm.default_keyword_rules()`

Returns the compiled-in default values for [keyword_rules](../config/keyword_rules.md).

This is a personal-fork feature; it is not part of upstream wezterm.

The returned list can be modified and reassigned, which makes it easy to drop,
recolor or extend individual built-in rules:

```lua
local wezterm = require 'wezterm'
local config = wezterm.config_builder()

local rules = wezterm.default_keyword_rules()
for i = #rules, 1, -1 do
  if rules[i].name == 'mac' then
    table.remove(rules, i)
  end
end
table.insert(rules, { regex = '\\bTODO\\b', fg = '#ffd75f' })

config.keyword_rules = rules
return config
```
