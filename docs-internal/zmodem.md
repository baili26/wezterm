# ZMODEM (lrzsz `rz`/`sz`) support

<!-- 本文件只存在于 personal fork 的 local 分支，不属于上游 wezterm -->

`local` 分支的 Rust 功能：在 WezTerm 的 pane 里直接跑 `sz`/`rz` 传文件，
不要求远端换协议，也不要求本地安装 lrzsz。

设计/取舍讨论见规划稿；本文件偏向**实现细节 + 实测数据**，日常用法见
[`zmodem-usage.md`](zmodem-usage.md)。

**状态（2026-11-18）**：功能完整，日常可用。下载（远端 `sz`）、上传（远端 `rz`，
三种选文件方式）、进度/速率、tab 栏进度条、完成通知、取消、超时、断点续传、
`tmux -CC` 都已在真机验证；默认关闭，需要 `config.zmodem.enabled = true`。

代码分布（约 4.2k 行，其中 2.9k 是注释和测试）：

| 文件 | 行数 | 职责 |
|---|---|---|
| `mux/src/zmodem/detector.rs` | 514 | 纯字节状态机：识别握手、跨 chunk、剥前导/`OO` |
| `mux/src/zmodem/session.rs` | 1076 | `zmodem2` 驱动循环：落盘、进度、取消、超时 |
| `mux/src/zmodem/sink.rs` | 217 | 文件名净化、下载目录、断点续传落盘 |
| `mux/src/zmodem/mod.rs` | 1974 | 会话注册表、pane 内提示符、通知、zenity |
| `config/src/zmodem.rs` | 190 | 配置项 |
| `mux/tests/zmodem_roundtrip.rs` | 262 | 大文件双向端到端 |

测试：`mux` 53 个单测 + 3 个端到端，`config` 3 个。

---

## 1. 为什么现在才做，而且只能做在这里

上游 wezterm 从来没有 ZMODEM 支持（全仓库 `zmodem|lrzsz|trzsz` 零命中）。原因是架构性的：

- PTY 原始字节在读线程 `mux/src/lib.rs: read_from_pane_pty()` 读到，经 socketpair
  交给 `parse_buffered_data()`。
- `parse_buffered_data()` 持有的 `termwiz::escape::parser::Parser` **先把字节流变成
  `Action`**（`Print(char)` 已按 UTF-8 解码，控制字节变成 `ControlCode`），
  之后才进终端模型 `Performer`。
- ZMODEM 要的是 8-bit 原样流（ZDLE = 0x18，任意二进制），到 `Action` 这一层已经
  不可逆地损坏了。

所以实现点是：**`parser.parse()` 之前**，用一个纯字节的检测器嗅探触发串
（`mux/src/zmodem/detector.rs`），一旦命中，把这段字节流从 VT 解析器手里接管过来，
交给 `mux/src/zmodem/session.rs` 驱动协议引擎。

## 2. 上线实测：lrzsz 真实发出的字节

用 `script -q -c "sz /etc/hostname" /dev/null` 抓的真实握手（lrzsz 0.12.21rc）：

| 命令 | 收到的前若干字节 |
|---|---|
| `sz file` | `72 7a 0d` (`rz\r`) + `**\x18B` + `00000000000000` + `\r\n\x8a\x11` |
| `rz`      | `rz waiting to receive.` + `**\x18B` + `0100000023be50` + `\r\n\x8a\x11` |

即：`**` (ZPAD) `\x18` (ZDLE) `B` (ZHEX)，后跟 **14 个 hex**（5 字节载荷 + 2 字节 CRC16），
载荷第一个字节是帧类型：`00` = ZRQINIT（对端要发 → 我们**下载**），
`01` = ZRINIT（对端要收 → 我们**上传**），`08` = ZFIN。

注意几个容易踩的点：

- **是 14 个 hex 而不是 12**。trzsz-go 的正则写作 `\*\*\x18B0(0|1)[0-9a-f]{12}`，
  拆开看就是 `B0` + `0|1` + 12 位，总数一致。
- `sz` 前面那 3 个字节是普通文本 `rz\r`，必须正常交给 VT 解析器，否则用户看不到
  `sz` 的输出。检测器只把**触发串及其之后**的字节扣下来。
- lrzsz 会**周期性重发**握手。一次失败/取消后必须 `detector.reset()`，否则用户
  重新执行 `sz` 会毫无反应（触发串被漏掉）。

## 3. 协议引擎：`zmodem2` 0.7.3，并且**已实测互通**

`zmodem2` 是纯 Rust、`no_std`、调用方驱动 I/O 的状态机：

```rust
loop {
    match engine.poll() {
        Action::WriteWire(bytes) => { writer.write_all(bytes); engine.wire_written(n); }
        Action::WriteFile(bytes) => { file.write_all(bytes); engine.file_written(n)?; }
        Action::ReadFile{offset, max_len} => { /* seek+read */ engine.submit_file(&buf)?; }
        Action::Event(ev) => { /* FileStarted/FileCompleted/SessionCompleted/Aborted */ }
        Action::Idle => { /* 等新数据，或 engine.timeout() */ }
    }
}
```

### 互通验证结论（2026-11-17，本机 lrzsz 0.12.21rc）

写了一个旁路探针（独立 cargo 工程，不依赖 wezterm），用管道把引擎和真实 lrzsz 对打：

| 方向 | 结果 |
|---|---|
| `zmodem2::Receiver` ← `sz -b -B 32768 file` | 3 MiB 随机数据，字节完全一致 |
| `zmodem2::Sender` → `rz -b` | 3 MiB 随机数据，字节完全一致 |

配置：`Receiver::with_flow_control(0, true)`（广播 CANOVIO + 零缓冲，
远端 `sz` 才会连续流式发送；否则每 1 KiB 一个往返）。
上传方向**不能**用 `Sender::set_streaming_window(usize::MAX)`，见下一节。

`set_manual_file_accept(true)` 模式也跑通（`FileStarted` 里调 `accept_file_at(0)`），
这条路径让"名字不合规/磁盘写不进"可以干净地用 `skip_file()` 拒绝，
而不是在引擎已经把 ZRPOS 发出去之后才发现。

### 上传必须有界窗口（2026-11-24，380 MiB 卡死事故的根因之一）

`zmodem2::Sender` 在发出 `ReadFile` 请求到收到 `submit_file` 之间**完全不解析反向
通道**（`sender.rs:244-247`：`pending_request.is_some()` 时 `submit_wire` 直接
`break`）。而 `rz` 的 ZRINIT 实测是 `**\x18B0100000023be50`——缓冲长度 0 +
CANOVIO，于是 `usize::MAX` 的窗口让**整个文件变成一个 frame**，只有一个 ZDATA 头、
几万到几十万个 ZCRCG，最后才有一个 ZCRCW。后果：整个传输期间对端的 ZACK /
ZRPOS / ZCAN / ZFIN 一个都读不到；对端停止读取（子包校验失败要 ZRPOS、磁盘满、
被 kill）时我们毫无察觉，继续往里灌。

lrzsz 的 `sz` 不是这么干的：它每发一个子包就 `rdchk()` 采样一次反向流（规范 §9.1
"Full Streaming with Sampling"）。引擎做不到这件事，所以驱动层改用**有界窗口**：
默认 `upload_window_kib = 4096`，每 4 MiB 等一次 ZACK。代价是每 4 MiB 一个往返
（SSH 上可以忽略），换来的是对端异常在一个窗口内就能被发现。

`mux/tests/zmodem_roundtrip.rs::a_bounded_window_stops_the_sender_for_an_ack`
固定这条行为：窗口为 `usize::MAX` 时全文件只有 2 个 ZCRCW（ZFILE + 唯一的数据
frame），64 KiB 窗口时至少 16 个。

### 驱动循环里三条必须遵守的顺序规则

这三条都是"链路看起来卡死"而不是崩溃，很难从现象反推：

1. **先抽干 socket，再服务 action**。若"读一块→写一块→再等"，双方会互相等待。
2. **不要在 `Event` 上退出循环**。`poll()` 先返回排队的事件，再返回排队的待发字节，
   所以 `SessionCompleted` 之后还可能有一条 ZFIN/`OO` 没写出去。正确做法是
   记住 `saw_end`，直到 `Idle` 再退出。
3. **不要 flush writer，也不要跨 poll 持有 writer guard**。pty 写本来就是无缓冲；
   SSH shim 的 `flush()` 在出错时会阻塞等待新 writer，可能永远不返回。

## 4. 与 terminal model / GUI 的接口

| 需求 | 做法 |
|---|---|
| 进度条（tab 栏） | 往 pane 注入 `OSC 9 ; 4 ; 1 ; <pct>`（`mux::localpane::emit_output_for_pane`），终端模型已有 `ConEmuProgress` 处理与 `Alert::Progress` |
| 文本进度 / 汇总行 | 同上，注入 `\r\x1b[2K...` |
| 完成通知 | `MuxNotification::Alert { alert: ToastNotification {..} }`，复用 `frontend.rs` 的通知管线（自动遵守 `notification_handling`） |
| 按键抑制 | `Mux` 级注册表（`zmodem::is_active`），`LocalPane::key_down` / `send_paste` / `send_composed_text` 开头拦一层；Ctrl-C / Esc = 取消 |
| 上传选文件 | 拖拽（`WindowEvent::DroppedFile`）→ `zmodem::offer_upload`；否则 `zenity`；再否则 pane 内提示输入路径 |

**不需要**改 `term` crate：没有新增 `Alert` 变体，也没有新增 `set_progress`。

### 4.1 pane 内上传提示符

选文件的第三条路（没有拖拽、没有 zenity）是一个**画在 pane 里的行编辑器**。它不是
GUI 浮层，所以 `tmux -CC` 下同样能用。设计要点：

- **字节光标**，不是"最后追加"：`buf: String` + `cursor: usize`，光标永远落在 UTF-8
  边界上（`prev_char_boundary` / `next_char_boundary`）。这样 `Ctrl-A/E/K/D`、
  `Ctrl-W`、`Delete`、方向键的语义和 shell 完全一致。
- **竖线光标**：提示符期间发 `DECSCUSR`（`\x1b[6 q`）切到 bar，会话结束发
  `\x1b[0 q` 还原。方块光标会让 `Backspace` 看起来在删错东西，因为它盖在字符**上**。
- **只占一行**：`prompt_window()` 按**显示列**把 `prefix + buf` 切进一个单行窗口，
  两端用 `<`/`>` 标记被裁掉的部分，并且**从不写最右一列**（pending-wrap 会让下一次
  重绘落到新行）。这是必须的：重绘用 `\r` 只回到物理行首，一旦换行，擦除就擦错了行。
- **倒计时在 prefix 里**：`[45s]` 由 `tick_prompt()` 每秒重绘一次，值为
  `accept_window_secs - 握手到达至今`。
- 补全按**光标所在 token** 解析（不是最后一个 token），插入后光标跟到末尾；
  `cd` 的参数只补目录；多候选按列网格列出（最多 10 行）。

## 5. 安全边界

- 默认 `zmodem.enabled = false`。这是一个**远程可触发本地写文件**的协议，
  与 iTerm2 / MobaXterm 一样的风险模型。
- 额外受 `allow_download_protocols` 约束（与 OSC 1337 下载共用同一个开关）。
- 文件名净化与 OSC 1337 下载共用同一份实现（`mux/src/zmodem/sink.rs`）：
  剥路径、拒绝 `.`/`..`/含 `:`、`create_new` 冲突改名。**不**接受远端给的
  `mode`/`mtime`（不 chmod、不设置时间）。
- mux server 与 `wezterm cli proxy` 启动时调用
  `zmodem::set_local_file_access_allowed(false)`：它们的 pane 属于远端，
  下载文件应该落在用户机器上，而不是服务器磁盘上。这些场景下检测器直接不启用，
  终端保持原有行为。
- **唯一会发信号的代码路径**是 `zmodem::interrupt_peer()`：当对端已经退出 ZMODEM
  状态（握手后约 50 秒）、取消序列不再被理会时，它向**前台进程**发 `SIGINT`。
  边界刻意收得很紧：
  * 只在前台进程名**精确匹配** `rz`/`sz`/`lrz`/`lsz`/`trz`/`tsz`/`rb`/`sb` 时才发，
    所以「远端 `rz` 跑在本机 `ssh` 后面」这种情况（pgrp leader 是 `ssh`）**不会**
    被误杀；
  * 用 `kill(2)` 而不是往 tty 写 `0x03`，所以不会在 shell 里留下 `^C`；
  * 只在两个地方调用：上传倒计时到点、以及会话结束后的重发握手冷却期。
  没有其它 `kill`/`signal` 调用。

  `2026-11-24` 新增的第二条路径**不发信号**，只是给驱动一个"别把 GUI 拖死"的出口：

  * 每次协议写之前，若 `Pane::pty_master_fd()` 给出了 pty master 的 fd（本地 pane 与
    `wezterm ssh` 域都有；`WrappedSshPty` 返回 `None`），先 `poll(POLLOUT)` 最多
    `WAIT_WRITABLE`（10 s）。不可写就直接失败——**此时还没拿 writer 锁**，pane 完好，
    用户看到的是 `the peer stopped reading its input; transfer cancelled`。
  * 若写已经进了 kernel 才卡住（`write(2)` 无法取消），它会一直握着 pane 的 writer
    互斥锁——终端模型与每一次按键共享这把锁，整个 GUI 会冻住。这种情形下驱动
    `pane.kill()` 结束该 pane 的子进程：pty slave 关闭后阻塞的写以 EIO 返回，锁被释放。
    日志是 `write to pane N blocked for …s; ending the pane so the terminal cannot hang`。

  两条都只影响正在传输的那个 pane。

## 6. 边界与已知限制

| 场景 | 状态 | 说明 |
|---|---|---|
| 本地 pane / `wezterm ssh`（`multiplexing = 'None'`） | ✅ | 主用例 |
| 普通 `tmux`（非 -CC） | ❌ | tmux 是终端模型不是字节中继：它把 pane 输出解析进自己的 grid（`input_c0_dispatch` 消费 CAN 等 C0 字节），且 `allow-passthrough` **只有输出方向**、还要求 pane 内程序主动用 DCS 包裹。输入方向没有透传通道，ZMODEM 的 ACK 走不通。**这是 tmux 的设计约束，不是 WezTerm 能绕过的。** trzsz 之所以能在 tmux 里用，是因为它用 base64 文本 + 纯文本魔术头，不依赖 passthrough。 |
| `tmux -CC`（control mode） | ⚠️ 可用但慢 | `%output` 是原样 pane 字节、输入走 `send-keys -H`，保真；但每次输入都是一条 tmux 命令往返（`mux/src/tmux.rs` 的 `send_next_command` 一次一条且等 `%begin/%end`），1 KiB 子包一个往返意味着高延迟链路很慢 |
| `SSHMUX:` / unix domain（mux server 侧 pane） | ❌ | 原始字节只存在于 server 端；已在 server 侧硬关闭（见 §5），GUI 端是 `ClientPane`（`reader() == None`）拿不到字节 |
| 文件 > 4 GiB | ❌ | `zmodem2::Position` 是 `u32`；会被拒绝并提示 |
| 目录传输 | ❌ | ZMODEM 没有目录概念（trzsz 才有） |
| 上游 `zmodem2` 采样反向流 | ❌ | 引擎在 `pending_request` 期间不解析反向通道（`sender.rs:244-247`）；只能靠驱动层的有界窗口兜底，见 §3 |

## 7. 实测记录（2026-11-17，本机，Xvfb 无 WM）

| 用例 | 结果 |
|---|---|
| 下载 56 B / 200 KB | ✅ 字节一致，落到配置目录 |
| 下载单个 8 MB | ✅ 字节一致，约 2 秒 |
| 下载单个 50 MB | ✅ 字节一致，约 9 秒（跑满本地 pty） |
| 一次两个文件 `sz a b` | ✅ 两个文件都完整 |
| 上传单个 200 KB / 50 MB | ✅ 字节一致 |
| 一次两个文件（pane 内提示多路径） | ✅ 两个都发送 |
| 上传保留 mtime | ✅（`FileInfo::with_modified`，与真实 `sz`/`rz` 行为一致） |
| Ctrl-C 取消下载 | ✅ 对端 `sz` 立即收到 ZCAN（`Retry 0: Got ZCAN`），半截文件被删除，终端恢复正常 |
| 短超时（20 s）下的 8 MB 传输 | ✅ 按设计超时取消，不会卡死 |
| 关闭开关（`enabled = false`） | ✅ 行为与改动前一致（`sz` 输出仍是乱码，功能不介入） |
| `zmodem2` ↔ 真实 lrzsz 旁路探针（3 MB / 50 MB，双向） | ✅ 字节一致 |

**2026-11-18 补充验证**（第四、五轮修复之后，Xvfb + `wezterm cli get-text` 读屏）：

| 用例 | 结果 |
|---|---|
| 45 秒不发文件 | ✅ 倒计时走到 `[3s]` 后自动取消，`rz` 进程已退出，写 `the peer stopped accepting a transfer` |
| 提示符里 Ctrl-C | ✅ 显示 `transfer cancelled`，shell 上**没有** `^C`，无残留 `rz` 进程 |
| Ctrl-C 取消后立刻 `sz` | ✅ 5.6 KiB 下载成功，`cmp` 一致（修复前会被当成重发握手吞掉） |
| 提示符 `cd /u` + Tab + `X` | ✅ 变成 `cd /usr/X`，光标在末尾 |
| `←←Z` / `Ctrl-A` / `Ctrl-K` | ✅ 按行编辑语义工作 |
| 输入 250 字符 | ✅ 屏幕非空行数恒为 2；倒计时刷新期间也不增长 |
| 连按 100 次退格 | ✅ 仍为 2 行 |
| 大目录 `ls`（40 文件 + 2 目录） | ✅ 8 行多列，末行 `... (too many to show; keep typing)` |

### 实现过程中踩到并修掉的三个性能/正确性坑

1. **每轮都 `wait_readable(50ms)`**：即使已经有一大堆积压在 `wire` 里，也会先睡
   50 ms，导致 8 MB 要传半个多小时。修法：只有 `wire` 空了才做阻塞 poll；
   有积压时用 1 ms 的短等待让出 CPU。
2. **`Vec::drain(..n)` 逐子包搬移**：积压 50 MB 时每个 1 KB 子包都要 memmove 一遍，
   整体 O(n²)，50 MB 传不完就超时。修法：改成读游标 `wire_pos`，只在游标超过 1 MiB
   时压缩一次。
3. **`drain_available` 无上限抽干 socket**：会把整个文件先吞进内存（峰值 50 MB），
   也破坏了 socketpair 的背压。修法：每次迭代最多读 256 KiB。

### 关于结束行与 shell 提示符的竞争（已修）

第一版把"传输完成"的汇总行**异步**注入 pane（`emit_output_for_pane` 内部 `spawn_into_main_thread`）。
`sz`/`rz` 退出和 shell 重画提示符几乎同时发生，汇总行往往落在提示符**之后**、且没有结尾 `\r`，
光标停在行中间，看起来就像"必须再按一次回车才能回到命令提示符"。

修法：新增 `localpane::emit_output_for_pane_sync`，用一次性的 rendezvous channel 等主线程把
actions 消费完（带 500 ms 上限，主线程卡住也不会拖死传输），收尾的两行改走它。
实测 8 MB / 50 MB 下载后立刻键入命令即可执行，无需多余回车。

### 这一轮修掉的坑

1. **取消后被困在 rz 提示符**。`lrzsz` 收到 ZMODEM 取消序列会退出协议，但进程本身要等
   一个真正的 SIGINT；而取消后 `rz` 还会继续重发 `**\x18B01…` 握手，于是 WezTerm 立刻
   又开了一个新会话——看起来就是"怎么都退不出去"。三处一起修：
   * `Ctrl-C`/`Esc` 在发完取消序列后，额外向孩子写一个 `0x03`（TTY 会转成 SIGINT）；
     —— **这一步后来撤掉了**，见下面的第三、四轮：`0x03` 会在 shell 上留下 `^C`，
     而且 `rz` 变 raw 之后根本没用；
   * 取消/超时后的 20 秒内，**裸**握手（前面没有任何程序输出）只回取消，不新开会话；
     带程序输出的握手（用户真的又敲了一次 `rz`）正常开始；
   * `begin_session` 会清掉冷却期。
2. **阻塞的 pty 写会钉死读线程**。写改到辅助线程，带 15 秒上限（且不超过会话超时），
   超时就放弃本次会话——这样 Ctrl-C 永远有机会被处理。
3. **`ls`/`cd` 之后立刻传输会"卡住"**：提示符的等待计时没有因为浏览而刷新，
   用户慢慢挑文件时被判超时。现在每条提示符命令都刷新计时。
4. `Tab` 多候选时只补公共前缀、不列候选 → 现在两者都做；`ls` 支持通配符与单文件；
   提示符支持粘贴、`Ctrl-W/-U/-K/-A`。

### 第三轮（用户实测反馈）修掉的问题

| 现象 | 根因 | 处理 |
|---|---|---|
| 补全输出把终端刷屏 | 一行一个候选 | 改成列优先、最多 10 行的网格（`format_matches`），`ls` 也用同一套布局 |
| `cd` 的 Tab 把文件也列出来 | 候选没按命令区分 | `buffer_asks_for_dir()`：`cd` 的补全只列目录 |
| `ls`/`cd` 之后传输还是卡住 | 上传等待期间对端静默被算成"stalled" | 等待用户选文件时不计静默；另外发现 lrzsz 自己的硬限制：`rz` 约 50 秒后不再接受文件提议（实测 45s OK / 55s 失败），已在文档写明 |
| `Ctrl-A` 与 `Ctrl-W` 感觉一样 | 之前没有光标，Ctrl-A 只能"保留目录前缀" | 提示符加了真正的字节光标：`←/→/Home/End`、`Ctrl-A/E/K/D`、`Delete` 全部按行编辑语义工作，光标位置也会画出来 |
| `Ctrl-K` 删不掉 | 同上 | 同上 |
| 取消后 shell 上残留 `^C` | 我们多写了一个 ETX，`rz` 退出后留在 pty 输入队列被 shell 读走 | 去掉 ETX：ZMODEM 取消序列本身就足以让 lrzsz 退出（实测 0.1s） |
| 按键说明不全 | — | `help` 文本与文档同步更新 |

### 第四轮（用户实测反馈）

| 现象 | 根因 | 处理 |
|---|---|---|
| 50 秒硬限制只能靠用户记住 | — | 提示符显示 `[45s]` 倒计时（`accept_window_secs`，从**握手到达**起算），到点主动取消 |
| 等太久之后彻底卡住、`Ctrl-C` 无效 | `rz` 在 ~51 s 后退出 ZMODEM 状态，只打印裸 `C`，不再读输入；此时取消序列和 ETX 都没用（实测：写 `\x18`×10 + `\x08`×10 和写 `\x03` 都无反应，只有真 `SIGINT` 能结束它，实测 0.1 s 内退出） | 倒计时到点前就取消（取消序列还来得及）；真超时后由 `zmodem::interrupt_peer()` 对**确认是 lrzsz 的前台进程**发 `SIGINT`（白名单 `rz`/`sz`/`lrz`/`lsz`/`trz`/`tsz`/`rb`/`sb`，所以不会误杀 `ssh`），用 `kill(2)` 而非写字节，shell 上不会留 `^C` |
| 重发握手时还被塞了一个 ETX | 上一轮为"让 rz 死"补的 | 改为上面的 `interrupt_peer()`，`lib.rs` 的冷却分支里不再写 `0x03` |
| 光标画在字符**上**，退格删的不是光标左边的字符 | 用的是默认方块光标 | 提示符期间发 `DECSCUSR` 切到**竖线**（`\x1b[6 q`），会话结束发 `\x1b[0 q` 还原成用户配置的形状；列号改为 1 基（`CSI G` 第 1 列是最左边），并补了宽字符/UTF-8 边界的处理 |
| `cd /t` + Tab 之后光标停在 `t` 上 | 补全用 `truncate + push_str`，光标没跟着走 | 补全改成 `splice(buf, start, cursor, text)` 并返回新的光标位置；同时按**光标所在 token** 补全，而不是"最后一个 token" |
| 不给文件时空等两分钟 | 只靠 `upload_wait_secs` | 由 `accept_window_secs` 提前结束，并说明原因（`the peer stopped accepting a transfer`） |
| 取消 `rz` 后紧接着 `sz` 没反应，20 秒后才恢复 | `Feed.fresh` 是在**剥掉 `rz\r` 前导之后**才算的，而 `sz` 恰好只打印这 3 个字节，于是真 `sz` 被当成"对端在重发已取消的握手"，直接回了 CAN | 改成在剥前导**之前**判断（`spoke_first`）；补了单测 `sz_preamble_still_counts_as_fresh` 和 `reset_clears_state` 的对偶断言 |
| 输入超过屏宽后，每敲一个字 / 每个倒计时 tick 都刷出新行 | 重绘用 `\r` 回到**物理行**首，但 `prefix + buf` 已经换行到下一行，`\x1b[2K` 擦的是新行，旧行留在屏幕上 | 提示符改成**单行水平窗口**（`prompt_window`）：按显示列切窗口、光标保持在可见区，两端用 `<`/`>` 标出被裁掉的部分；同时**不写最右一列**，避免 pending-wrap 让下一次重绘落到新行 |

**回归断言**：`prompt_never_exceeds_one_row` 对 cols ∈ {20,40,79,80,81,200,1000} × 光标位置 ∈ {行首、中间、行尾} 逐一断言"绘制宽度 ≤ cols-1 且光标落在画出的行内"；`slice_cells_counts_columns_not_chars` 固定 CJK 双宽格的切片语义。真机验证：在 80 列 pane 里输入 200 个字符，`wezterm cli get-text` 统计的非空行数始终是 2（命令回显 + 提示符），倒计时刷新期间也不增长；`Home` 滚回行首、`Ctrl-E` 滚到行尾、连按 100 次退格后仍是 2 行。

### 第四轮实测（lrzsz 0.12.21rc，`rz` 与 `rz -b` 结果一致）

```
t= 0.0s  rz waiting to receive.**\x18B0100000023be50\r\x8a\x11
t=10.0s  **\x18B...        ← 重发握手
t=20/30/40s  同上
t=51.0s  C                 ← 退出 ZMODEM，开始每秒一个裸 C
t=62..150s  C
t=161.0s 十次 CAN + 十次 BS，进程退出
```

- **45 秒发取消序列 → 0.02 秒内退出**（`status=32768`，即被 SIGINT 终止的伪装），所以
  `accept_window_secs = 45` 留够了余量。
- **51 秒之后再发取消序列无效**：实测写 `\x18`×10 + `\x08`×10 无反应，写 `\x03`（ETX）
  也无反应——`rz` 这时已把 tty 设成 raw，ETX 不再产生 SIGINT。
- 只有真 `SIGINT` 有效：`kill(pid, SIGINT)` 后 0.1 秒内退出。

### 第六轮（2026-11-24）：380 MiB 上传卡死

用户实测：pane 里 `ssh` 到远端跑 `rz`，选 398,622,992 B 的文件，进度行停在
`zmodem: 2.1.331 - 0% of 380.2 MiB - 7.8 MiB/s`，约一分钟以后吐乱码、pane 死 / ssh 丢；
有时 pane 死有时不死。≤ 50 MB 的上传全部正常。

三条尺寸相关的缺陷叠加，全部在**驱动层**（`zmodem2` 本身不需要 patch）：

| 现象 | 根因 | 处理 |
|---|---|---|
| 卡住不动，对端早已停止接收也毫无察觉 | 上传窗口 `usize::MAX` + 引擎在 `pending_request` 期间不读反向通道 → 整个文件一个 frame，ZACK/ZRPOS/ZCAN 全都读不到 | `upload_window_kib`（默认 4096）有界窗口 |
| 无论对端是否正常，约 60 秒后必被判定为超时 | `stalled()` 只看读方向；上传是纯写流，对端按协议本就静默 | 两个方向任一有进展就不算 stall（`last_wire_write` 终于被用上；读文件也算进展） |
| 超时后 `rz` 永远停在 raw 模式、屏幕上出现乱码 | 上传的 stalled 分支只入队 `abort()`，没发取消序列；随后残余二进制交回 VT 解析器 | 所有上传中止路径统一走 `abort_upload()`（abort + flush CAN）；会话结束后 `drain_after_abort()` 在 2 s / 4 MiB 内吞掉残留，并保留期间出现的新握手交给检测器 |
| pane 有时永久冻死、GUI 卡住 | 每次 `WriteWire` 起一个线程（380 MiB ≈ 39 万次），写超时后丢弃线程，而它持有 pane writer 锁 | 一个会话一个常驻写线程；写超时改为结束该 pane（pty slave 关闭 → 阻塞写返回 EIO → 锁释放） |
| 传输中动鼠标把协议流写坏 | `LocalPane::mouse_event` 没有像 `key_down`/`send_paste` 那样被 ZMODEM 拦截 | 传输期间忽略鼠标事件 |
| 进度行看不出卡在哪 | 只有百分比与速率 | 在输出停滞 ≥2 s 时追加 ` - write blocked for Ns` |

回归测试：`stall_counts_progress_in_either_direction`、
`a_wedged_write_is_reported_instead_of_hanging`、
`writes_are_delivered_in_order_over_one_thread`、
`a_bounded_window_stops_the_sender_for_an_ack`、`upload_window_is_configurable`。

### 第七轮（2026-11-25）：本地 `rz` 正常、经 `ssh` 仍失败的真根因

第六轮修完，本地 pty + 真 `rz` 400 MiB 全部通过，但用户经 `ssh` 到 10.95.22.31 仍然
在 0–65% 之间掉线（`the peer stopped reading its input for 10s`，或 60 s 静默超时）。
用旁路探针（`/tmp/zmsshprobe`：本地 pty + 交互式 `ssh`，完全不经 wezterm）复现，确认
**与 wezterm 无关**，是 `ssh` 客户端的转义扫描：

> `ssh` 在把 pty 上的字节转发给远端之前，会扫描 **行首**（`\n` 之后）的转义字符
> `~`。`\n~.` 直接断连，`\n~^Z` 挂起本地 ssh。ZMODEM 的 `ZDLE_TABLE` 只转义 CR
> （0x0d），**不转义 LF**，所以 payload 里的任意 `\n~` 组合都会被 ssh 吃掉两个字节，
> 对端 CRC 失败 → `rz` 回 ZRPOS 并停读 → 我们看到的“对端不读了”。
> 380 MiB 里几乎必然出现这种组合；小文件概率极低，所以一直“小文件可以”。

修法：出方向写之前过一遍 `defuse_ssh_escape()`（`mux/src/zmodem/session.rs`）——
把**紧跟 `~` 的 LF** 以及**帧尾的 LF** 用 ZDLE 转义（`ZDLE, 0x0a ^ 0x40` = `18 4a`）。
这是协议本身就定义的转义，lrzsz `rz` 会原样还原；不需要转到 ZBIN，也不影响 CRC。
实测：

| 用例 | 结果 |
|---|---|
| 旁路探针，交互 `ssh` + `rz`，1 MiB payload 内植入 `\n~.` | 不转义：连接被 ssh 关闭（`Shared connection ... closed`），对端只收到 491520 B；转义后：**字节一致** |
| 同上，1 MiB 全 CR/LF 转义（保守方案） | ✅ 一致，说明 lrzsz 接受转义后的 LF |
| wezterm（v2）经 `ssh` 上传 398,622,992 B（用户那份 380.2 MiB 文件） | ✅ 35 s，两端 `md5` 相同 |
| wezterm（v2）经 `ssh` 上传 293 KiB / 32 MiB | ✅ `md5` 相同 |
| 本地 pty + 真 `rz` 400 MiB（第六轮用例） | ✅ 仍一致（帧里没有 `\n~` 时不做任何改写） |

回归测试：`ssh_escape_pairs_never_reach_the_wire`、`defuse_leaves_frames_without_line_breaks_alone`。

### 待补充
- [ ] `tmux -CC` 下的实测吞吐（尤其是 `send-keys -H` 的每条命令长度上限）
- [ ] zenity 在有桌面的真实会话里弹窗（Xvfb 无 WM 环境只验证了"失败后回退"）
- [ ] 断网 / 对端强杀时的超时与 CAN 行为（目前只验证了 Ctrl-C 与静默超时）
- [ ] Windows / macOS 实机
- [ ] 断点续传（`resume = true`）只跑过单测与旁路探针，没有在真机上中断过真实的
  `sz` 传输来验证"接着传"的效果
- [x] 380 MiB 上传经真实 `ssh`（10.95.22.31，远端 OpenSSH 7.4p1 / lrzsz 0.12.20）
  端到端通过：35 s，`md5` 一致
