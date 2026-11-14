<!-- 本文件只存在于 personal fork 的 local 分支，不属于上游 wezterm -->

# 生产环境视觉警示：只改光标 / 选区，不改背景

目标：扫一眼就知道"我现在在不在生产机上"，且**不影响正文配色判断**。
整窗背景染红（`colors.background` + `window_background_opacity`）这个方案
**已否决**——正文里 error 本来就是红的，加背景色会让"程序自己的红"和
"环境的红"混在一起。所以只动两处：光标、选区。

本文所有结论都在本机（tmux 3.6a + wezterm）实测过，标注了证据来源。

---

## 0. 先说结论

| 方案 | 作用域 | 改动位置 | 推荐度 |
|---|---|---|---|
| A. tmux `cursor-colour` | 跟随 tmux 的活动窗口（= 当前主机） | 远端 `~/.tmux.conf` | ★★★ 首选 |
| B. 远端发 `OSC 12/17/19` | 单个 pane | 远端 `/etc/profile.d/` | ★★ 补充（能覆盖选区） |
| C. wezterm `set_config_overrides` | 整个 wezterm 窗口 | 本机 `wezterm.lua` | ★ 只适合"整窗专用生产" |

**做不到的**：pane 边框着色。`colors.split` 只有一种全局分割线颜色
（`wezterm-gui/src/termwindow/render/split.rs`），没有 per-pane 边框，
要"生产 pane 红框"必须改代码。本方案不含。

---

## 1. 为什么光标比背景更适合做警示

- 光标是**每时每刻都在视野里**的元素，且只有一处，不会被日志颜色淹没；
- 背景染色会和 `error` 红色高亮、`top`/`htop` 的底色互相干扰，导致你
  误判程序自己的配色；
- 光标颜色可以**由远端程序直接设置**（OSC 12），所以能做到"跟着主机走"，
  不依赖 wezterm 猜当前是不是生产机。

---

## 2. 方案 A：tmux `cursor-colour`（首选）

tmux 3.6a 支持 window 级选项 `cursor-colour`，它会把颜色以 `OSC 12`
发给外层终端，也就是 wezterm。

**实测证据**（`script` 抓外层字节流）：

```
# setw cursor-colour red 之后外层收到的字节
ESC ] 12 ; rgb:80/00/00 BEL        ← 出现 3 次（red 被 tmux 降为 80/00/00）
# 在两个分别设了 red / green 的 tmux 窗口之间来回切换
red, red, red, green, green, red, red   ← 切窗口时 tmux 会重新发一遍
```

结论：**tmux 会跟随活动窗口重新下发光标色**，所以"切到哪个窗口，
光标就变成那台主机的颜色"，这正是你要的语义。

wezterm 侧接收位置：`term/src/terminalstate/performer.rs:1018`
（`TextCursorColor` → 同时写 `cursor_border` 和 `cursor_bg`），
渲染读取在 `wezterm-gui/src/termwindow/render/pane.rs:91,301`
（`palette.cursor_bg/cursor_border`，**来自 pane 自己的 palette**）。

### 配置

远端每台机器的 `~/.tmux.conf`：

```tmux
# 生产机
setw -g cursor-colour '#ff5f5f'
# 预发
# setw -g cursor-colour '#ffaf00'
# 开发机：不设（默认 none，沿用 wezterm 自己的配色）
```

想连选区也变：

```tmux
setw -g cursor-colour '#ff5f5f'
```

> ⚠️ tmux 的 `copy-mode-selection-style` / `selection-style` **不会**转成
> `OSC 17`，实测外层收不到任何 `OSC 17`。所以 tmux 这一档只能管光标，
> 选区要变走方案 B。

> ⚠️ `cursor-colour` 不接受 tmux 格式串：`set -gw cursor-colour '#{?#{==:#{host_short},x},red,green}'`
> 会报 `bad colour`。必须**静态**写死颜色，所以是"每台机器一份 tmux.conf"
> 或"一份 conf 里按主机名注释切换"。

---

## 3. 方案 B：远端 shell 直接发 OSC（能覆盖选区）

wezterm 支持这几个 OSC（`wezterm-escape-parser/src/osc.rs:475-490`）：

| OSC | 含义 | wezterm 映射到 |
|---|---|---|
| `OSC 12` | 光标色 | `cursor_bg` + `cursor_border` |
| `OSC 17` | 选中文本背景 | `selection_bg` |
| `OSC 19` | 选中文本前景 | `selection_fg` |
| `OSC 112/117/119` | 复位上面三个 | 恢复配置色 |

关键差别：**这些 OSC 只影响发出它们的那个 pane**（palette 是 per-TerminalState，
最后被 `pane.palette()` 读出来），所以你可以做到"只有生产 pane 变红，
同一个窗口里其它 pane 保持原样"。这是纯 Lua/配置做不到的粒度。

### 3.1 不在 tmux 里时

`/etc/profile.d/00-tint.sh`（远端）：

```sh
case "$(hostname -s)" in
  prod-*|db-*|pve-*|k8s-master-*)
    # 光标红、选区暗红
    printf '\033]12;#ff5f5f\007'
    printf '\033]17;#5a1414\007'
    printf '\033]19;#ffffff\007'
    ;;
esac
```

### 3.2 在 tmux 里时（必须包 passthrough）

实测：tmux **会吞掉**里面程序直接发的 `OSC 1337` / `OSC 777`：

```
直接发  SetUserVar=plainkey  → 外层收到 0 次
包一层  ESC Ptmux; ... ESC \ → 外层收到 1 次（且需要 allow-passthrough on）
```

所以：

```sh
# 远端 ~/.tmux.conf
set -g allow-passthrough on
```

```sh
__tint() {  # $1 = OSC 号, $2 = 颜色
  if [ -n "${TMUX-}" ]; then
    printf '\033Ptmux;\033\033]%s;%s\007\033\\' "$1" "$2"
  else
    printf '\033]%s;%s\007' "$1" "$2"
  fi
}

case "$(hostname -s)" in
  prod-*|db-*)
    __tint 12 '#ff5f5f'
    __tint 17 '#5a1414'
    __tint 19 '#ffffff'
    ;;
esac
```

`allow-passthrough on` 的含义（`man tmux`）：允许 pane 内程序用
`\ePtmux;...\e\\` 绕过 tmux；`on` 只在 pane 可见时放行，`all` 连不可见也放行。
只做视觉提示的话 `on` 够了。

> ⚠️ 注意 OSC 12/17/19 是**粘性**的：ssh 退出后颜色不会自动恢复。
> 换到非生产机器前要么发 `OSC 112`/`117`/`119` 复位，要么在新机器登录时
> 也发一遍（非生产发 wezterm 配色里的默认值，或用 `OSC 104` 全量复位）。

---

## 4. 方案 C：wezterm 侧按"环境标记"改整窗配色

需要远端告诉 wezterm"这是生产"，用 `OSC 1337 SetUserVar`
（wezterm 会在收端 base64 解码，事件里拿到的已是明文）：

```sh
# 远端；tmux 里必须包 passthrough
__setver() {
  local v; v=$(printf '%s' "$2" | base64)
  if [ -n "${TMUX-}" ]; then
    printf '\033Ptmux;\033\033]1337;SetUserVar=%s=%s\007\033\\' "$1" "$v"
  else
    printf '\033]1337;SetUserVar=%s=%s\007' "$1" "$v"
  fi
}
__setver env prod
```

本机 `wezterm.lua`：

```lua
local PROD = {
  cursor_bg     = '#ff5f5f',
  cursor_border = '#ff5f5f',
  cursor_fg     = '#000000',
  selection_bg  = '#5a1414',
  selection_fg  = '#ffffff',
}

wezterm.on('user-var-changed', function(window, pane, name, value)
  if name ~= 'env' then return end
  local overrides = window:get_config_overrides() or {}
  -- 幂等：值没变就别动，否则每次事件都会触发一次完整配置重载
  local want = (value == 'prod')
  if (overrides.colors ~= nil) == want then return end

  if want then
    -- ⚠️ 见下方"坑"：这里必须把你 config.colors 里自定义的项全部重写一遍
    overrides.colors = {
      cursor_bg     = PROD.cursor_bg,
      cursor_border = PROD.cursor_border,
      cursor_fg     = PROD.cursor_fg,
      selection_bg  = PROD.selection_bg,
      selection_fg  = PROD.selection_fg,
    }
  else
    overrides.colors = nil
  end
  window:set_config_overrides(overrides)
end)
```

### ⚠️ 坑：`set_config_overrides` 是整体替换 `colors`，不是合并

这是本方案最容易踩的一处，你现有配置里已经踩过一次
（`wezterm.lua` 第 344-345 行的注释："重新设置选中文本的前背景色，不然会恢复成默认"）。

原因链：

1. `config.colors` 的类型是 `Option<Palette>`（`config/src/config.rs:131`）；
2. `set_config_overrides` 走的是 Lua 赋值 `config["colors"] = <新表>`
   （`config/src/config.rs` 的 `apply_overrides_obj_to`），**整表替换**；
3. 之后才做 `resolved_palette = resolved_palette.overlay_with(&cfg.colors)`
   （`config/src/config.rs:1435`），而 `overlay_with` 的缺省回退是
   **配色方案**（`color_scheme`）的值，不是你在 `config.colors` 里写的值。

所以你 `config.colors` 里写了 `selection_fg/bg`，一旦 override 里只写
`cursor_bg`，选区色就会被还原成 Campbell 的默认值。**override 里要写全。**

---

## 5. 推荐组合

1. **每个 pane 一条 `OSC 17/19`**（经 tmux passthrough）→ 选区暗红，
   这是"最像生产环境的视觉提示"，且只看这一个 pane。
2. **光标色用方案 A**（tmux `cursor-colour`）→ 切窗口时自动跟着走，
   连 tmux 里嵌套的 wezterm 都不需要额外配置。
3. 如果整台机器只跑生产业务、窗口也只开生产 → 用方案 C 整窗改，最省心。

不必三个都用。**只用 A 就能拿到 80% 的效果**（光标变色 + 跟随窗口）。

---

## 6. 排错表

| 现象 | 原因 / 处理 |
|---|---|
| 设了 `cursor-colour` 但光标没变 | 确认外层是 wezterm（又不是别的终端）；`printf '\033]12;#ff0000\007'` 手打一遍验证；检查是否被 tmux 的 `window-active-style` 覆盖 |
| 在 tmux 里发 OSC 完全没反应 | tmux 默认吞掉；`set -g allow-passthrough on` + 用 `\ePtmux;` 包一层 |
| 选区没变红 | tmux 的 selection-style **不会**转 OSC 17（实测 0 次）；用方案 B 或方案 C |
| `bad colour: #{?...}` | `cursor-colour` 不支持格式串，只能写死 |
| 整窗配色被打乱 | 见第 4 节"坑"：override 里要写全 `colors` |
| ssh 换机器后光标还是红的 | OSC 12 是粘性的；在非生产机 login 时发 `OSC 112` 复位 |
| 想给 pane 加红边框 | 做不到，需要改代码 |
