<!-- 本文件只存在于 personal fork 的 local 分支，不属于上游 wezterm -->

# 关键词高亮：使用手册（速查）

> 功能设计/实现说明见同目录 [`keyword-highlight.md`](keyword-highlight.md)。
> 本文件只讲**怎么配**，照抄即可用。

## 1. 30 秒上手

在 `~/.config/wezterm/wezterm.lua`（本机实际是 `/data/baili.work/tpl/conf/wezterm/wezterm.lua`）里加：

```lua
config.keyword_rules = {
  { regex = '(?i)\\berror\\b', fg = '#ff5f5f', bold = true },
  { regex = '(?i)\\bwarn(ing)?\\b', fg = '#ffd75f' },
  { regex = '(?i)\\bok\\b', fg = '#5fd75f' },
}
```

保存后 `Ctrl+Shift+R` 热重载，立刻生效。**不写 `keyword_rules` 就用内置的 8 条预置规则**
（error / warning / success / info / ipv4 / mac / url / k8s-status），默认开启。

## 2. 规则字段

每条规则是一个 Lua table：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `regex` | string | ✅ | 匹配正则。支持 fancy-regex：lookaround `(?=)` `(?<=)`、反向引用 `\1`。只用 `\b` 等普通语法时自动走 regex crate 快引擎；见第 9 节 |
| `fg` | 颜色 | | 前景色 |
| `bg` | 颜色 | | 背景色 |
| `bold` | bool | | 加粗（默认 false） |
| `italic` | bool | | 斜体（默认 false） |
| `underline` | bool | | 下划线（默认 false） |
| `name` | string | | 规则名，用于按名删除/改写内置规则 |

**颜色写法**（都能用）：
`'#ff5f5f'`、`'#f55'`、`'#ff5f5fcc'`（带透明度）、`'red'`、`'Orange'`、`'hsl:20 100 50'`、`'rgb:255 95 95'`

**没配的字段保持原样**：只写 `bold = true` 就只加粗、不改颜色。

**优先级**：列表里**后面的规则覆盖前面的**。

## 3. 内置预置规则一览

| `name` | 颜色 | 匹配内容 |
|---|---|---|
| `error` | 红 `#ff5f5f` + 粗 | fail/failed/failure、error、fatal、panic、exception、traceback、denied、refused、not found、segmentation fault、core dumped、abort、critical、invalid、unsupported、cannot、unable to、out of memory、oom、no space left、connection reset、unreachable、killed、错误、失败、异常、致命 |
| `warning` | 黄 `#ffd75f` | warn/warning、caution、deprecated、timeout、timed out、retry、too many、degraded、unhealthy、backoff、restart、probe failed、not ready、警告、注意、超时 |
| `success` | 绿 `#5fd75f` | ok、okay、success、succeeded、done、pass(ed)、complete(d)、healthy、active (running)、listening on、成功、已完成 |
| `info` | 青 `#5fd7ff` | info、notice、starting、started、loading、loaded、creating、created、building、built、checking、launching、connected to、信息、开始 |
| `ipv4` | 紫 `#d787ff` | `10.0.0.12` 这类 IPv4 |
| `mac` | 紫 `#d787ff` | `00:1a:2b:3c:4d:5e` |
| `url` | 仅下划线 | `http(s)://...` |
| `k8s-status` | 红 + 粗 | ErrImagePull、ImagePullBackOff、CrashLoopBackOff、OOMKilled、Evicted、Pending、Terminating |

## 4. 删除 / 改写 / 追加内置规则

```lua
local rules = wezterm.default_keyword_rules()   -- 拿到内置规则的副本

for i = #rules, 1, -1 do
  if rules[i].name == 'ipv4' then
    table.remove(rules, i)                 -- 删除：不再高亮 IP
  elseif rules[i].name == 'warning' then
    rules[i].fg = '#ffa500'                -- 改写：警告改成橙色
  elseif rules[i].name == 'success' then
    rules[i].bg = '#0a2a0a'                -- 改写：成功加底色
  end
end

table.insert(rules, {                      -- 追加自定义
  name = 'my-k8s',
  regex = '\\b(ErrImagePull|CrashLoopBackOff)\\b',
  fg = '#ff5f5f',
  bold = true,
})

config.keyword_rules = rules
```

## 5. 开关

- 内置默认键：`Ctrl+Shift+H`（按**窗口**独立，切走再切回窗口仍保持）
- 命令面板 `Ctrl+Shift+P` 搜 “keyword” 也能切
- 想让某个键开/关，自己绑（用户绑定优先）：

```lua
config.keys = {
  { key = 'H', mods = 'CTRL|SHIFT', action = wezterm.action.ToggleKeywordHighlight },
}
```

- 默认从关到开由配置决定，**运行期切换不会被无关的热重载重置**（只有改了
  `config.keyword_highlight_enabled` 才会重新播种）：

```lua
config.keyword_highlight_enabled = false   -- 新窗口默认关，用快捷键按需开
```

## 6. 常用配方

**日志级别（syslog / journald）**

```lua
config.keyword_rules = {
  { regex = '\\bEMERG\\b|\\bALERT\\b|\\bCRIT\\b', fg = '#ff0000', bold = true },
  { regex = '\\bERR(OR)?\\b', fg = '#ff5f5f' },
  { regex = '\\bWARN(ING)?\\b', fg = '#ffd75f' },
  { regex = '\\bNOTICE\\b', fg = '#5fd7ff' },
  { regex = '\\bINFO\\b', fg = '#87d787' },
  { regex = '\\bDEBUG\\b', fg = '#808080' },
}
```

**只看自己关心的告警词**

```lua
config.keyword_rules = {
  { regex = '(?i)\\b(oom|segfault|core dumped|no space left|too many open files)\\b',
    fg = '#ffffff', bg = '#aa0000', bold = true },
}
```

**k8s 对象名 / 命名空间**

Pod 名的随机后缀由 k8s 的 `util/rand.SafeEncodeString` 生成，字母表固定为
`bcdfghjklmnpqrstvwxz2456789`（**没有元音**，也剔除了 `l`/`o`/`u`/`y`）。
用这个字符集做判据，才能把真 pod 名和 `ibm-spectrum-scale` 这类产品名区分开：
后者 `spectrum`(8 位) + `scale`(5 位) 的形状与 ReplicaSet 哈希 + 随机后缀完全一样。

```lua
-- 后缀 5~10 位，只含「辅音+数字」且至少一个辅音；英文词天然不匹配
local C = 'bcdfghjklmnpqrstvwxz'
local parts = {}
for pos = 0, 5 do                       -- 首个辅音出现在第 pos 位
  local lo, hi = math.max(0, 4 - pos), 9 - pos
  if hi >= lo then
    parts[#parts + 1] = (pos == 0 and '' or ('[%s0-9]{%d}'):format(C, pos))
      .. '[' .. C .. ']' .. ('[%s0-9]{%d,%d}'):format(C, lo, hi)
  end
end
local POD = '\\b[a-z0-9]+(?:-[a-z0-9]+)*-(?:' .. table.concat(parts, '|') .. ')\\b'

{ regex = POD, fg = '#87d7ff' },                      -- pod 名
{ regex = '\\b(namespace|ns)[=: ]+[a-z0-9-]+', fg = '#d7af87' },
```

> 别用 `[a-z0-9]{8,10}-[a-z0-9]{5}` 这种「形状匹配」：它漏掉
> `kube-proxy-9v556` / `pod-2r3shd8`（没有哈希段），又误报 `ibm-spectrum-scale`。

**AI 训练任务：loss / step / OOM**

```lua
{ regex = '(?i)\\b(nan|inf|loss=\\s*nan)\\b', fg = '#ffffff', bg = '#cc0000', bold = true },
{ regex = '(?i)\\b(step|iter|epoch)\\s*[=:]?\\s*\\d+', fg = '#5fd7ff' },
{ regex = '(?i)\\b(CUDA out of memory|NCCL.*(timeout|error))\\b', fg = '#ff5f5f', bold = true },
```

**IP / 端口 / URL 一眼分清**

```lua
{ regex = '\\b(?:\\d{1,3}\\.){3}\\d{1,3}\\b', fg = '#d787ff' },
{ regex = ':(\\d{2,5})\\b', fg = '#d7af87' },
{ regex = 'https?://[^\\s\'"]+', underline = true },
```

> 以上配方里的 22 条正则都已用 `fancy-regex 0.14` 实测可编译；第 8 节的完整配置
> 也在 Xvfb 里跑过：默认规则 + 自定义 k8s/CUDA 规则同时生效，`bg` 与 `bold` 正常。

## 7. 排错

| 现象 | 原因 / 处理 |
|---|---|
| 完全不生效 | 开个新窗口（老窗口若被 `keyword_highlight_enabled = false` 建的就默认关）；或按 `Ctrl+Shift+H` 看一眼；`wezterm show-keys --lua \| grep -i keyword` 确认绑定存在 |
| 正则报错、配置加载失败 | Lua 字符串里的反斜杠要写两个：`'\\berror\\b'`。用 `[[...]]` 长字符串可少转义一层：`[[\berror\b]]` |
| 匹配过头（比如路径里的 ok） | 加词边界 `\b`，或收窄成 `\bOK\b`、`(?i)\b(ok\|success)\b` |
| copy mode / 搜索里没有高亮 | 设计如此：这两个覆盖层有自己的配色，关键词高亮会让位 |
| 改了规则但没变 | 配置热重载 `Ctrl+Shift+R`；若改的是 `keyword_rules`，重载即生效 |
| 怀疑影响性能 | 只在画面变化时对“要重绘的行”跑正则，静态画面零开销；规则多时注意 `\b`/lookaround 的开销，见第 9 节 |

## 8. 完整可粘贴配置

```lua
local wezterm = require 'wezterm'
local config = wezterm.config_builder()

config.keyword_highlight_enabled = true

local rules = wezterm.default_keyword_rules()
-- 去掉不关心的
for i = #rules, 1, -1 do
  if rules[i].name == 'mac' then table.remove(rules, i) end
end
-- 追加自己的
table.insert(rules, {
  regex = '(?i)\\b(oomkilled|crashloopbackoff|errimagepull)\\b',
  fg = '#ffffff', bg = '#aa0000', bold = true,
})
table.insert(rules, {
  regex = '(?i)\\b(cuda out of memory|nccl.*timeout)\\b',
  fg = '#ff5f5f', bold = true,
})
config.keyword_rules = rules

config.keys = {
  { key = 'H', mods = 'CTRL|SHIFT', action = wezterm.action.ToggleKeywordHighlight },
}

return config
```

## 9. 性能：双引擎与规则规模

每条规则在加载时编译两次：`fancy-regex`（保底，支持 lookaround/反向引用）和
`regex` crate（DFA，快）。**能被 regex crate 编译的规则自动走快引擎**，
否则退回 fancy-regex，语义完全不变。

想确认哪些规则走了慢引擎（调试时很有用）：

```bash
WEZTERM_LOG=config=debug wezterm ls-fonts 2>&1 | grep "stays on fancy-regex"
```

实测（60 行混合日志一屏全量重绘，release 构建）：

| 规则集 | 改造前（全 fancy） | 双引擎后 |
|---|---|---|
| 内置 8 条 | 8.3 ms | 0.1 ms |
| 内置 8 + 扩展 99 条 | **89 ms** | **9.0 ms** |

99 条扩展里 94 条走快引擎，13 条留在 fancy（都是用了 lookbehind 的：
`x-cmd`、`x-path-*`、`x-flag`、`x-port`、`x-version`、`x-ls-perm`、`x-redirect`、
`x-prompt`、`x-sudo-hint`、`x-hex`、`x-ipv6`、`x-env-name`），其中 `x-cmd` 一条
就占慢引擎成本的一半。**规则数量本身不是问题，`\b` 和 lookaround 才是**：
同一个交替 `(?i)(?:starting|started|…)` 是 0.06 µs，加上 `\b` 变 8.29 µs。

自己写规则时的建议：

- 能用 `\bword\b` 别用 lookbehind（`\b` 现在也会走快引擎）。
- 确实需要 lookbehind 时，尽量让词足够独特（长词），并控制条数。
- 一条超长交替（几十个词）比拆成几条短规则更划算，但要留意灾难性回溯。
