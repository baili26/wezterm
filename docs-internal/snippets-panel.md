<!-- 本文件只存在于 personal fork 的 local 分支，不属于上游 wezterm -->

# 片段面板（snippet palette）

`local` 分支的 Rust 功能：一个自绘 modal，左边列表、右边预览，把
`~/.config/wezterm/snippets/` 下的文本片段插进当前 pane。

外观目标是"像 fzf 一样干净"，但**不是透明浮层**：面板默认是**不透明黑底 + 细圆角边框**，
输入行下和 footer 上各一条分隔线。这样后面有终端内容时也不会看不清。
层次还靠对齐、缩进、暗色次要文字、选中色条和命中高亮来做。面板内所有界面文字是英文。

设计期讨论记录在 `docs-internal/snippets.md`；**日常使用/配置速查见
[`snippets-panel-usage.md`](snippets-panel-usage.md)**（本文件偏实现细节）。
早期那套纯 Lua 的试用版（`snippets-kit/`）已经删除，Rust 面板是唯一实现。

---

## 1. 怎么用

默认键 **`Ctrl+Alt+O`**（`Ctrl+Alt+P` 被 fcitx5 的 `[Hotkey/TogglePreedit]`
在 X11 层抓走了，用不了）。想换键就在配置里重绑：

```lua
config.keys = {
  { key = 'o', mods = 'CTRL|ALT', action = wezterm.action.ShowSnippetPalette },
}
```

也可以在命令面板里搜 "Snippet"。

### 面板内按键

| 键 | 作用 |
|---|---|
| `↑` `↓` | 选择 |
| `PgUp` `PgDn` | 翻页 |
| `Enter` | 插入（不执行），关闭面板 |
| `Ctrl+Enter` | 插入并回车执行（`danger` 片段 / 含填充位片段会降级为只插入） |
| `Ctrl+N` | 新建片段：先取选区当正文，再在面板里输入标题 |
| `Ctrl+E` | 右侧 split 用 `$EDITOR` 打开该片段文件，**并关闭面板** |
| `Ctrl+Alt+O` | 再按一次关闭面板（toggle） |
| `Ctrl+D` | 移到 `.trash/`（需按 `y` 确认） |
| `Ctrl+T` | 逐项编辑元数据（title → tags → desc → pin → danger） |
| `Ctrl+R` | 重新扫描目录 |
| `Ctrl+H` | 底部按键提示：隐藏 → 单行 → 全部，循环 |
| `Ctrl+I` | 开关预览区的元数据块 |
| `Ctrl+W` | 删除搜索框里最后一个词（和 shell 一样） |
| `Ctrl+U` | 清空搜索词 |
| `Alt+W` | 预览软折行开关 |
| `Ctrl+J` / `Ctrl+K` | 预览上下滚动 |
| `Ctrl+C` | 复制预览区里已拖选的内容 |
| `Esc` | 先清预览选区，再按一次关闭（子模式下先退出子模式） |

搜索框/提示框共用同一套单行编辑键：`←` `→` `Home` `End` `Ctrl+A`
`Backspace` `Delete` `Ctrl+W` `Ctrl+U`，并支持输入法词组直接输入。
唯一的例外是搜索框里的 `Ctrl+E`：它被"用编辑器打开片段"占用了，
要把光标移到行尾用 `End`（`Ctrl+T` 的提示框里 `Ctrl+E` 仍然是行尾）。

不使用功能键：`F1`/`F3`/`F4` 常被桌面环境或其它程序截走。

### 鼠标

面板打开时**独占鼠标**（之前点击会穿透到下面的 pane，选中看不见的终端文本）：

| 操作 | 效果 |
|---|---|
| 左键点列表行 | 选中该行 |
| 左键拖过预览区 | 选中预览文本（松手后保持高亮，**不会**自动复制） |
| 右键 | 复制当前预览选区到 clipboard，并清除高亮 |
| 左键双击列表行 | 插入（同 `Enter`） |
| 滚轮在列表区 | 上/下移动选中行 |
| 滚轮在预览区 | 滚动预览 |

空白处（空行或行尾空白列）不能起选，避免选中一堆看不见的空格。

因为模态独占鼠标，面板开着时**无法拖选终端文本**。要复制内容用 `Ctrl+E`
进编辑器，或先关掉面板。

### 新建片段

`Ctrl+N` 用**当前选区**当正文：

- 没选中任何东西时不会新建空文件，footer 会提示"先选文本"；
- 输入标题时，右侧预览直接显示**即将创建的片段**（标题 + 正文），确认后再落盘到 `inbox/`。

### 搜索语法

空格分隔多个词，全部满足才命中（AND）：

| 输入 | 含义 |
|---|---|
| `kubectl` | 在 title / desc / tags / 相对路径上模糊匹配，**命中的字符会加粗高亮** |
| `#k8s` | tags 或所在目录包含 `k8s` |
| `@net/` | 相对路径以 `net/` 开头 |
| `kind:config` | 类型过滤（`command` / `config`） |
| `!` | 只看 `danger: true` |
| `/nodePort` | 在正文里做子串匹配（默认不搜正文） |

只有自由文本词参与高亮；`#tag` / `@dir` / `!` 这类过滤词不标字。

排序：空的查询下 `pin: true` 的排最前，其余按使用频率；有查询时按匹配得分，
同分看使用频率。使用记录存在 `$XDG_DATA_HOME/wezterm/snippet-frecency.json`。

---

## 2. 片段文件

```
~/.config/wezterm/snippets/
├── k8s/
│   ├── pod-detail.snip
│   └── deploy.yaml
├── net/iptables.snip
├── inbox/            # Ctrl+N 生成
└── .trash/           # Ctrl+D 移入（隐藏目录，不会被扫描）
```

```markdown
---
title: 查看某命名空间的 pod
desc: 插入后光标停在命名空间处，直接 Tab 补全
tags: [k8s, pod, debug]
kind: command
danger: true       # 可选
pin: true          # 可选
---
kubectl -n $|$ get pod -o wide
```

front matter 是可选的。判定规则：**只有文件第一行是 `---`，且闭合块里至少有一个
认识的键**（`title`/`desc`/`tags`/`kind`/`danger`/`pin`）才算 front matter，
所以以 `---` 开头的 k8s yaml 不会被误判。

支持的语法只有这几种，不是完整 YAML：`key: value`、`key: [a, b]`、
`true`/`false`/`yes`/`no`、双引号或单引号字符串。不认识的键会被忽略（`Ctrl+T`
改写时**原样保留**，注释也保留）。

### 光标填充位：`$|$`

**单行**片段里的 `$|$` 是填充位：预览里显示出来（高亮），插入时被删掉，光标停在那里。

```
kubectl -n $|$ get pod -o wide   →   kubectl -n | get pod -o wide
```

- 这是三个字符的标记，不是正则，也不是 shell 变量；`$?`、`file?.txt`、URL query
  都不会误触发；
- **多行片段不生效**：yaml/sql 模板里的 `$|$` 原样保留，光标留在末尾。
  多行要回跳到中间某行得发上箭头，在软折行、编辑器自动换行、双宽字符下都不准，
  恢复代价还比单行高。插进 vim 之后用 `/name` 搜索更快；
- 正文里本来就需要的字面量 `$|$`（极少见），关掉这个功能即可：
  `config.snippet_panel.placeholder = false`；
- 实现是"粘贴完成后单独写一轮左箭头 `\x1b[D`"。方向键在 bash/zsh/fish/vim
  插入模式/psql/python REPL 里都表示左移一格；在不解析控制序列的程序里
  （`cat > file` 读 stdin）会多出 `^[[D` —— 这是唯一会污染文本的情形。

---

## 3. 配置

```lua
config.snippet_panel = {
  dir = '/home/baili/.config/wezterm/snippets',  -- 默认 $XDG_CONFIG_HOME/wezterm/snippets
  width = 0.80,          -- 面板占终端列数比例（最小 40 列，四周留白）
  height = 0.62,         -- 占行数比例（最小 10 行）
  list_ratio = 0.40,     -- 列表列占比，其余 60% 给预览

  bg = '#000000',            -- 面板底色（默认不透明黑）
  fg = nil,                  -- 前景；nil = 用终端前景色（对比度不够时自动反色）
  selection_bg = '#3a3a4d',  -- 选中条底色
  selection_fg = '#f0f0f0',  -- 选中条文字色（选中行整体反色）
  match_fg = '#ffd75f',      -- 命中高亮色

  notify_timeout_ms = 10000, -- 新建/删除的桌面通知停留时长

  preview_meta = true,   -- 预览区是否显示完整元数据块（Ctrl+I 切换）
  footer = 'Short',     -- 底部按键提示初始状态：'Hidden' / 'Short' / 'Full'（Ctrl+H 循环）
  preview_wrap = false,  -- 预览是否软折行（Alt+W 临时切换）
  enable_trash = true,   -- Ctrl+D 是否可用
  placeholder = true,    -- 是否启用单行 `$|$` 填充位
}
```

想还原成"透明浮层"，把底色设成全透明并调暗边框即可：

```lua
config.snippet_panel.bg = '#33000000'   -- 半透明黑（ARGB）
```

字体用终端自己的字体与字号（不再借用命令面板的更大字号），所以面板里的列宽
和终端文本对得上。尺寸按**窗口像素尺寸 ÷ 单元格尺寸**算出来，再以字符为单位居中，
不会出现右侧顶到窗口边缘。

---

## 4. 实现位置

| 文件 | 内容 |
|---|---|
| `config/src/snippet.rs` | `SnippetPanelConfig`（新增） |
| `config/src/keyassignment.rs` | `ShowSnippetPalette` 变体 |
| `config/src/config.rs` | `snippet_panel` 字段 |
| `wezterm-gui/src/termwindow/list_preview.rs` | 通用"列表 + 预览"modal + 导航/高亮单测（新增） |
| `wezterm-gui/src/termwindow/snippet_palette.rs` | 扫描 / 解析 / 筛选 / 写文件 + 单元测试（新增） |
| `wezterm-gui/src/termwindow/mouseevent.rs` | 模态独占鼠标的拦截 + 坐标换算复用 |
| `wezterm-gui/src/termwindow/modal.rs` | `Modal::composed_text()`，把输入法上屏的文字交给模态 |
| `wezterm-gui/src/termwindow/keyevent.rs` | 模态期间吞掉所有按键（含 IME 上屏），不再漏给下面的 pane |
| `wezterm-gui/src/termwindow/mod.rs` | `ShowSnippetPalette` 分支挂 modal |
| `wezterm-gui/src/commands.rs` | `CommandDef` + 默认键 |
| `wezterm-gui/src/overlay/selector.rs` | `matcher_indices()`，给命中高亮用 |

`ListPreviewModal<S: ListPreviewSource>` 是通用的，片段只是第一个实现；以后做
告警列表或 AI 上下文选择器可以直接复用（实现 `source_name` / `generation` /
`filter` / `row` / `preview_lines` / `accept` / `footer` 即可）。

扫描发生在**打开面板时**（几百个小文件，毫秒级），不注册配置重载监视；
外部改完文件按 `Ctrl+R`。

`generation()` 是刷新机制：任何让底层数据改变的操作（rescan / 新建 / 删除 /
改元数据）都会 +1，modal 看到它变了就重跑 `filter`，所以列表和选中项总是新的。

---

## 5. 测试

```sh
. scripts/dev-env.sh
cargo test -p wezterm-gui              # 45 个，含 list_preview 与 snippet_palette
cargo +nightly fmt --all -- --check
```

覆盖：

- front matter 解析/改写、`---` 开头的 yaml 不被误判、`$|$` 与填充位位置、
  多行不触发填充位；
- `#tag` / `@dir` / `kind:` / `!` / `/body` 筛选、pin 与频率排序、trash 后从列表消失；
- **导航不崩**：`move_down` / `move_up` / `page` 各 100 次，空列表、单条、
  40 条都跑一遍（这条挡住过一个 `BorrowMutError` panic：`select()` 在同一语句里
  二次借用 `selected_row`）；
- 宽度工具在 CJK 下按显示宽度补齐；命中高亮只标自由文本词；
- 列表滚动不会越过最后一条（`clamp_top`）：否则底部会露出一片空白行，
  看起来像"新建了一条空片段"；
- `edit_line` 不得吞掉命令键（`Enter` / `Ctrl+H` / `Ctrl+I` / `Ctrl+J` 等）——
  这条挡住过一个真实回归：搜索框的兜底分支排在前面，把后面所有按键都吃掉了。

端到端验证（Xvfb + xdotool，2026-11-14 / 11-16 实测通过）：

1. `Ctrl+Alt+O` → 打 `pod` → `Enter`，读回命令行是
   `kubectl -n  get pod -o wide`，再打 `XX` 落在 `-n ` 之后 → 光标停在填充位；
2. 多行 yaml 片段插入后光标在末尾；
3. `Ctrl+Enter` 对普通片段会执行；对 `danger: true` 只插入；
4. `Ctrl+N` → 输入标题 → 文件出现在 `inbox/`，列表立刻多一条并选中它；
5. `Ctrl+D` → `y` → 文件移到 `.trash/<相对路径>`，列表立刻少一条；
6. `Ctrl+E` → 面板消失，右侧出现 `vim <片段路径>`；
7. `Ctrl+T` 依次改 title/tags/desc/pin/danger 时，未知字段与注释保留；
8. 方向键 ×20、PgUp/PgDn ×5、Home/End、空结果下按键：日志无 `panicked`；
9. 鼠标点列表行 → 选中条移动；拖过预览区 → 高亮保持；右键 → 写入 clipboard 并清除高亮；
   点面板外**不**关闭面板（只清除预览选区），点击不穿透到 pane。
10. `Ctrl+Alt+O` 再按一次 → 面板关闭（toggle）。
11. 无选区按 `Ctrl+N` → 不建文件，footer 出现提示；有选区时预览区显示草稿。
12. `Ctrl+W` 删除搜索框最后一个词（`deploy` → 空），列表随之恢复。
13. `Ctrl+H` 循环：单行 → 全部 → 隐藏（隐藏时面板少一行的分隔线和提示行，列表因此多两行）；
    有提示消息（如新建成功、复制成功）时即使处于隐藏状态也会临时显示，6 秒后自动收起。
14. `Ctrl+I` → 预览区元数据块（title/path/tags/desc/kind/cursor）整体隐藏，再按一次恢复；
    元数据是完整版，不是压缩成一行。
15. `Ctrl+J`/`Ctrl+K` 只在预览内容超过可视高度时移动；`Alt+W` 让长行折行。
16. 搜索框里 `Home`/`Ctrl+A` 把光标移到行首（`Xkubectl` 验证插入位置正确），
    `Ctrl+T` 的提示框同样支持左右移动光标后插入。

---

## 6. 已知限制

| 限制 | 说明 |
|---|---|
| 多行片段没有光标定位 | 要精确填空得做"面板内填空"，暂未实现 |
| 填充位之后若含双宽字符 | 一次左箭头只移一列，落点会偏右；文本本身完整 |
| 面板开着时不能拖选终端文本 | 模态独占鼠标是刻意的；面板里可以直接拖选预览区，或先关面板 |
| 命中高亮作用在标题和正文 | 元数据块保持原色 |
| front matter 只是 YAML 子集 | 见上；复杂结构放正文 |
| 面板不会自动刷新 | 外部改文件后 `Ctrl+R`（面板内新建/删除/改元数据是自动的） |
| 只有 `Ctrl+D` 一种删除 | 生成 `.trash/`，不会真删；恢复靠手动 mv |
| 空正文文件被跳过 | 扫描时 `parse_file` 直接丢弃，所以不会有"空标题行"；想保留就随便写一行注释 |
