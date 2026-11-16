<!-- 本文件只存在于 personal fork 的 local 分支，不属于上游 wezterm -->

# 片段面板：使用手册（速查）

> 功能设计/实现说明见同目录 [`snippets-panel.md`](snippets-panel.md)。
> 本文件只讲**怎么用、怎么配**，照抄即可用。

面板一句话：一个**居中的浮层**，左边是可搜索的片段列表，右边是选中片段的完整预览，
回车把正文插进当前 pane。

```
┌──────────────────────────────────────────────────────────────┐
│ snippets > pod            3/37                               │
├──────────────────────────────────────────────────────────────┤
│ ▌查看某命名...  k8s #pod      │ title:  查看某命名空间的 pod    │
│  标准 Deplo...  k8s #yaml     │ path:   k8s/pod-detail.snip     │
│  iptables...    net           │ tags:   k8s, pod, debug         │
│  ! 危险操作     #danger       │ desc:   宽格式列出               │
│                               │ kind:   command                 │
│                               │ cursor: column 12               │
│                               │ ─────────────                   │
│                               │ kubectl -n $|$ get pod -o wide   │
├──────────────────────────────────────────────────────────────┤
│ enter insert  ^enter run  ^e edit  ^w del-word  ^u clear ...  │
└──────────────────────────────────────────────────────────────┘
```

---

## 1. 30 秒上手

1. **打开面板**：`Ctrl+Alt+O`（`Ctrl+Alt+P` 被 fcitx5 的
   `[Hotkey/TogglePreedit]` 在 X11 层抢走了，用不了）。再按一次关闭。
   也可以在命令面板（`Ctrl+Shift+P`）里搜 "Snippet"。
2. **放入第一个片段**：把文件丢进 `~/.config/wezterm/snippets/`，比如

   ```bash
   mkdir -p ~/.config/wezterm/snippets/k8s
   cat > ~/.config/wezterm/snippets/k8s/pod-detail.snip <<'EOF'
   ---
   title: 查看某命名空间的 pod
   tags: [k8s, pod, debug]
   ---
   kubectl -n $|$ get pod -o wide
   EOF
   ```

   front matter 完全可以不写：没有 `title` 就用文件名，没有 `tags` 就不显示。
3. **打开面板 → 输入 `pod` → 回车**，命令行里就有了
   `kubectl -n  get pod -o wide`，光标停在本该填命名空间的位置。

想换打开键：

```lua
config.keys = {
  { key = 'o', mods = 'CTRL|ALT', action = wezterm.action.ShowSnippetPalette },
}
```

---

## 2. 按键一览

按 `Ctrl+H` 可以在 `隐藏 → 单行 → 全部` 之间循环底部提示行，忘了按键时按一下就到"全部"。

### 浏览与插入

| 键 | 作用 |
|---|---|
| `↑` `↓` | 上下选择 |
| `PgUp` `PgDn` | 翻页 |
| `Enter` | **插入**（不执行），关闭面板 |
| `Ctrl+Enter` | **插入并回车执行** |
| `Esc` | 关闭（有预览选区时先清选区） |
| `Ctrl+Alt+O` | 关闭（同打开键，toggle） |
| 鼠标双击列表行 | 插入（同 `Enter`） |

> `Ctrl+Enter` 有两种**自动降级**：`danger: true` 的片段、以及带 `$|$` 填充位的片段
> 只插入不执行 —— 因为还没填参数就执行是危险的 / 无意义的。降级时会有桌面通知说明。

### 搜索

| 键 | 作用 |
|---|---|
| 直接打字 | 模糊匹配 title / desc / tags / 相对路径；命中字符在列表的标题列和预览区里黄色高亮 |
| `Ctrl+W` | 删掉最后一个词（和 shell 一样） |
| `Ctrl+U` | 清空 |
| `←` `→` `Home` `End` | 移动光标（可以在词中间插字） |
| `Backspace` `Delete` | 删除 |
| `Ctrl+A` | 光标到行首 |
| 输入法 | 中文等词组可以直接上屏，进的是搜索框 |

### 管理

| 键 | 作用 |
|---|---|
| `Ctrl+N` | 用**当前选区**新建片段（先在面板里填标题） |
| `Ctrl+E` | 用 `$EDITOR` 在右侧 split 打开选中的片段文件（面板自动关闭） |
| `Ctrl+D` | 移到 `.trash/`（按 `y` 确认） |
| `Ctrl+T` | 逐项编辑元数据：title → tags → desc → pin → danger |
| `Ctrl+R` | 重新扫描目录（外部改过文件后按这个） |

### 外观

| 键 | 作用 |
|---|---|
| `Ctrl+H` | 底部按键提示：隐藏 / 单行 / 全部 |
| `Ctrl+I` | 开关预览区的元数据块 |
| `Alt+W` | 预览软折行开关 |
| `Ctrl+J` / `Ctrl+K` | 预览区上下滚动 |

### 鼠标

面板打开时**独占鼠标**——点击不会穿透到下面的终端。

| 操作 | 效果 |
|---|---|
| 左键点列表行 | 选中 |
| 左键拖过预览区 | 选中预览文本（松手后保持高亮，不自动复制） |
| **右键** | 复制预览选区到剪贴板，并清除高亮 |
| `Ctrl+C` | 同上（键盘方式） |
| 滚轮在列表 / 预览 | 移动选择 / 滚动预览 |

起点必须在有文字的地方：空行和行尾空白不能起选，免得复制出一堆看不见的空格。

---

## 3. 片段文件

### 目录结构

```
~/.config/wezterm/snippets/      # 默认位置，可用 config.snippet_panel.dir 改
├── k8s/
│   ├── pod-detail.snip
│   └── deploy.yaml              # 任意扩展名都行
├── net/iptables.snip
├── inbox/                       # Ctrl+N 生成，带时间戳文件名
└── .trash/                      # Ctrl+D 移入；点开头的目录不会被扫描
```

子目录会被递归扫描（最多 4 层），**点开头的目录/文件一律跳过**，
所以把不想看见的片段挪进 `.trash/` 或任何 `.xxx/` 都行。

### 文件格式

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

| 字段 | 类型 | 作用 |
|---|---|---|
| `title` | 字符串 | 列表里显示的名字；不写就用相对路径 |
| `desc` | 字符串 | 预览区的一行说明，也参与搜索 |
| `tags` | 列表 | `[a, b]` 或 `a, b`；参与搜索和高亮 |
| `kind` | 字符串 | `command` / `config` 之类，参与 `kind:` 过滤 |
| `danger` | 布尔 | 列表里标 `!`，`Ctrl+Enter` 不执行 |
| `pin` | 布尔 | 列表里标 `*`，空查询时排最前 |

细节：

- front matter 是**可选**的，判定规则是"文件第一行是 `---`，且闭合块里至少有一个认识的键"，
  所以以 `---` 开头的 k8s yaml **不会**被误判成元数据；
- 语法只支持这几种，不是完整 YAML：`key: value`、`key: [a, b]`、
  `true`/`false`/`yes`/`no`/`on`/`off`、双引号或单引号字符串；
- 不认识的键会被忽略，但 `Ctrl+T` 改写时**原样保留**，注释也保留；
- `kind` 不写时按扩展名猜：`.yaml` `.yml` `.json` `.toml` `.conf` → `config`，其余 `command`；
- **正文为空（或只有空白）的文件会被跳过**，不会在列表里留一个空标题行。

### 光标填充位 `$|$`

**单行**片段里的 `$|$` 是光标落点：预览里会显示出来（黄色），插入时被删掉，光标停在那里。

```
kubectl -n $|$ get pod -o wide   →   kubectl -n | get pod -o wide
```

- 用 `$|$` 而不是 `?`，因为 `$?`、PS1 里的 `?`、shell glob、URL query 都会撞；
- **多行片段不生效**：yaml/sql 模板里的 `$|$` 原样保留，光标留在末尾。
  多行要回跳到中间某行得靠发上箭头，在软折行、编辑器和双宽字符下都不准。
  插进 vim 之后用 `/name` 搜更快；
- 不想要这个功能：`config.snippet_panel.placeholder = false`；
- 原理是"粘贴完成后单独补发一轮左箭头 `\x1b[D`"。方向键在 bash/zsh/fish/
  vim 插入模式/psql/python REPL 里都表示左移一格；在不解析控制序列的程序里
  （`cat > file` 读 stdin）会多出 `^[[D`，这是唯一会污染文本的情形。

---

## 4. 搜索语法

空格分隔多个词，**全部满足**才命中（AND）：

| 输入 | 含义 |
|---|---|
| `kubectl` | 在 title / desc / tags / 相对路径上做模糊匹配 |
| `#k8s` | tags 或所在目录包含 `k8s` |
| `@net/` | 相对路径以 `net/` 开头 |
| `kind:config` | 类型过滤 |
| `!` | 只看 `danger: true` |
| `/nodePort` | 在**正文**里做子串匹配（默认不搜正文） |

可以混用：`k8s #debug /kubectl`。

只有自由文本词参与高亮，`#tag` / `@dir` / `kind:` / `!` 这类过滤词不标字。

**排序**：空查询时 `pin` 优先，其余按使用频率；有查询时按匹配得分，同分看使用频率。
使用记录存在 `$XDG_DATA_HOME/wezterm/snippet-frecency.json`（删掉就重来）。

---

## 5. 配置

全部配置项都挂在 `config.snippet_panel` 下，**都可以不写**（下面注释里是默认值）：

```lua
config.snippet_panel = {
  -- 目录
  dir = '/home/baili/.config/wezterm/snippets',  -- 默认 $XDG_CONFIG_HOME/wezterm/snippets

  -- 尺寸与布局
  width = 0.80,        -- 占终端列数比例（最少 40 列，四周留白）
  height = 0.62,       -- 占行数比例（最少 10 行）
  list_ratio = 0.40,   -- 列表列占比，剩下 60% 给预览

  -- 颜色
  bg = '#000000',            -- 面板底色；不透明黑，后面有内容也看得清
  fg = nil,                  -- 前景；nil = 用终端前景色（对比度不够时自动反色）
  selection_bg = '#3a3a4d',  -- 选中条底色
  selection_fg = '#f0f0f0',  -- 选中条文字色
  match_fg = '#ffd75f',      -- 搜索命中高亮色
  footer_fg = '#8bd8ff',     -- 底部按键提示颜色

  -- 预览
  preview_meta = true,   -- 是否显示元数据块（title/path/tags/desc/kind/cursor）
  preview_wrap = false,  -- 长行是否软折行
  keep_preview_rows = 3, -- 预览向下最多滚过末尾几行空白（0 = 最后一行停在底部）

  -- 面板行为
  footer = 'Short',      -- 底部按键提示初始状态：'Hidden' / 'Short' / 'Full'
  enable_trash = true,   -- 是否允许 Ctrl+D
  placeholder = true,    -- 是否启用单行 $|$ 填充位

  -- 通知
  desktop_notifications = false, -- 动作是否弹桌面通知（默认只写在面板底部）
  notify_timeout_ms = 10000,     -- 通知停留毫秒数
}
```

几个容易踩的点：

- **`preview_meta` 就是 `Ctrl+I` 那个开关的持久化版本**。`Ctrl+I` 只影响当前这一次打开，
  想每次打开都默认隐藏元数据就设 `preview_meta = false`（之后仍可用 `Ctrl+I` 临时打开）。
- `footer` 是**枚举，首字母必须大写**：`'Hidden'` / `'Short'` / `'Full'`。
  写成小写 `'hidden'` 会在启动时报
  `not a valid SnippetFooter variant. Did you mean 'Hidden'?`。`Ctrl+H` 仍然可以随时循环。
- `desktop_notifications = false` 时，**`danger` 片段的通知依然会弹**——那时面板已经关闭，
  底部提示看不见了，必须让你知道"插入了但没执行"。
- 想要透明浮层：`config.snippet_panel.bg = '#33000000'`（ARGB，这里是半透明黑）。

---

## 6. 常用配方

**把已有的 shell 历史当片段库**

```bash
# 从 history 里挑一行进 snippets，之后面板里搜得到
fc -ln -1 | sed '1s/^/---\ntitle: 常用命令\ntags: [shell]\n---\n/' \
  > ~/.config/wezterm/snippets/inbox/history.snip
```

更省事的做法：在终端里选中一行 → `Ctrl+Alt+O` → `Ctrl+N` → 回车。

**单行 vs 多行怎么选**

| 场景 | 建议 |
|---|---|
| 一行命令、中间要填参数 | 单行 + `$|$`，插入后直接接着打字 |
| yaml / sql / 多行脚本 | 多行，光标在末尾；插进 vim 后 `/关键字` 跳转 |
| 想插入后不执行 | `danger: true`，或者用 `Enter` 而不是 `Ctrl+Enter` |

**危险命令**

```markdown
---
title: 清理 Evicted pod
tags: [k8s, danger]
danger: true
---
kubectl get pods --all-namespaces | awk '$4=="Evicted"{print $2" -n "$1}' | xargs -r -n2 kubectl delete pod
```

列表里会显示 `!`，`Ctrl+Enter` 只插入不执行。

**给常用的置顶**

```markdown
---
title: vim 打开当前文件
pin: true
---
vim $|$
```

空查询时 `pin` 的片段总在最前面。

---

## 7. 排错

| 现象 | 原因 / 处理 |
|---|---|
| 按 `Ctrl+Alt+O` 没反应 | 被输入法或桌面环境占用：`Ctrl+Alt+P` 在 fcitx5 下就被抢了。换个键或在系统里解绑 |
| 面板里列表是空的 | 目录不对（`config.snippet_panel.dir`）或正文为空。按 `Ctrl+R` 重扫 |
| 新放进去的文件不出现 | 面板只在**打开时**扫描；`Ctrl+R` 或重开面板。点开头的文件/目录不会被扫描 |
| 中文标题列宽对不齐 | 已按显示宽度（CJK=2）处理；若仍异常请截图 |
| 插入后粘贴出 `^[[D` | 目标程序不解析方向键（如 `cat > file` 读 stdin）。关掉 `placeholder` 或避开这种场合 |
| `Ctrl+E` 打不开编辑器 | 需要设置 `$EDITOR`；没设时用 `vi` |
| 面板开着不能拖选终端文本 | 模态独占鼠标是刻意的。先在面板里拖选预览区，或关掉面板再选 |
| 打开面板后写配置报 `SnippetFooter variant` | `footer` 值要首字母大写：`'Hidden'` / `'Short'` / `'Full'` |
| 桌面通知不自动消失 | 部分通知守护进程不遵守 timeout。默认已关闭桌面通知 |

---

## 8. 完整可粘贴配置

```lua
local wezterm = require 'wezterm'
local config = wezterm.config_builder()

config.snippet_panel = {
  dir = '/home/baili/.config/wezterm/snippets',
  width = 0.80,
  height = 0.62,
  list_ratio = 0.40,
  bg = '#000000',
  selection_bg = '#3a3a4d',
  match_fg = '#ffd75f',
  footer_fg = '#8bd8ff',
  preview_meta = true,
  preview_wrap = false,
  footer = 'Short',
  enable_trash = true,
  placeholder = true,
  desktop_notifications = false,
}

config.keys = {
  { key = 'o', mods = 'CTRL|ALT', action = wezterm.action.ShowSnippetPalette },
}

return config
```
