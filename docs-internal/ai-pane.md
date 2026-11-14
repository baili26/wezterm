<!-- 本文件只存在于 personal fork 的 local 分支，不属于上游 wezterm -->

# L2：AI 作为独立 pane，wezterm 只负责"喂上下文"

零 Rust 改动，纯 Lua + `wezterm cli`。本文每一处 API 都在本机 wezterm（local 分支）
里实测过，标注了验证方式。

---

## 0. 为什么是 "AI 在自己的 pane 里"

不要在 Lua 里 `curl` 大模型 API 然后弹 toast。理由：

| 在 Lua 里调 API | AI 在 pane 里 |
|---|---|
| 没有流式输出，要等整段返回 | 流式、可中断、可翻历史 |
| 多轮对话状态要自己存 | 对话状态在进程里，白送 |
| 要处理 token/鉴权/超时 | 复用 `claude` / `aichat` / `ollama run` 现成能力 |
| 输出不能搜索/复制 | 就是终端内容，copy mode、QuickSelect 全可用 |

所以 wezterm 的职责只有一件事：**把"我在看的这段"塞进 AI pane 的 stdin**。

---

## 1. 骨架：开一个 AI pane 并记住它

```lua
local wezterm = require 'wezterm'
local act = wezterm.action

local function ai_key(window)
  return 'ai_pane_' .. window:window_id()
end

local function get_ai_pane(window)
  local id = wezterm.GLOBAL[ai_key(window)]
  if not id then return nil end
  -- 重启/pane 被杀后 id 会失效，必须校验
  local ok, pane = pcall(wezterm.mux.get_pane, id)
  if ok and pane then return pane end
  wezterm.GLOBAL[ai_key(window)] = nil
  return nil
end

wezterm.on('open-ai', function(window, pane)
  local existing = get_ai_pane(window)
  if existing then
    existing:activate()      -- 聚焦该 pane 及其所在 tab
    return
  end
  local right = pane:split {
    direction = 'Right',
    size = 0.35,
    args = { 'bash', '-lc', 'exec claude' },   -- 换成 aichat / ollama 都可以
  }
  wezterm.GLOBAL[ai_key(window)] = right:pane_id()
end)
```

要点：

- `pane:split` 是 **async** 方法（`lua-api-crates/mux/src/pane.rs:96` 的
  `add_async_method`），所以在 `wezterm.on` 回调里可以直接用；它会返回新 Pane 对象，
  `:pane_id()` 拿 id。
- `wezterm.GLOBAL` 存的是 json-like 值，**跨配置热重载保留**，但进程重启就没了。
  实测嵌套表也能存取（`{name=..., stops={...}}`）。
- 想激活已存在的 AI pane，用 `pane:activate()`
  （`docs/config/lua/pane/activate.md`，会同时激活所在 tab）。
  只想按方向切，用 `act.ActivatePaneDirection 'Right'`。注意 **没有**
  `act.ActivatePane { pane = ... }` 这个动作，别写错。

---

## 2. 取上下文：三种粒度，按需要选

### 2.1 上一个命令的输出区（推荐，最干净）

依赖 shell integration（`OSC 133`）。装了之后：

```lua
local function last_output(pane)
  local ok, zones = pcall(function() return pane:get_semantic_zones('Output') end)
  if not ok or not zones or #zones == 0 then return nil end
  local z = zones[#zones]
  local ok2, text = pcall(function() return pane:get_text_from_semantic_zone(z) end)
  if ok2 and text and #text > 0 then return text end
  return nil
end
```

**实测**（Xvfb + 真实 shell integration）：执行 `printf "INFO ok\nERROR boom\n"` 后
`#zones` 有值，取到的是 `INFO ok`（18 字节），不含提示符和命令本身。
——这就是它比"整屏文本"好的地方：**不会把上一条命令、无关输出混进去**。

> 没有 shell integration 时 `get_semantic_zones()` 返回空表，不报错。
> 所以要写 fallback（下一节）。

### 2.2 可见区 / 最后 N 行（无 shell integration 时的 fallback）

```lua
local text = pane:get_lines_as_text(120)   -- 物理行；get_logical_lines_as_text 是按逻辑行
```

注意 `pane:get_lines_as_text(n)` 的 `n` 是**从视口底部往上数**，
`docs/config/lua/pane/get_lines_as_text.md` 有说明。

### 2.3 手动选中的那段（最精准）

```lua
local text = window:get_selection_text_for_pane(pane)
if text and text ~= '' then ... end
```

这三者可以做成一个函数按优先级退化：选中 > Output zone > 可见区。

---

## 3. 喂过去：`send_paste` + **必须等一下**

```lua
wezterm.on('feed-ai', function(window, pane)
  local ai = get_ai_pane(window)
  if not ai then return end
  local text = last_output(pane) or pane:get_lines_as_text(120)
  if #text == 0 then return end
  ai:send_paste('以下是我刚才命令的输出，帮我分析：\n```\n' .. text .. '\n```\n')
  ai:send_paste('\n')     -- 再补一个回车让 AI CLI 提交
end)
```

### ⚠️ 关键坑：新 split 出来的 pane 不能立刻写

刚 `split` 出来的 pane，pty 里的进程还没起来，**立刻 `send_paste` 会丢数据**。

实测对比（同一份配置）：

| 做法 | 结果 |
|---|---|
| `split` 后立刻 `send_paste('CTX:\n'..text)` | AI pane 只收到 `CTX(zone):\nINFO ok` 的第一行，后续丢失 |
| `split` → `wezterm.sleep_ms(600)` → `send_paste` | 完整收到 `HDR` + 整段文本 |

所以：

```lua
local right = pane:split { ... }
wezterm.sleep_ms(600)         -- 等 pty
right:send_paste(...)
```

`wezterm.sleep_ms` 在事件回调里可用（`wezterm.on` 的 handler 是 async 上下文，
`config/src/lua.rs:787` 用的是 `call_async`）。

---

## 4. 大日志：走文件，不要走 send_paste

几千行日志塞进 `send_paste` 会：撑爆 bracketed paste、让 AI CLI 的行编辑卡住、
而且你没法回头核对到底发了什么。

改成本地落盘 + 只把路径告诉 AI：

```lua
local path = os.tmpname()
local f = io.open(path, 'w+')
f:write(text)
f:close()
ai:send_paste('日志已存到 ' .. path .. '，请读这个文件并分析。\n')
```

Lua 的 `io` 库可用（`config/src/lua.rs:212` 用 `Lua::new()`，除 `debug` 外标准库全在）。
**实测** `io.open` 写入、`io.popen` 执行外部命令，在配置加载阶段都正常。

---

## 5. 从外部脚本喂上下文（`wezterm cli`）

不需要 API token，走已有的 unix socket（`WEZTERM_UNIX_SOCKET` 继承给子进程，
`mux/src/domain.rs:479-482`）。

```bash
# 取某 pane 最近 200 行（负数 = 往滚动区里数）
wezterm cli get-text --pane-id 3 --start-line -200 > /tmp/ctx.log

# 塞给 AI pane（TEXT 省略时读 stdin）
wezterm cli send-text --pane-id 7 --no-paste < /tmp/ctx.log
```

以实测的 `wezterm cli send-text --help` 为准，`--no-paste` 是"直接写"而不是
bracketed paste —— 传多行配置给 shell 时要注意这一点（不带 `--no-paste` 更安全）。

开 pane 也可以完全从命令行做：

```bash
wezterm cli split-pane --right --percent 35 -- bash -lc 'exec claude'
# 输出新 pane 的 pane-id
```

这让你能用**外部脚本**（tmux hook、监控脚本、cron）来驱动 AI pane，
wezterm 侧一行配置都不用加。

---

## 6. 在命令面板里挂入口（不用改 Rust）

```lua
wezterm.on('augment-command-palette', function(window, pane)
  return {
    { brief = 'AI: 分析上一条命令的输出', icon = 'md_robot',
      action = act.EmitEvent 'feed-ai' },
    { brief = 'AI: 打开助手 pane',         icon = 'md_robot',
      action = act.EmitEvent 'open-ai' },
  }
end)
```

### ⚠️ 坑：面板条目里的 action 必须用 `EmitEvent`，不要用 `wezterm.action_callback`

实测两次：

- 用 `act.EmitEvent 'picked-pod'` → 选中后事件处理器正常执行（log 有记录）；
- 用 `wezterm.action_callback(fn)` → 选中后 **`fn` 没被执行**（log 为空，也没有报错）。

`action_callback` 本质是 `EmitEvent('user-defined-N')`（`config/src/lua.rs:659`），
在面板路径上没走到。所以统一写成命名事件：

```lua
wezterm.on('feed-ai', function(window, pane) ... end)   -- 处理器
-- 面板里：action = act.EmitEvent 'feed-ai'
```

顺带：面板支持 `doc` 字段，会渲染在条目后面，适合放"这条会发送什么"的说明。

---

## 7. 完整可粘贴版本

```lua
local wezterm = require 'wezterm'
local act = wezterm.action

local function ai_key(window) return 'ai_pane_' .. window:window_id() end

local function get_ai_pane(window)
  local id = wezterm.GLOBAL[ai_key(window)]
  if not id then return nil end
  local ok, p = pcall(wezterm.mux.get_pane, id)
  if ok and p then return p end
  wezterm.GLOBAL[ai_key(window)] = nil
  return nil
end

local function context_for(window, pane)
  -- 1) 手动选中
  local sel = window:get_selection_text_for_pane(pane)
  if sel and sel ~= '' then return sel, 'selection' end
  -- 2) 最后一个 Output 语义区
  local ok, zones = pcall(function() return pane:get_semantic_zones('Output') end)
  if ok and zones and #zones > 0 then
    local ok2, t = pcall(function()
      return pane:get_text_from_semantic_zone(zones[#zones])
    end)
    if ok2 and t and #t > 0 then return t, 'output-zone' end
  end
  -- 3) 可见区
  return pane:get_lines_as_text(120), 'viewport'
end

wezterm.on('open-ai', function(window, pane)
  local existing = get_ai_pane(window)
  if existing then
    existing:activate()
    return
  end
  local right = pane:split {
    direction = 'Right',
    size = 0.35,
    args = { 'bash', '-lc', 'exec claude' },
  }
  wezterm.GLOBAL[ai_key(window)] = right:pane_id()
end)

wezterm.on('feed-ai', function(window, pane)
  local ai = get_ai_pane(window)
  if not ai then
    window:toast_notification('wezterm', '先打开 AI pane（AI: 打开助手 pane）', nil, 2500)
    return
  end
  local text, how = context_for(window, pane)
  if not text or #text == 0 then return end

  if #text > 4000 then
    local path = os.tmpname()
    local f = io.open(path, 'w+')
    f:write(text)
    f:close()
    ai:send_paste('我这段输出（来源=' .. how .. '）存在 ' .. path
      .. '，请读文件并分析：\n')
  else
    ai:send_paste('以下是我刚才的输出（来源=' .. how .. '），帮我分析：\n```\n'
      .. text .. '\n```\n')
  end
  ai:send_paste('\n')
end)

wezterm.on('augment-command-palette', function(window, pane)
  return {
    { brief = 'AI: 打开助手 pane', icon = 'md_robot',
      doc = '在右侧 35% 开一个 claude pane', action = act.EmitEvent 'open-ai' },
    { brief = 'AI: 分析当前输出', icon = 'md_robot',
      doc = '取选区 / 上一个命令输出 / 可见区，发给 AI pane',
      action = act.EmitEvent 'feed-ai' },
  }
end)

return {
  keys = {
    { key = 'a', mods = 'CTRL|ALT', action = act.EmitEvent 'open-ai' },
    { key = 'A', mods = 'CTRL|ALT', action = act.EmitEvent 'feed-ai' },
  },
}
```

按键选择说明：`CTRL|ALT+letter` 在 wezterm 默认绑定里**完全没有占用**
（`grep 'Modifiers::CTRL.union(Modifiers::ALT)' wezterm-gui/src/commands.rs` 无结果），
在 bash readline 里 `\e\C-a` 一类也基本空白，冲突面最小。

---

## 8. 排错表

| 现象 | 原因 / 处理 |
|---|---|
| AI pane 只收到半截 | 新 pane 没等 pty：`split` 后 `wezterm.sleep_ms(600)` |
| `get_semantic_zones` 返回空 | 没装 shell integration；用 `get_lines_as_text` 兜底 |
| 面板里点了没反应 | 用了 `action_callback`；改成 `act.EmitEvent 'xxx'` |
| `wezterm.mux.get_pane` 报错 | pane 已被杀；`pcall` 包住并清掉 `wezterm.GLOBAL` 里的 id |
| 发过去的中文乱码 | 检查 AI CLI 的 locale；`args = { 'bash','-lc','exec claude' }` 里先 `export LANG=...` |
| 长日志发过去卡住 | 改走 `os.tmpname()` 落盘，只发路径 |
