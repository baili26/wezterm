<!-- 本文件只存在于 personal fork 的 local 分支，不属于上游 wezterm -->

# Snippet 快捷输入：设计与实现选项

> 结论先行：**"填内容"这一步必须发生在真正的终端里，不能放在 modal 面板里。**
> 这直接决定了架构。本文所有可行/不可行的判断都在本机实测过。

---

## 1. 为什么"悬浮面板里填"行不通（你的直觉是对的）

你提的第一版设计是：面板里显示模板，在面板里把 `{namespace}` 填完再回显到终端。
问题是你说的那条——**按 Tab 会触发 shell/vim 自己的补全**，而这正是我们要的能力
（`kubectl -n <Tab>` 列出 namespace、`kubectl get pod <Tab>` 补 pod 名）。

实测确认（Xvfb + 真实 wezterm）：

- `wezterm` 的 Modal（`termwindow/modal.rs` 的 `Modal` trait，CommandPalette /
  CharSelector / PaneSelector 都是它）**拦截所有按键**。`keyevent.rs:273` 里
  modal 的 `key_down` 返回 `true` 就直接 `return`，按键永远不会到 pane。
- 所以只要面板还在，Tab 就只是面板的一个按键，不会有 shell 补全。

结论：**面板只能用来"选"，不能用来"填"。**
选完之后必须把控制权交还给终端，让用户在原生的命令行上填。

---

## 2. 两种取用方式

### 2.1 复用命令面板（零代码，推荐先用这个）

`augment-command-palette` 是同步钩子，返回的每个条目都会出现在
`Ctrl+Shift+P` 的面板里，带 fuzzy 搜索（实测 `type 'pod detail'` → 只剩一条）。

```lua
wezterm.on('augment-command-palette', function(window, pane)
  return {
    { brief = 'snippet: kubectl pod 详情',
      doc = '宽格式列出某 namespace 的 pod',
      icon = 'cod_symbol_snippet',
      action = act.EmitEvent 'snip-start-pod-detail' },
  }
end)
```

> ⚠️ **必须用 `act.EmitEvent`，不能用 `wezterm.action_callback`。**
> 实测两次：`EmitEvent` 选中后处理器正常执行；`action_callback` 选中后回调
> 完全没跑（无日志、无报错）。`action_callback` 本身也是转成
> `EmitEvent('user-defined-N')`（`config/src/lua.rs:659`），但在面板这条路径上
> 没走通，所以统一写命名事件。

优点：0 行 Rust，立刻能用；fuzzy 搜索、`doc` 字段展示都由现成代码提供。
缺点：面板是"命令列表"的视觉，不是专门的 snippet 浏览器；每次要 `Ctrl+Shift+P` 再打字。

### 2.2 专用悬浮面板（需要写代码）

如果嫌命令面板里混杂着其它命令，就做一个真正的 `Modal`：

- 骨架抄 `wezterm-gui/src/termwindow/palette.rs`:576 的 `impl Modal for CommandPalette`；
- 列表渲染抄 `palette.rs:445-465`（居中 + `Element::float` + `min_width`）；
- 只需支持：输入过滤、↑↓ 选择、Enter 选中、Esc 取消；
- **选中后 `cancel_modal()`，再 `pane:send_paste(第一段)`**，然后一切照旧。

关键点：这个面板**只做选择**，不做填充。填充仍在终端里。
所以它其实是"一个只能选 snippet 的命令面板"，工程量约 1-2 天，收益只是视觉区隔。
**建议：先不做，把 2.1 用起来，用久了确实嫌吵再说。**

---

## 3. 核心机制：`?` 光标位怎么实现

### 3.1 唯一稳妥的做法：把模板按 `?` 切段，逐段追加

不要试图"先贴整条命令再把光标挪到某个位置"——我试过，不可靠：

| 方案 | 实测结果 |
|---|---|
| 贴整条 → `Ctrl-E` 到行尾 → `Ctrl-B` × N 左移 | 第二个位点对得上（want=22 now=22），**第一个位点对不上**（want=12 → now=18）——readline 不允许把光标移到输入区起点之前（提示符占 18 列） |
| 贴整条 → 计算列号 → 用 `pane:get_cursor_position()` 校验 | 同上，且只对 readline 类程序成立；vim / less / python REPL 里 `Ctrl-E` 含义完全不同 |

改成**按段追加**就完全没有光标数学：

```
模板:  kubectl -n ? get pod ? -o wide
切段:  ["kubectl -n ", " get pod ", " -o wide"]
```

1. 贴第 1 段 → 光标自然停在第一个缺口（`kubectl -n ` 之后）；
2. 用户输入 `prod-ns`，或者按 Tab 让 shell 补全 namespace；
3. 用户按**推进键**，贴第 2 段 → 光标落在第二个缺口；
4. 再按一次推进键，贴第 3 段，完成。

**实测完整链路**（Xvfb，真实 bash）：

```
log:  START 1[kubectl -n ]
      用户输入 prod-ns
      NEXT 2[ get pod ]
      用户输入 nginx-abc
      NEXT 3[ -o wide]
读取该行:  ROW[baili@BAILI-PC:~$ kubectl -n prod-ns get pod nginx-abc -o wide]
```

——拼出来完全正确，中间两次 Tab/输入都没被 wezterm 吃掉。

多行模板（YAML）同理，并且**实测可用**：

```
模板:  cat > out.yaml <<EOF\napiVersion: v1\nkind: Pod\nEOF
```
`send_paste` 发 heredoc（bracketed paste 会把换行原样保留，不会提前执行），
用户按回车 → 文件内容正确写出。实测 `/tmp/ml/out.yaml` 内容为
`apiVersion: v1 / kind: Pod`。

### 3.2 推进键选哪个

要求：① 不与 bash readline 常用绑定冲突；② 不与 wezterm 默认绑定冲突；
③ 只按一次（不要 Tab，Tab 要留给 shell）。

**选 `CTRL|ALT+J`（下一个）/ `CTRL|ALT+K`（上一个）：**

- wezterm 默认键里 `CTRL+ALT` 组合**完全没有占用**
  （`grep 'Modifiers::CTRL.union(Modifiers::ALT)' wezterm-gui/src/commands.rs` → 无结果）；
- bash readline 的 `\e\C-j`、`\e\C-k` **都没有绑定**（`bind -p` 里查不到）；
- 在 wezterm 里是全局绑定，命中就 `EmitEvent`，根本不会送到 shell，
  所以哪怕目标程序是 vim 也不会被影响（**但这也意味着它会抢走 vim 的 Ctrl+Alt+J，
  这一点要接受**）。

不想占 `J/K` 的话，`CTRL|ALT+Enter` 或 `F2` 也行；`F2` 在 wezterm 默认键里也空着。

---

## 4. ⚠️ 不要用 key_tables 做状态机

一开始想用 `ActivateKeyTable { one_shot = false }` + `PopKeyTable` 做"snippet 模式"，
因为这样能优雅地把 `J` 只在这个模式下改掉。**实测非常不稳**：

```
# 默认配置：激活表后再按 J
Char('k') ALT|CTRL -> perform ActivateKeyTable { name: "sn" }
（然后按 j，没有任何 table:sn 命中，按键直接进了 shell）

# 把 status_update_interval 调到 600000 后
Char('k') ALT|CTRL -> perform ActivateKeyTable { name: "sn" }
table:sn Char('j') NONE -> perform EmitEvent("hit")     ← 生效了
```

原因是 `TermWindow::config_was_reloaded()` 里有 `self.key_table_state.clear_stack()`
（`wezterm-gui/src/termwindow/mod.rs:1740`），而 `config_was_reloaded()` 会被
外观变化、`set_config_overrides`、tab bar 显隐等多条路径触发（这个坑我们之前
为关键词高亮的开关踩过一次）。**状态被无声清空，故障表现为"按键没反应"，
极难查。**

**所以：用普通全局绑定 + `wezterm.GLOBAL` 存当前 snippet 状态。**

```lua
config.keys = {
  { key = 'j', mods = 'CTRL|ALT', action = act.EmitEvent 'snip-next' },
}
```

`wezterm.GLOBAL` 跨配置重载保留（`docs/config/lua/wezterm/GLOBAL.md`），
实测存嵌套表 `{ i = 1, parts = {...} }` 也正常。

---

## 5. 模板怎么管理（你说"放 Lua 里不合适"——同意）

YAML 里带 `{{ }}`、`:`、`#`、长缩进，塞进 Lua 字符串要么用 `[=[ ]=]`、
要么层层转义 `?`，写起来和改起来都痛苦。改成**一个目录、一个文件一条**。

```
~/wezterm-snippets/
├── k8s/
│   ├── pod-detail.snip
│   └── deploy.yaml
└── ops/
    └── ss-listen.snip
```

`pod-detail.snip`：

```
---
title: 查看 pod 详情
desc: 宽格式列出某命名空间的 pod
tags: [k8s, pod]
---
kubectl -n ? get pod ? -o wide
```

`deploy.yaml`（连 YAML 片段本身也能当模板，写 `?` 只是一个占位）：

```
---
title: Deployment 骨架
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: ?-deploy
spec:
  replicas: ?   # 改成 2 再回车
```

### 5.1 加载代码（实测可跑）

```lua
local SNIP_DIR = wezterm.home_dir .. '/wezterm-snippets'

local function split_on_mark(tpl)          -- '?' 切段；'\0' 可作字面 '?'
  local parts, cur = {}, {}
  for i = 1, #tpl do
    local ch = tpl:sub(i, i)
    if ch == '?' then
      table.insert(parts, table.concat(cur)); cur = {}
    else
      table.insert(cur, ch)
    end
  end
  table.insert(parts, table.concat(cur))
  return parts
end

local function load_snippets()
  local out = {}
  -- ⚠️ 必须用相对模式 + relative_to，见下方"坑"
  local files = wezterm.glob('**/*', SNIP_DIR)
  table.sort(files)
  for _, rel in ipairs(files) do
    local fh = io.open(SNIP_DIR .. '/' .. rel, 'r')
    local body = fh and fh:read('*a') or nil
    if fh then fh:close() end
    if body then
      local meta, text = body:match('^%-%-%-%s*\n(.-)\n%-%-%-%s*\n(.*)$')
      local title, desc = rel, ''
      if meta then
        local ok, m = pcall(wezterm.serde.yaml_decode, meta)
        if ok and type(m) == 'table' then
          title = m.title or rel
          desc  = m.desc or ''
        end
      else
        text = body
      end
      if text and #text > 0 then
        table.insert(out, { title = title, desc = desc, path = rel,
                            parts = split_on_mark(text) })
      end
    end
  end
  return out
end
```

实测输出（目录里两个 k8s 文件 + 一个 net 文件，含子目录）：

```
SKIP (dir or unreadable): k8s
SNIP k8s/deploy.yaml      title=(no front matter) stops=1
SNIP k8s/pod-detail.snip  title=查看 pod 详情        stops=2
SKIP (dir or unreadable): net
SNIP net/ss-listen.snip   title=(no front matter) stops=1
```

注意 `glob` 也会返回**目录**，`io.open` 打开目录会返回 `nil`，所以 `SKIP` 是正常的
（这也是子目录不需要额外过滤的原因）。

### 5.2 ⚠️ 坑：`wezterm.glob` 的绝对路径 `**` 会 panic

```
wezterm.glob('/tmp/x/**/*')      --> panic: walk is always relative to walker.root: StripPrefixError(())
wezterm.glob('**/*', '/tmp/x')   --> 正常，返回相对路径
```

这是上游 `filenamegen 0.2.7` 的 bug（`recursivewalker.rs:58`）。
**必须用 `wezterm.glob('**/*', SNIP_DIR)` 这种"相对模式 + relative_to"的形式。**
（`relative_to` 会顺手把前缀去掉，所以后面拼路径要再补上 `SNIP_DIR`。）

### 5.3 改完模板想立刻生效

```lua
wezterm.add_to_config_reload_watch_list(SNIP_DIR)
```

实测：新增一个 `.snip` 文件后，配置被重新求值、`glob` 计数从 1 变 2。
（`require` 一个 Lua 文件会隐式加进 watch list，但普通目录不会，要显式加。）

> ⚠️ **配置会被非常频繁地重新求值**（实测启动阶段 Lua 配置执行了几十次）。
> 所以 `load_snippets()` 必须廉价：只 `glob` + 读文件，不要在里面调
> `run_child_process`、不要递归扫大目录。几十个小文件完全没问题。

---

## 6. 完整可用版本（实测跑通）

```lua
local wezterm = require 'wezterm'
local act = wezterm.action

local SNIP_DIR = wezterm.home_dir .. '/wezterm-snippets'
local LOG = nil  -- 调试时设成 '/tmp/snip.log'

local function log(m)
  if not LOG then return end
  local f = io.open(LOG, 'a')
  if f then f:write(m .. '\n') f:close() end
end

local function split_on_mark(tpl) ... end        -- 见 5.1
local SNIPS = load_snippets()                    -- 见 5.1

-- 每条 snippet 一个固定名字的事件（配置加载时就注册好）
for _, s in ipairs(SNIPS) do
  wezterm.on('snip-start-' .. s.path, function(window, pane)
    wezterm.GLOBAL.snip = { id = s.path, i = 1, parts = s.parts }
    pane:send_paste(s.parts[1])
    log('START ' .. s.path .. ' parts=' .. #s.parts)
  end)
end

wezterm.on('snip-next', function(window, pane)
  local st = wezterm.GLOBAL.snip
  if not st then
    window:toast_notification('wezterm', '没有正在填写的 snippet', nil, 1500)
    return
  end
  local n = st.i + 1
  if n > #st.parts then
    wezterm.GLOBAL.snip = nil
    -- 全部填完，光标在行尾，用户自己按回车执行
    log('DONE ' .. st.id)
    return
  end
  st.i = n
  wezterm.GLOBAL.snip = st
  pane:send_paste(st.parts[n])
  log('NEXT ' .. n .. ' [' .. st.parts[n]:gsub('\n', '\\n') .. ']')
end)

wezterm.on('snip-abort', function(window, pane)
  wezterm.GLOBAL.snip = nil
end)

wezterm.on('augment-command-palette', function(window, pane)
  local items = {}
  for _, s in ipairs(SNIPS) do
    table.insert(items, {
      brief  = 'snippet: ' .. s.title,
      doc    = s.desc .. '  (' .. s.path .. ')',
      icon   = 'cod_symbol_snippet',
      action = act.EmitEvent('snip-start-' .. s.path),
    })
  end
  return items
end)

wezterm.add_to_config_reload_watch_list(SNIP_DIR)

return {
  keys = {
    { key = 'j', mods = 'CTRL|ALT', action = act.EmitEvent 'snip-next' },
    { key = 'k', mods = 'CTRL|ALT', action = act.EmitEvent 'snip-abort' },
  },
}
```

用法：`Ctrl+Shift+P` → 打 `snippet: pod` → Enter → 模板已贴好、光标在第一个缺口 →
Tab 让 shell 补全 / 手打 → `Ctrl+Alt+J` → 下一个缺口 → … → 回车执行。

---

## 7. 已知限制（诚实清单）

| 限制 | 说明 |
|---|---|
| 只能**往后**填，不能回头改 | 因为用的是"追加"，第 n 段的推进不可逆。填错了就 `Ctrl+U`/`Ctrl+C` 重来，或按 `Ctrl+Alt+K` 清状态 |
| `?` 是保留字 | 模板里真要一个 `?`（比如 `grep -E '?foo'`），暂时无转义。可改成用 `\0` 或 `%%` 当标记，改一行 `split_on_mark` 即可 |
| 推进键是全局的 | `Ctrl+Alt+J` 在任何程序里都会被 wezterm 拦下。若某程序确实用它，可以改成 `F2` 或只在有 snippet 时**有条件转发**（`perform_action(SendKey{...})`，但这条我没完整验证，见下） |
| 多行模板最后要用户自己按回车 | 这是刻意的：不自动执行，避免 `kubectl delete` 之类误触 |
| 面板样式不能改 | 阶段一（复用命令面板）的样式由 `command_palette_*` 配置控制，和普通命令混在一起 |
| 如果真做了专用 Modal | 它一样不能承担"填内容"，只能改选择体验 |

关于"有条件转发推进键"：理论上当 `wezterm.GLOBAL.snip == nil` 时在 handler 里
`window:perform_action(act.SendKey{ key='j', mods='CTRL|ALT' }, pane)` 就能把按键还回去。
我在测试里写了这段但**没有验证转发后目标程序是否真的收到**，
所以列在这里作为可选优化，不写进推荐版本。

---

## 8. 和"告警"的关系

完全不相关，两套独立机制。但有一个共同的前提事实：**Pane 输出在 Lua 侧是能读的**
（`pane:get_lines_as_text` / `get_semantic_zones` / `get_text_from_region` 都实测可用，
包括**非活动 pane**），这既是 snippet 取上下文的基础，也是纯 Lua 告警轮询的基础。
告警的设计见 `alerts.md`。
