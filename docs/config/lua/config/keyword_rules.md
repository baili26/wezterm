---
tags:
  - keyword
---
# `keyword_rules`

Defines rules that recolor matches in the terminal output as it is rendered.
This is an opt-in personal-fork feature; it is *not* part of upstream wezterm.

The value is a list of rule entries. Each entry has the following fields:

* `regex` - the regular expression to match against each line of output.
  Supports [Fancy Regex Syntax](https://docs.rs/fancy-regex/latest/fancy_regex/#syntax)
  (look-around and backreferences) on top of the usual
  [Regex syntax](https://docs.rs/regex/latest/regex/#syntax).
* `fg` - optional foreground color. Accepts `#rgb`/`#rrggbb`/`#rrggbbaa`,
  CSS color names (`Orange`, `red`, ...) and `hsl:`/`rgb:` forms.
* `bg` - optional background color, same syntax as `fg`.
* `bold`, `italic`, `underline` - optional booleans; default `false`.
* `name` - optional string label, handy when you want to remove or replace a
  built-in rule from Lua.

Colors and styles that a rule does *not* set are left as the program drew
them, so a rule with only `bold = true` adds emphasis without changing any
color.

Rules are applied in the order given: if two rules match the same cell, the
one that appears later in the list wins.

Matching happens at paint time against a copy of the line, so it applies to
both the live screen and the scrollback, and it never changes what copy/paste
or the search function sees.

Assigning `keyword_rules` replaces the built-in defaults.  The built-in
defaults can be retrieved with
[`wezterm.default_keyword_rules()`](../wezterm/default_keyword_rules.md),
which lets you remove or tweak individual entries and append your own:

```lua
local wezterm = require 'wezterm'
local config = wezterm.config_builder()

local rules = wezterm.default_keyword_rules()
for i = #rules, 1, -1 do
  if rules[i].name == 'ipv4' then
    table.remove(rules, i)          -- drop a built-in rule
  elseif rules[i].name == 'warning' then
    rules[i].fg = '#ffa500'         -- recolor a built-in rule
  end
end
table.insert(rules, {               -- add your own rule
  name = 'k8s',
  regex = '\\b(ErrImagePull|CrashLoopBackOff)\\b',
  fg = '#ff5f5f',
  bold = true,
})

config.keyword_rules = rules
return config
```

To write a fully custom set instead:

```lua
config.keyword_rules = {
  { regex = '(?i)\\b(FATAL|CRITICAL)\\b', fg = '#ff0000', bold = true },
  { regex = '\\bTODO\\b', fg = '#000000', bg = '#ffd75f' },
}
```

Keyword highlighting can be toggled at runtime, per window, with
[`ToggleKeywordHighlight`](../keyassignment/ToggleKeywordHighlight.md)
(`CTRL+SHIFT+H` by default).

See also: [`keyword_highlight_enabled`](keyword_highlight_enabled.md).
