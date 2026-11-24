# ZMODEM 使用说明

<!-- 本文件只存在于 personal fork 的 local 分支，不属于上游 wezterm -->

在 WezTerm 里用 `sz`/`rz` 传文件。实现细节见 [`zmodem.md`](zmodem.md)。

一句话版本：远端照常敲 `sz 文件` 或 `rz`，WezTerm 接管协议，文件自动落盘 / 自动发出，
不需要在远端或本地额外装别的东西。

## 0. 速查

| 你想做的事 | 怎么做 |
|---|---|
| 从远端拿文件 | 远端 `sz 文件`（可多个、可 `*.log`） |
| 往远端送文件 | 远端 `rz`，然后拖文件 / zenity / 提示符里输路径 |
| 中途取消 | `Ctrl-C`（提示符里是 `Esc`） |
| 看还剩多久能挑文件 | 提示符末尾的 `[45s]` |
| 关掉这个功能 | `zmodem.enabled = false`（默认就是关的） |

## 1. 打开

```lua
config.zmodem = {
  enabled = true,
}
```

默认是**关闭**的：这是一个能被远端程序触发、往你本机写文件的协议。

## 2. 下载（远端 `sz`）

在远端（或本机任何 shell 里）：

```bash
sz 报告.pdf              # 单个文件
sz *.log                 # 多个文件
```

WezTerm 认出握手后接管字节流，文件落到 `~/Downloads`（可用 `download_dir` 改），
pane 里显示进度，tab 栏出现进度条，结束时弹一条通知。
重名不覆盖，自动变成 `报告.pdf.1`。

## 3. 上传（远端 `rz`）

```bash
cd /tmp && rz
```

WezTerm 会自动选一种方式让你挑本地文件：

1. **直接拖文件到窗口**（最快，也支持在敲 `rz` 之前就拖好）；
2. 有桌面环境时弹 **zenity** 多选文件框；
3. 以上都没有时，pane 里出现 `zmodem: files to send (in <目录>)> ` 提示。
   这个目录就是 `rz` 所在目录（你敲 `rz` 时所在的目录），相对路径按它解析；
   支持 `~/`、引号包住带空格的名字、以及 `*.log` 这种通配符。
   空回车会告诉你"可以输入什么"，而不是干巴巴报错。
   这个提示符自带几条命令：`ls [目录]`、`cd [目录]`、`pwd`、`help`；
   `Tab` 补全：唯一匹配直接补全，多匹配补到公共前缀并**按列**列出候选
   （最多 10 行，不会把提示符顶出屏幕）；`cd` 之后的 Tab 只列目录。
   `←`/`→`/`Home`/`End` 移动光标，`Ctrl-W` 删前一个词、`Ctrl-U` 清行、
   `Ctrl-K` 删到行尾、`Ctrl-A`/`Ctrl-E` 行首/行尾、`Ctrl-D`/`Delete` 删光标处字符，
   `↑`/`↓` 翻历史，可直接粘贴，`Esc` 取消，`Ctrl-C` 中止传输（不会再给 shell 留个 `^C`）。
   光标画成**竖线**（不是方块），落在两个字符之间，所以 `Backspace` 删的一定是光标左边
   那个字符；`Tab` 补全后光标停在插入内容的末尾，可以接着往下打。
   **提示符永远只占一行**：路径很长时它会像 shell 一样水平滚动，两端用 `<` / `>` 表示
   还有没显示出来的内容，`Home`/`Ctrl-A` 滚回行首、`End`/`Ctrl-E` 滚到行尾。
   提示符末尾带**倒计时** `[45s]`，见 §4。
   `ls` 支持通配符和单个文件：`ls *.png`、`ls z1.png`，输出同样是多列布局。

## 4. 传输过程中

- 按键会被吞掉（否则会把协议流打脏），**`Ctrl-C` 中止传输**；鼠标事件同样被忽略。
- 超过 `timeout_secs`（默认 60 秒）没有任何进展会发取消序列并恢复成普通终端。
  “进展”**算两个方向**：上传时对端本来就沉默，只要数据还在写出去就不算卡住——
  早期版本只看读方向，导致任何超过一分钟的上传都会被误判超时。
- 写不进去时不会拖死整个终端：先等 10 秒让对端把缓冲读走，仍不可写就取消传输、
  pane 照常可用；只有写已经卡在内核里（`write(2)` 不可取消）才结束该 pane，
  避免它一直占着 writer 锁把 GUI 冻住。
- 进度行显示实时速率；每个文件结束时显示该文件的大小/耗时/平均速率；
  全部结束时再打一行总计（总字节、总耗时、平均速率）。
- **取消之后不会被困在提示符里**：`Ctrl-C` 发协议取消序列后 `sz`/`rz` 会自己退出
  （实测约 0.1 秒），不需要按两次，也不会在 shell 上留下 `^C`。若对端仍在重发握手，
  WezTerm 在 20 秒冷却期内直接回取消序列而不重开会话；你自己重新敲 `rz` 则立即恢复。
- **挑文件时有 45 秒倒计时（`accept_window_secs`）**。这是 lrzsz 自己的硬限制：
  `rz` 在第 0/10/20/30/40 秒各发一次握手，之后就不再接受任何文件提议，只每秒打印一个
  `C` 干等（实测到第 161 秒才自己退出）。所以提示符会显示 `[37s]` 这样的剩余时间，
  到点主动取消并写 `the peer stopped accepting a transfer`——实测第 45 秒发取消序列，
  `rz` 在 0.02 秒内就退出了。倒计时**从握手到达时开始算**，`ls`/`cd` 浏览期间照常走，
  因为它量的是对端的时间。不想要倒计时可以设 `accept_window_secs = 0`。
  万一真等到超过 50 秒（例如 zenity 弹在别的屏幕上），`rz` 已经退出 ZMODEM 状态、
  连取消序列和 `^C` 都不认了，此时 WezTerm 会对**确认是 lrzsz 的前台进程**补一个
  `SIGINT`（白名单 `rz`/`sz`/`lrz`/`lsz`/`trz`/`tsz`/`rb`/`sb`，所以不会误杀 `ssh`；
  用 `kill(2)` 而不是打字，所以不会在 shell 上留 `^C`）。
- 对端已经有同名文件时 `rz` 会整份拒收（ZSKIP）。这时不会显示"已发送"，
  而是明确写 `was skipped by the peer`，并提示用 `sz -E` 覆盖。
  这是 `lrzsz` 自己的行为，不是 WezTerm 的判定。

## 5. 落盘、续传与通知

### 5.1 下载落盘规则

- 目录：`download_dir`，未配置则是系统下载目录（`~/Downloads`）。
- **重名不覆盖**：`报告.pdf` 已存在就写成 `报告.pdf.1`、`.2`……（想直接覆盖设
  `overwrite = true`）。
- 远端给的文件名会被净化：剥掉路径部分，拒绝 `.`/`..`/含 `:` 的名字。
  **不会** chmod，也**不会**沿用远端的权限位。
- 传了一半就取消/超时的文件会被删掉，不会留在目录里当垃圾——除非开了 `resume`，
  那时它会被保留下来供下次续传。

### 5.2 断点续传（`resume`）

`resume = true` 时，如果 `download_dir` 里已经有同名且更短的半截文件，WezTerm 会向
对端请求从那个位置继续（ZRPOS），而不是从头再来。`lrzsz` 的 `sz` 支持这个。
风险是"复用一个已存在的文件"，所以默认关闭。

### 5.3 桌面通知

传完会弹一条通知（内容含文件名和平均速率）。只关这一条用
`show_notification = false`；其它通知（shell 的 OSC 9、响铃等）仍由
`notification_handling` 统一管理。全部跳过的传输（`rz` 那边已有同名文件）字节数为 0，
不会弹通知。

## 6. 配置项

| 字段 | 默认 | 说明 |
|---|---|---|
| `enabled` | `false` | 总开关 |
| `download_dir` | `nil` | `nil` = 系统下载目录；写绝对路径 |
| `overwrite` | `false` | `false` 时重名自动加序号，不覆盖已有文件 |
| `show_progress` | `true` | `false` 只关掉 pane 里的文字进度（含速率），tab 栏进度条仍在 |
| `show_notification` | `true` | 传完是否弹桌面通知；只影响 ZMODEM 这一条，其它通知仍由 `notification_handling` 管 |
| `timeout_secs` | `60` | 多久没有任何进展算超时。**两个方向都算**：上传时对端按协议就是静默的，只要数据还在写出去就不算卡住 |
| `upload_wait_secs` | `120` | 等用户选文件的最长时间 |
| `accept_window_secs` | `45` | 对端还能接受文件提议的时长；提示符会倒计时，到点直接取消（`0` = 关闭倒计时） |
| `upload_file_picker` | `'Auto'` | `'Auto'` / `'Native'` / `'Prompt'` |
| `resume` | `false` | 断点续传：`download_dir` 里有半截文件就请求对端从断点继续（需要 `sz` 支持 ZRPOS） |
| `upload_window_kib` | `4096` | 上传时连续发多少 KiB 就停下来等对端确认（1 个子包 = 1 KiB）。`0` = 不等确认一口气流式发完，最快但看不见对端异常，见 §6.2 |

另外还受 `allow_download_protocols`（默认 `true`）约束——关掉它会同时禁用
iTerm2 的 `imgcat` 下载和 ZMODEM。

`upload_file_picker` 的枚举值是**大小写敏感**的：`'Auto'` / `'Native'` / `'Prompt'`，
写成小写 `'prompt'` 会报配置错误。

## 6.1 一个能直接用的最小配置

```lua
config.zmodem = {
  enabled = true,
  download_dir = '/home/me/incoming',   -- 省掉就用 ~/Downloads
  show_notification = true,
}
```

## 6.2 经 `ssh` 上传：为什么默认值就是能用的

`ssh` 客户端会扫描它**发给远端**的字节，行首（`\n` 之后）的转义字符 `~`
是它自己的快捷键：`~.` 断连、`~^Z` 挂起。ZMODEM 的编码只转义 CR、不转义 LF，
所以数据里随机的 `\n~` 会被 ssh 吃掉两个字节，远端 CRC 失败后回 ZRPOS 并停止
读取——现象就是进度条冻住、最后报 peer stopped reading。

这跟 lrzsz、跟 WezTerm 的协议实现都无关，任何“本地终端 → ssh stdin”的上传都有
这个风险；大文件必然中招（380 MiB 里期望出现数千次），小文件几乎碰不到。

WezTerm 现在在写出去之前把“紧跟 `~` 的 LF”和“帧尾的 LF”做一次 ZDLE 转义，
lrzsz 会原样还原，所以**默认配置就能经 ssh 传大文件，不需要改 ssh 设置**。

- 如果你的客户端把转义字符改成了别的（`EscapeChar X`），终端无从预知，仍建议对
  传输用的那台机器写 `EscapeChar none`（或 `ssh -e none`）。
- 下载方向（远端 `sz`）不受影响：字节走的是 ssh 的 stdout，不经过这段扫描。
- `upload_window_kib` 与这个问题无关，它是“多久停一次等确认”，默认 4096 KiB。
  设小（如 512）能更快发现对端异常，设 `0` 是旧的无界流式行为：速度不会更快
  （实测差异在噪声内），但会失去“对端出错 → ZRPOS 重传”这条恢复路径。

## 7. 不支持的场景

- **普通 `tmux` 里不行**。tmux 会解析并消费 pane 的输出（包括 ZMODEM 的控制字节），
  而 `allow-passthrough` 只支持输出方向、还要求 pane 内程序主动用 DCS 包裹。
  ZMODEM 的应答方向根本没有通道。想在 tmux 里传文件请用 trzsz（`trz`/`tsz`），
  它走纯文本协议，天生兼容 tmux。
- `tmux -CC` 可以用，但受 tmux 命令往返限制，比较慢。
- `SSHMUX:` 域名下的 pane（mux server）不支持：字节在服务器那一侧。
- 4 GiB 以上的文件、目录传输不支持。

## 8. 已知限制清单

| 限制 | 原因 | 能不能绕过 |
|---|---|---|
| 挑文件只有约 50 秒 | lrzsz 自己的重试预算（实测第 51 秒起就不再听） | 不能；提示符里有倒计时提醒 |
| 4 GiB 上限 | `zmodem2::Position` 是 `u32` | 不能；会明确拒绝并提示 |
| 不能传目录 | ZMODEM 协议没有目录概念 | 先 `tar` 再传 |
| 远端 history 里会留下 `sz xxx` / `rz` | 那是**远端 shell** 写的，终端改不了 | 远端 `HISTCONTROL=ignorespace` 后命令前加空格，或 `HISTIGNORE='sz *:rz'` |
| 普通 `tmux` 里不行 | tmux 只做输出方向 passthrough | 用 trzsz |
| `SSHMUX:` / unix domain 不行 | 原始字节只在 server 侧 | 在持有 PTY 的那一侧跑 |
