<!-- 本文件只存在于 personal fork 的 local 分支，不属于上游 wezterm -->

# WezTerm 个人分支：实时关键词高亮 + 上游跟踪方案

## Context

当前仓库 `/home/baili/github/wezterm` 是官方 wezterm 的 fork（`origin` = `baili26/wezterm`），HEAD 与官方 `main` 完全一致（`ce8e6b14`），尚未有本地改动。官方不支持 MobaXterm 式的「输出关键词实时着色」，因此需要在**自己维护的本地分支**上实现该功能，同时保证官方后续更新能低成本合入（只给自己用，不提 PR）。

目标功能：

1. 在官方 Lua 配置里用正则 + 颜色定义关键词规则；
2. 终端输出实时着色（fg/bg/粗体/下划线/斜体）；
3. 内置一套参考 MobaXterm 的预置规则，默认开启；
4. 快捷键（默认 `Ctrl+Shift+H`）**按窗口**实时开关；
5. 能取消某条预置规则、覆盖颜色、追加新规则。

已确认的选择：

- 开关**按窗口独立**；规则支持**前景+背景+粗体/下划线/斜体**；
- 默认快捷键 `Ctrl+Shift+H`，你自己的 `Search` 绑定改到 `Ctrl+Shift+F`；
- 预置规则取**精简词表 + 运维高频词**（error/warning/success/info + IPv4/MAC/URL + k8s 异常状态），只改前景色；
- `keyword_highlight_enabled` **默认 true**，装上即生效；
- Git 用 `main` 跟随官方 + `local` 分支放功能。

---

## 一、Git 管理（先做，后续所有改动都在 local 分支上）

### 分支模型

```
upstream/main  ──fast-forward──▶  main（纯镜像，永远不改）
                                     │
                                     └──rebase──▶ local（功能提交，日常构建/使用）
```

- `main`：官方镜像，只做 `git merge --ff-only upstream/main`。当前已是干净的 `ce8e6b14`，无需改写历史。
- `local`：从 `main` 切出，功能提交在这里；每次同步后 `git rebase main`。
- 功能集中在**新文件 + 约 4 处小钩子**，rebase 冲突面很小（详见第六节）。

### 一次性配置

```bash
cd /home/baili/github/wezterm
git remote add upstream https://github.com/wezterm/wezterm.git   # 已验证可访问
git fetch upstream
git config --global rerere.enabled true     # 重复冲突自动复用解法
git config --global pull.ff only
git branch --set-upstream-to=upstream/main main   # main 跟随官方
git switch -c local main
```

`origin` 仍指向自己的 GitHub fork，用于备份：`git push origin main local`。

### 日常同步脚本（建议放 `scripts/sync-upstream.sh`，不进 local 分支也可以）

```sh
#!/bin/sh
set -e
cd "$(dirname "$0")/.."
git fetch upstream
git switch main
git merge --ff-only upstream/main        # 失败说明 main 被污染，停下来人工处理
git switch local
git rebase main
echo "synced to $(git --no-pager log -1 --format='%h %s' main)"
```

### 冲突面控制原则

- 新功能尽量放**新文件**：`config/src/keyword.rs`、`wezterm-gui/src/keywordhighlight.rs`。
- 必须改的既有文件只用最小 hunk：枚举加一个 variant、match 加一个 arm、config 结构体加两个字段、`render/pane.rs` 加 5~10 行。
- 不重命名/不搬迁官方代码；不改 `Cargo.lock` 以外的生成文件（`config/src/scheme_data.rs`、`docs/**/index.md` 等）。
- 每完成一个阶段在 local 上打 tag：`git tag hl-v1`，便于回退。

---

## 二、功能设计

### 2.1 配置项（`config` crate）

新增文件 `config/src/keyword.rs`：

```rust
//! Keyword highlighting rules
use crate::color::RgbaColor;   // config/src/color.rs:31，Lua 字符串自动解析（#rgb / 颜色名 / css）
use fancy_regex::Regex as FancyRegex;
use wezterm_dynamic::{FromDynamic, FromDynamicOptions, ToDynamic, Value};

/// 双引擎匹配器：能交给 regex crate 的模式走 DFA，其余退回 fancy-regex
#[derive(Clone, Debug)]
pub struct MatchRegex {
    fancy: FancyRegex,            // 序列化的事实来源 + 兜底
    fast: Option<regex::Regex>,   // 能编译就说明不需要 lookaround
}

#[derive(Debug, Clone, FromDynamic, ToDynamic)]
pub struct KeywordRule {
    /// 可选名字，便于在 Lua 里按名删除/调试
    #[dynamic(default)]
    pub name: String,
    /// 正则；`MatchRegex` 自己实现了 FromDynamic/ToDynamic，Lua 侧仍是字符串
    pub regex: MatchRegex,
    /// 前景色，None = 保持终端原色
    #[dynamic(default)]
    pub fg: Option<RgbaColor>,      // Lua: fg = '#ff5555' / 'Red'
    /// 背景色，None = 保持原背景
    #[dynamic(default)]
    pub bg: Option<RgbaColor>,      // Lua: bg = '#3a1d1d'
    #[dynamic(default)]
    pub bold: bool,
    #[dynamic(default)]
    pub italic: bool,
    #[dynamic(default)]
    pub underline: bool,
}
```

> 说明 1：`config/Cargo.toml` 需同时加 `fancy-regex.workspace = true` 和 `regex.workspace = true`
> （workspace 定义在 `Cargo.toml:91` / `Cargo.toml:184`；`mux`、`termwiz`、`wezterm-gui` 都是这么引的）。
>
> 说明 2：`RgbaColor` 已实现 `#[dynamic(try_from = "String")]`（`config/src/color.rs:31-36`），所以 `fg = '#ff5555'` 与 `fg = 'Orange'` 都能解析；转 `ColorAttribute` 用现成的 `From<ColorSpec>`（`config/src/color.rs:107`）。
>
> 说明 3：`Option<T>` 的 `FromDynamic` 已实现（`wezterm-dynamic/src/fromdynamic.rs:91`），字段留空即 `None`。
>
> 说明 4（双引擎的由来）：`fancy-regex` 把 `\b`、lookaround 都判为 "hard"
> （`fancy-regex-0.14.0/src/lib.rs:1576` 的 `Assertion::is_hard()`），只要出现就整条
> 退回 PikeVM 回溯解释器，比 regex crate 的 DFA 慢 30~100 倍。实测同一交替：
> `(?i)(?:starting|started|…)` 0.06 µs，加 `\b` 后 8.29 µs。而绝大多数关键词规则
> 只用 `\b`，完全可以交给 regex crate。于是编译期同时尝试两个引擎，
> `regex::Regex::new()` 成功就用它，失败（lookaround / 反向引用）才用 fancy。


在 `config/src/config.rs` 的 `Config` 结构体中加两个字段（紧跟 `hyperlink_rules` 附近，减少上下文漂移）：

```rust
#[dynamic(default = "default_keyword_rules")]
pub keyword_rules: Vec<keyword::KeywordRule>,

/// 新窗口打开时是否启用关键词高亮（运行期可用快捷键按窗口切换）
#[dynamic(default = "default_true")]
pub keyword_highlight_enabled: bool,
```

`default_keyword_rules()` 放在 `config/src/config.rs`（与 `default_hyperlink_rules` at :1727 并列），见 2.2。

Lua 侧无需额外工作：`FromDynamic/ToDynamic` + `impl_lua_conversion_dynamic!(Config)` 自动支持；`wezterm.action`/`has_action` 也会自动识别新动作。

在 `config/src/lua.rs:365` 附近注册默认值查询：

```rust
wezterm_mod.set(
    "default_keyword_rules",
    lua.create_function(move |lua, ()| {
        Ok(to_lua(lua, crate::config::default_keyword_rules()))
    })?,
)?;
```

这让用户能做到「取消/覆盖/新增」（见 2.4）。

### 2.2 内置预置规则（参考 MobaXterm 的分类，不照抄其正则）

MobaXterm 默认把那套叫 “Default (OK/warning/error keywords)”，分 Red/Green/Yellow/Blue/Magenta/Cyan 六类，核心是 error/warn/success/info + IP/MAC/URL/转义序列。我们按同样分类写一套**可读、可维护**的规则（避免直接复制闭源规则文本，也避免过宽匹配）：

| name | 颜色 | 语义 | 正则（要点） |
|---|---|---|---|
| `error` | `#ff5f5f` + bold | 错误 | `(?i)\b(fail(ed\|ure)?\|error\|errors?\|fatal\|panic\|exception\|traceback\|denied\|refused\|not found\|no such file\|segmentation fault\|core dumped\|abort(ed)?\|critical\|invalid\|unsupported\|cannot\|unable to\|out of memory\|oom\|no space left\|connection reset\|unreachable\|killed)\b\|错误\|失败\|异常\|致命` |
| `warning` | `#ffd75f` | 警告 | `(?i)\b(warn(ing)?s?\|caution\|deprecated\|timeout\|timed out\|retry(ing)?\|too many\|degraded\|unhealthy\|back-?off\|restart(ing)?\|probe failed\|not ready)\b\|警告\|注意\|超时` |
| `success` | `#5fd75f` | 成功 | `(?i)\b(ok\|okay\|success(ful\|fully)?\|succeeded\|done\|pass(ed)?\|complete(d)?\|healthy\|active \(running\)\|listening on)\b\|成功\|已完成` |
| `info` | `#5fd7ff` | 信息 | `(?i)\b(info(rmation)?\|notice\|starting\|started\|loading\|loaded\|creating\|created\|building\|built\|checking\|launching\|connected to)\b\|信息\|开始` |
| `ipv4` | `#d787ff` | IPv4 | `\b(?:\d{1,3}\.){3}\d{1,3}\b` |
| `mac` | `#d787ff` | MAC | `\b[0-9a-fA-F]{2}(?::[0-9a-fA-F]{2}){5}\b` |
| `url` | 无颜色 + underline | URL | `https?://[^\s'"<>]+` |
| `k8s-status` | `#ff5f5f`（仅错误态） | k8s 常见异常 | `\b(ErrImagePull\|ImagePullBackOff\|CrashLoopBackOff\|OOMKilled\|Evicted\|Pending\|Terminating)\b` |

预置规则只改前景色（`error`/`k8s-status` 加粗），不改背景，避免大面积刷屏；URL 只加下划线。用户想要底色可自己写 `bg`。

**关于运维高频词（已决定：加，但克制）**：在精简词表上追加了 `out of memory/oom`、`no space left`、`connection reset/unreachable`、`unhealthy`、`back-?off`、`killed`（错误/警告侧），以及 `healthy`、`active (running)`、`listening on`（成功侧），并单列 `k8s-status`。这些词在运维场景命中率高、歧义小。其余 MobaXterm 里偏宽的类别（Blue 的转义序列/路径/`|| &&`、Cyan 的 shell 命令、Magenta 的网卡名/shell 关键字）**不预置**——它们误报多，留给你在 Lua 里按需加，例如：

```lua
local rules = wezterm.default_keyword_rules()
table.insert(rules, { name = 'nic', regex = '\\b(Gi|Fa|GigabitEthernet|FastEthernet)[0-9/]+\\b', fg = '#d787ff' })
config.keyword_rules = rules
```

### 2.3 渲染集成（核心，`wezterm-gui`）

新增文件 `wezterm-gui/src/keywordhighlight.rs`，实现纯函数（可单测）：

```rust
/// 在 Line 的克隆上就地改写属性
pub fn apply_keyword_rules(rules: &[KeywordRule], line: &mut Line) {
    if rules.is_empty() { return; }
    // 注意：as_str() 对压缩行返回借用，必须先 into_owned，否则后面拿 &mut 会冲突
    let text = line.as_str().into_owned();      // wezterm-surface/src/line/line.rs:631

    // 1) 逐条规则 find_ranges(text, &mut ranges) 收集 (byte_range, rule_idx)；
    //    零宽匹配已被 MatchRegex 过滤；fancy_regex 返回 Err 时该条到此为止
    //    （与 hyperlink.rs:187-215 同策略）
    // 2) 用 line.visible_cells() 一次遍历，边累加 byte 偏移边记录
    //    (byte_start, byte_len, cell_index, width)。这是最稳的映射方式：
    //    as_str() 正是按 visible_cells() 顺序拼接的（line.rs:631-641），
    //    而 cell_index 直接给出原始下标（双宽字符的占位空格在 VecStorage 里
    //    是独立 cell，见 vecstorage.rs:110-112 的 CellRef 实现）
    // 3) spans 按 byte_start 有序，用 partition_point 跳到第一个可能重叠的
    //    span，匹配很短时避免整行重扫
    // 4) 规则按列表顺序后写覆盖先写：同一 cell 取「最后一条命中规则」
    // 5) 落到 cells_mut_for_attr_changes_only()（copy.rs:1468 同款；会自动
    //    coerce 成 VecStorage）：对 cell_index..cell_index+width 的每个 cell
    //    （含双宽占位）设置属性
    // 6) 只改属性不改文本；未命中 cell 保持原样
}
```

> 不建议用 `grapheme_indices`（`vecstorage.rs:49` 的写法）自己推 cell 下标：`visible_cells()` 已经同时给出 `str()` 与 `cell_index()`，双宽/emoji/组合字符都由 `Line` 自己保证一致，代码更短也不易错。

属性写入（`wezterm-cell/src/lib.rs`）：
- `attrs.set_foreground(ColorAttribute::TrueColorWithDefaultFallback(fg.into()))`
- `attrs.set_background(...)`（仅当 `bg.is_some()`）
- `attrs.set_intensity(Intensity::Bold)`（`wezterm_cell` 重导出 `wezterm_escape_parser::csi::Intensity`，:196）
- `attrs.set_italic(true)`
- `attrs.set_underline(Underline::Single)`

渲染钩子 `wezterm-gui/src/termwindow/render/pane.rs` 的 `LineRender::render_line`（当前 :371-554）。**关键顺序**：clone + 着色必须放在 `line_quad_cache` 命中检查（:445-468）**之后**、`render_screen_line` 调用（:489）**之前**，否则缓存命中也会白跑正则和克隆：

```rust
// 原 :424，仍对原始行求哈希（着色是原始行的确定性函数，缓存键无需包含颜色）
let shape_hash = self.term_window.shape_hash_for_line(line);

// ... :426-468 构造 LineQuadCacheKey 并查 line_quad_cache，
//     命中则 apply_to 后直接 return（着色不参与，开销为零）...

// :475-489 缓存未命中，准备 render_screen_line 时：
let styled;
let render_line: &Line = match &self.keyword_rules {
    Some(rules) => {
        let mut l = (*line).clone();
        crate::keywordhighlight::apply_keyword_rules(rules, &mut l);
        styled = l;
        &styled
    }
    None => line,   // `line` 是 &&mut Line，按现有代码写法解引用
};
// 之后 RenderScreenLineParams { line: render_line, .. } 传给 render_screen_line
```

> 这样正则与 clone 只在**实际重绘**的行上发生：滚动/输出时会重绘可见行，但静置画面时为 0。若滚动时仍觉卡，再加 per-line 匹配缓存（见风险表）。

`LineRender` 结构体增加一个字段 `keyword_rules: Option<Vec<KeywordRule>>`。在 `paint_pane` 里**每帧每 pane 只算一次**（不是每行）：

```rust
// 注意 pane_state() 返回 RefMut（termwindow/mod.rs:3289），取完就 drop，
// 不要跨 with_lines_mut 持有，避免运行时重借用 panic
let suppress = self.pane_state(pos.pane.pane_id()).overlay.is_some();
let keyword_rules: Option<Vec<KeywordRule>> = if self.keyword_highlight_enabled
    && !suppress
    && !self.config.keyword_rules.is_empty()
{
    Some(self.config.keyword_rules.clone())
} else {
    None
};
```

> 之所以克隆而不是借用 `self.config` 的字段：`paint_pane(&mut self)` 之后 `LineRender` 持有 `&'a mut TermWindow`，借 `self.config.keyword_rules` 会与 `with_lines_mut(&mut self)` 冲突。`KeywordRule` 里 `Regex` 是 `Arc` 包装，克隆 7 条规则的开销可忽略（每帧每 pane 一次）。

要点与依据：
- **哈希用原始行**：着色是 `(原始行内容, 规则, 开关)` 的确定性函数，用原始 `shape_hash` 作缓存键即可复用；`config_generation` 与 `shape_generation` 已在 `LineQuadCacheKey`/`LineToEleShapeCacheKey` 里（`render/mod.rs:57-100`），规则或开关变化时键自动变化，不会串缓存。
- **不写回模型**：只在克隆体上改属性，滚动区/复制粘贴/搜索文本均不受影响（与 `overlay/copy.rs:1388-1509`、`quickselect` 的既有做法一致）。
- **远程/多路复用 pane 天然可用**：`Pane::with_lines_mut` 对 client pane 本来传的就是克隆（`mux/src/pane.rs:398-416`）。
- **fg/bg 两趟都生效**：`build_line_element_shape` 从 `cluster.attrs` 取色（`screen_line.rs:800-802`），背景 pass 也从 `cluster.attrs` 取（`screen_line.rs:197-258`），改属性即可同时覆盖文字与背景。
- **选中/光标优先级**：`compute_cell_fg_bg`（`render/mod.rs:532-743`）在着色之后应用 selection/cursor 颜色，选中区域仍以选中色为准，符合预期。
- **`use_box_model_render` 路径**（`render/pane.rs:24-30` 的 `paint_pane_box_model` → `build_pane`）目前**不经过** `render_screen_line`，关键词着色对它无效。该选项默认 `false`；如果你的配置开了它，需要额外在 `build_pane` 的对应渲染处加同一钩子（列为可选后续）。

### 2.4 取消 / 覆盖 / 新增（Lua 用法）

```lua
-- 1) 全部关掉（含快捷键开关的初始状态）
config.keyword_highlight_enabled = false

-- 2) 完全自定义（覆盖内置）
config.keyword_rules = {
  { name = 'myerror', regex = '(?i)\\b(FATAL|CRITICAL)\\b', fg = '#ff0000', bold = true },
  { name = 'mybg',    regex = '\\bTODO\\b', fg = '#000000', bg = '#ffd75f' },
}

-- 3) 在内置基础上改：先取默认，再按 name 删/改，再追加
local rules = wezterm.default_keyword_rules()
for i = #rules, 1, -1 do
  if rules[i].name == 'ipv4' then table.remove(rules, i) end   -- 取消某条
  if rules[i].name == 'warning' then rules[i].fg = '#ffa500' end -- 覆盖颜色
end
table.insert(rules, { name = 'k8s', regex = '\\b(ErrImagePull|CrashLoopBackOff)\\b', fg = '#ff5f5f', bold = true })
config.keyword_rules = rules

-- 4) 自定义开关快捷键（内置默认已是 Ctrl+Shift+H，通常无需重写）
config.keys = {
  { key = 'H', mods = 'CTRL|SHIFT', action = wezterm.action.ToggleKeywordHighlight },
}
```

> ✅ 已定：把你的 `Search` 绑定从 `CTRL|SHIFT|H` 改到 `CTRL|SHIFT|F`（`~/.config/wezterm/wezterm.lua:528-534`），`Ctrl+Shift+H` 留给开关。若你后来又想换，只需在 Lua 里写一条 `{ key='H', mods='CTRL|SHIFT', action=wezterm.action.ToggleKeywordHighlight }`（用户绑定优先于内置默认）。

### 2.5 开关动作与状态

- `config/src/keyassignment.rs:537-652` 枚举加一个 unit variant：`ToggleKeywordHighlight,`（无需改序列化；`wezterm.action` 自动可用）。
- `wezterm-gui/src/termwindow/mod.rs`：
  - `TermWindow` 增加字段 `keyword_highlight_enabled: bool`（放在 `show_tab_bar: bool` 附近，struct 定义约 :394）。
  - `new_window` 的结构体初始化处（`mod.rs:716-717`，紧跟 `show_scroll_bar: config.enable_scroll_bar,`）加 `keyword_highlight_enabled: config.keyword_highlight_enabled,`。
  - `config_was_reloaded`（:1727-1842）里重置为新的 `config.keyword_highlight_enabled`（配置重载是显式动作，重置合理）。
  - `perform_key_assignment`（:2604-3166）加分支：
    ```rust
    ToggleKeywordHighlight => {
        self.keyword_highlight_enabled = !self.keyword_highlight_enabled;
        self.shape_generation += 1;          // 同时失效 shape 与 quad 两级缓存
        if let Some(w) = &self.window { w.invalidate(); }
    }
    ```
- `wezterm-gui/src/commands.rs:616-2016` 加 `CommandDef`（命令面板可搜到）。注意该函数返回 `Option<CommandDef>` 且内部是 `Some(match action { ... })`，**没有 catch-all**，新增 variant 会被编译器强制要求补一个 arm；`commands.rs:164 expand_action` 对无 `CommandDef` 的内置动作会打 warn，所以别漏。
- 若要默认键，还要在 `compute_default_actions`（:2019+）尾部加 `ToggleKeywordHighlight,`，`CommandDef.keys` 写 `vec![(Modifiers::CTRL.union(Modifiers::SHIFT), "h".into())]`。
- 缓存失效结论：`shape_generation += 1` 已同时改变 `LineToEleShapeCacheKey` 与 `LineQuadCacheKey` 的键，配合 `window.invalidate()` 即可全量重绘；配置重载已自动 `shape_generation += 1`（:1781）。

### 2.6 需要改动/新增的文件清单

| 文件 | 改动 |
|---|---|
| `config/src/keyword.rs` | **新增**：`KeywordRule` + `RegexWrap` |
| `config/src/lib.rs` | 加 `pub mod keyword;`（并在 `pub use` 里导出 `KeywordRule`） |
| `config/src/config.rs` | 加 2 个字段 + `default_keyword_rules()`；文件头 `use crate::keyword;` |
| `config/src/lua.rs` | 注册 `wezterm.default_keyword_rules` |
| `config/src/keyassignment.rs` | 加 `ToggleKeywordHighlight` |
| `config/Cargo.toml` | 加 `fancy-regex.workspace = true` |
| `wezterm-gui/src/keywordhighlight.rs` | **新增**：匹配+着色纯函数 |
| `wezterm-gui/src/main.rs` | 加 `mod keywordhighlight;`（该 crate 只有 bin target，没有 lib.rs） |
| `wezterm-gui/src/termwindow/render/pane.rs` | 每帧算一次规则 + 克隆着色（小 hunk） |
| `wezterm-gui/src/termwindow/mod.rs` | 字段、初始化、重载、动作分支 |
| `wezterm-gui/src/commands.rs` | `CommandDef` + 默认键 |
| `docs/config/lua/config/keyword_rules.md` 等 | **新增**文档（可选但建议，便于日后自己查） |

---

## 三、实施顺序

0. **装构建环境**（见第五节；耗时最长，**先做**，可与 1、3、4 并行）。
1. **Git 基座**（第一节命令；切出 `local`；写 `scripts/sync-upstream.sh`）。
2. （环境就绪后）**纯逻辑先行**：`config/src/keyword.rs` + `config` 字段 + 默认规则 + `default_keyword_rules` Lua helper；`cargo check -p config`。
3. **匹配/着色函数 + 单测**：`wezterm-gui/src/keywordhighlight.rs`，测试放在该文件 `#[cfg(test)]`（`wezterm-gui` 只有 bin target，用 `cargo test -p wezterm-gui keyword` 运行；测试只用 `Line::from_text`，不碰字体/GL）。
4. **渲染接线**：`render/pane.rs` 钩子，`cargo check -p wezterm-gui`。
5. **开关动作**：`KeyAssignment` + `perform_key_assignment` + `commands.rs`。
6. **文档与 changelog**（可选）。
7. `cargo +nightly fmt --all`（CI 用 nightly 的 `imports_granularity`）。
8. **把本计划拷进仓库**：`docs-internal/keyword-highlight.md`（见第八节），单独一个 docs commit。
9. 在 `local` 上提交（建议拆 4 个 commit：`config` / `gui render` / `action+docs` / `design notes`），打 tag `hl-v1`。
10. 构建 release 并日常使用验证。
11. 调整 `~/.config/wezterm/wezterm.lua`：`Search` 改到 `CTRL|SHIFT|F`。

---

## 四、测试与验证

### 单元测试（无需 GUI）
`wezterm-gui/src/keywordhighlight.rs` 内：

- 基本匹配：`"an error occurred"` + `error` 规则 → 仅 `error` 5 个 cell 的 fg 改变，其余不变。
- 多规则优先级：先写 `foo`、后写 `o`，重叠字符取后者。
- 宽字符/emoji：`"❤ 错误 😍"` + 中文规则 → 双宽 cell 与其占位 cell 都被着色，`cell_index` 不偏移。
- 无匹配 / 空规则 / 非法正则（构造时已报错，运行时 Err 忽略）。
- 零宽匹配（如 `\b`、`(?=x)`）不产生无限循环、不着色。
- 保留属性：只配 `bold` 时不改 fg/bg；只配 `bg` 时文字色不变。

### 端到端手工验证
1. `cargo run --release -p wezterm-gui -- start --always-new-process --config-file ./test-conf.lua`（若 `wezterm`/`wezterm-gui` 找不到运行时依赖，可直接跑 `./target/release/wezterm-gui start --always-new-process --config-file ./test-conf.lua`）
2. `test-conf.lua` 内容：
   ```lua
   return {
     keyword_rules = {
       { name = 'e', regex = '(?i)\\berror\\b', fg = '#ff5f5f', bold = true },
       { name = 'ok', regex = '(?i)\\bok\\b', fg = '#5fd75f' },
     },
   }
   ```
3. 在 pane 里 `printf 'error\nok\nerror ok\n'`，确认实时着色。
4. `yes 'error' | head -200` 滚动刷屏：确认无卡顿（重点观察）。
5. 按 `Ctrl+Shift+H`：着色立即消失；再按恢复；新建窗口确认开关互不影响（按窗口独立）。
6. 打开 copy mode / 搜索：确认关键词着色让位，搜索高亮正常。
7. `wezterm show-keys --lua | grep -i keyword` 应出现默认绑定。
8. 配置热重载（`tmux` 外：`Ctrl+Shift+R` 默认重载配置）：确认新规则立即生效。
9. 选一段被高亮的文本复制，确认粘贴出来是纯文本、无颜色污染。

### 回归
`cargo test -p wezterm-term`、`cargo test -p wezterm-surface`、`cargo test -p config`、`cargo +nightly fmt --all -- --check`。

---

## 五、构建环境（当前机器缺失）

已确认：**没有 Rust 工具链**（无 `cargo`/`rustc`/`rustup`、无 `target/`），`libxkbcommon-dev`、`libwayland-dev` 未装，4 个子模块未初始化。网络可用（GitHub 与 static.rust-lang.org 均可达）。

一次性步骤（需 sudo，建议用户自己执行或确认）：

```bash
# 1) Rust 工具链
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
rustup component add rustfmt clippy
rustup toolchain install nightly --component rustfmt   # 仅格式化用

# 2) 系统依赖（脚本会调 apt）
cd /home/baili/github/wezterm
./get-deps

# 3) 子模块（harfbuzz/freetype 必需；build.rs 也会自动拉，但显式更可控）
git submodule update --init --recursive
```

构建：
```bash
cargo check -p config -p wezterm-gui            # 快速迭代
cargo build --release -p wezterm-gui -p wezterm -p wezterm-mux-server
printf '20261113-local\n' > .tag                # 可选：覆盖版本号（.tag 已 gitignore）
./target/release/wezterm start --always-new-process
```
提示：首次 release 构建耗时较长（估计 20~40 分钟，视 CPU）；期间可先用 `cargo check` 验证类型。

---

## 六、风险与缓解

| 风险 | 评估 | 缓解 |
|---|---|---|
| 上游 rebase 冲突 | 中低。改动集中在新增文件；`config/src/config.rs`（字段）、`keyassignment.rs`（枚举）、`commands.rs`（两个 match）、`render/pane.rs`、`termwindow/mod.rs` 属高频改动区 | 每个 hunk 保持最小、不重排上下文；开 `rerere`；同步后先 `cargo check` 再修冲突；tag 便于回退 |
| 官方也实现类似功能 | 低，但若发生 | 我们的实现独立在 `local` 分支，可整分支丢弃，只保留 Lua 配置迁移 |
| 正则影响性能 | 中。quad 缓存命中时不跑正则；但滚动时 `top_pixel_y` 变化会让所有可见行 miss（`LineQuadCacheKey` 含像素坐标，`render/mod.rs:65`） | 先按简单实现；若卡顿，加 per-line 匹配缓存 `(shape_hash, rules_generation) -> Vec<Range>`（`LfuCache`，模式同 `line_state_cache`），或改用 `regex` crate 快速路径 |
| 规则过宽误伤（如 `ok` 命中路径） | 中 | 预置规则只用保守词表和 `\b`；文档给出 `(?i)`/边界写法；提供 `name` 便于删除 |
| 与 copy-mode/search 抢色 | 低 | 检测 `pane_state.overlay.is_some()` 时跳过关键词着色 |
| 双宽字符/emoji 偏移 | 中 | 用 `Line::visible_cells()` 自带的 `cell_index()`（而非自推下标）做映射；单测覆盖 |
| 每帧 clone 行 | 低 | 仅当启用且有规则时 clone；一屏最多约 60 行 |
| 快捷键被用户配置遮蔽 | 已确认存在 | 已定：把自家 `Search` 从 `CTRL\|SHIFT\|H` 改到 `CTRL\|SHIFT\|F` |
| 上游 `RegexWrap` 被重构 | 低 | 我们在 config 内自带包装，不依赖其可见性 |

---

## 七、已确认的设计选择

| 项 | 结论 |
|---|---|
| 开关作用域 | 按窗口独立（`TermWindow` 字段） |
| 规则样式 | fg + bg + bold/italic/underline，未配的字段保持原样式 |
| 默认快捷键 | 内置 `Ctrl+Shift+H`；自家 `Search` 改到 `Ctrl+Shift+F` |
| 预置规则 | 精简词表 + 运维高频词：error/warning/success/info + IPv4/MAC/URL + k8s 异常状态，只上前景（error/k8s 加粗） |
| 默认开关 | `keyword_highlight_enabled = true`（装上即生效） |
| Git 模式 | `main` 跟随官方 + `local` 分支放功能 |

实施中仍可微调的两点（不影响开工）：
1. ~~预置词表是否再加运维高频词~~ → **已定：加**（见 2.2 表格与说明：`out of memory/oom`、`no space left`、`connection reset/unreachable`、`unhealthy`、`back-?off`、`killed`、`healthy`、`active (running)`、`listening on` + 独立的 `k8s-status` 规则；MobaXterm 里偏宽的 Blue/Cyan/Magenta 类不预置）。
2. 是否额外提供「临时只看某条规则」的调试动作（例如 `wezterm.action.EmitEvent` 配合 Lua 改 `keyword_rules`）——当前方案已支持通过配置热重载达到同等效果，暂不单独做。

---

## 八、这份计划本身怎么存（已定：存进仓库）

当前计划文件在 `~/.claude/plans/fork-mobaxterm-fork-git-pr-lua-calm-castle.md`，**在仓库之外、且会随会话失效**。实施时把它复制进仓库，跟功能一起提交：

- 路径：`docs-internal/keyword-highlight.md`。`docs-internal/` 是上游跟踪目录，但**新增文件不产生任何 rebase 冲突**；不要放进 `docs/`（那里被 `ci/generate-docs.py` 与 mkdocs 扫描，还会进 `SUMMARY.md` 生成流程）。
- 分支：只存在于 `local` 分支，作为独立 commit（`docs: keyword highlight design notes`）；`main` 保持纯净、不含此文件。
- 内容：把本文档原样拷入，开头加一行 `<!-- 本文件只存在于 personal fork 的 local 分支 -->`。
- 可选：在 `~/.config/wezterm/wezterm.lua` 顶部留一行指针 `-- 本地构建说明见 ~/github/wezterm/docs-internal/keyword-highlight.md`。
- 日后上游同步导致实现调整，直接在仓库里这份文档上改（它跟着 `local` 走），而不用回头找 `~/.claude/plans/`。

---

## 九、实施进度（2026-11-13）

已完成：

- Git：加了 `upstream` remote；`main` 跟踪 `upstream/main`（当前 `ce8e6b14`，与官方完全一致）；功能在 `local` 分支（tag `hl-v1`）。
- 已修：`config_was_reloaded()` 会被 appearance 变化和 tab bar 显隐触发，原来在里面无条件重置开关，导致按 `Ctrl+Shift+H` 关掉后立刻被重载重新打开。现在只在配置里的 `keyword_highlight_enabled` 本身变化时才重新播种，运行期切换能保持。
- 已改：`~/.config/wezterm/wezterm.lua`（实际文件 `/data/baili.work/tpl/conf/wezterm/wezterm.lua`）里 `Search` 从 `CTRL|SHIFT|H` 挪到 `CTRL|SHIFT|F`，`CTRL|SHIFT|H` 显式绑定 `ToggleKeywordHighlight`。
- `scripts/sync-upstream.sh`：同步脚本。
- `scripts/fetch-dev-deps.sh` + `scripts/dev-env.sh`：本机缺少 `libxkbcommon-dev` / `libwayland-dev` / `libx11-xcb-dev` 等，且 sudo 需要密码，故把 .deb 解包到 `~/.local/wezterm-sysroot`，用脚本注入 pkg-config / CFLAGS / LIBRARY_PATH。构建前先 `. scripts/dev-env.sh`。
- 代码：`config/src/keyword.rs`（`KeywordRule`）、`config/src/config.rs`（`keyword_rules` + `keyword_highlight_enabled` + `default_keyword_rules()`）、`config/src/lua.rs`（`wezterm.default_keyword_rules`）、`KeyAssignment::ToggleKeywordHighlight`、`wezterm-gui/src/keywordhighlight.rs`（着色 + 7 个单测）、`render/pane.rs` 钩子、`termwindow/mod.rs` 状态与动作、`commands.rs` 默认键 `CTRL|SHIFT|H`。
- 测试：`cargo test -p config keyword` 5 passed（含 default rules 的 Lua 往返）；`cargo test -p wezterm-gui keyword` 7 passed；回归 `cargo test -p config -p wezterm-surface -p wezterm-term -p wezterm-gui` 全绿（13 / 40 / 53 / 25）；`cargo +nightly fmt --all -- --check` 干净。

已完成的真机（Xvfb）验证：

- 预置规则：error 红、warning 黄、success 绿、info 青、IPv4/MAC 紫、URL 下划线、k8s 状态红 —— 逐词像素扫描确认只给命中的词着色（`error` 红、`error:` 里的冒号不着色等）。
- `Ctrl+Shift+H` 开关：关掉后所有高亮色消失，再按恢复；窗口 resize 后状态保持。
- copy mode 下自动让位（无关键词着色），退出后恢复。
- `wezterm.default_keyword_rules()` 删除 / 改色 / 追加：删掉 `ipv4` 后 `10.0.0.5` 不再着色；`warning` 改 `#ffa500` 生效；自定义规则带 `bg` + `underline` 生效。
- 命令面板可搜到 “Toggle keyword highlighting”。
- 4000 行压力输出：开/关高亮 CPU 采样 181 vs 174 ticks（6 秒），无可测差异。

待办：

- 可选：本机日常使用前，把 `~/.cargo/bin` 加进 PATH（或在 shell profile 里 source `scripts/dev-env.sh`），否则 `cargo`/`wezterm` 新构建不在 PATH 上。
- 若要全新机器重建 sysroot：`scripts/fetch-dev-deps.sh`（会把 .deb 缓存到 `~/.local/wezterm-sysroot/debs`）。

## 十、双引擎匹配（2026-11-19）

问题：规则一多就慢。实测 60 行日志一屏全量重绘，内置 8 条 + 扩展 99 条约
**89 ms**，滚动会掉帧。定位到根因不是规则数量，而是 `fancy-regex` 的
`Assertion::is_hard()`（`fancy-regex-0.14.0/src/lib.rs:1576`）把 `\b`、lookaround
都判为 hard，整条规则因此放弃 regex crate 的 DFA、退回 PikeVM 回溯解释器。
同一个交替 `(?i)(?:starting|started|…)`：不带 `\b` 是 0.06 µs，带 `\b` 是 8.29 µs。

做法：`config/src/keyword.rs` 用 `MatchRegex` 取代原来的 `RegexWrap`，
加载时编译两个引擎：

```rust
pub struct MatchRegex {
    fancy: FancyRegex,          // 永远存在：序列化来源 + 兜底
    fast: Option<regex::Regex>, // Some = 该模式不需要 lookaround
}
```

`regex::Regex::new()` 本身会拒绝 lookaround/反向引用，所以“编译成功”就等价于
“语义上可以安全地走快引擎”。`find_ranges()` 按 `fast` 是否存在分发；
`FromDynamic`/`ToDynamic` 手工实现（Lua 侧仍是一个字符串），`config/Cargo.toml`
加 `regex.workspace = true`。

`wezterm-gui/src/keywordhighlight.rs` 同步改成 `rule.regex.find_ranges(&text, &mut ranges)`，
并把「每条规则的每个匹配都扫一遍整行 spans」改成按 `byte_start` 二分
（`partition_point`）跳到第一个可能重叠的 span。

结果：

| 规则集 | 改造前 | 双引擎后 |
|---|---|---|
| 内置 8 条 | 8.3 ms/屏 | 0.1 ms/屏 |
| 内置 8 + 扩展 99 条 | 89 ms/屏 | **9.0 ms/屏** |

107 条中 94 条走快引擎，13 条留在 fancy（全部是用了 lookbehind 的规则），
这 13 条独占 7.5 ms，其中 `x-cmd` 一条 2.5 ms。检查方式：
`WEZTERM_LOG=config=debug wezterm ls-fonts 2>&1 | grep "stays on fancy-regex"`。

新增测试（`config/src/keyword.rs`）：

- `engine_selection`：`\b` 模式必须走快引擎；lookbehind / 反向引用必须留在 fancy。
- `both_engines_agree`：4 种规则形态 × 5 个样本，两个引擎返回的匹配字节区间逐一相同。

回归：`cargo test -p config keyword` 7 passed；`cargo test -p wezterm-gui keyword`
7 passed；`cargo +nightly fmt --all -- --check` 干净；Xvfb 逐像素比对显示颜色落点
与改造前完全一致；4000 行刷屏 CPU 采样开/关为 535 / 523 ticks（6 秒），无可测差异。

后续可选：per-line 匹配结果缓存（`LfuCache<(line_hash, rules_generation), Vec<(usize, usize)>>`，
模式同 `line_state_cache`），用于把滚动时的重复行降到接近零开销。
