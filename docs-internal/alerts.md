<!-- 本文件只存在于 personal fork 的 local 分支，不属于上游 wezterm -->

# 告警（regex 命中 / 命令失败 / 看门狗）：设计说明

> 本文先回答两个问题：**"PS1 显示退出码不就够了吗？"** 和
> **"N 分钟无输出这种看门狗到底有啥用？"**
> 结论会决定这件事值不值得改代码。

---

## 1. 命令退出码：你的质疑基本正确

### 1.1 PS1 方案的实际能力

在远端 `.bashrc` 里：

```bash
PROMPT_COMMAND='_s=$?; PS1="\[\e[38;5;$(( _s ? 196 : 46 ))m\]$ \[\e[0m\]\u@\h:\w\$ "'
```

或者更简单，只在失败时变红/变 `✘`。

这个方案的能力边界：

| 能做 | 不能做 |
|---|---|
| 在提示符上"看见"上一条命令成败 | 看不见**没有焦点 / 不在当前 tmux 窗口**的 pane |
| 在 tmux / ssh / 任何终端客户端里都生效 | 不能主动"推"（toast / 标题 / tab 变色） |
| 零 wezterm 改动、零编译 | 不能跨 pane 汇总；不能区分"这条指令失败"和"上一条"在滚动区里（一旦滚过去就只剩文本） |
| 延迟最低（命令结束即显示） | 无法区分"退出码 1 的正常业务返回"和真故障 |

**所以：如果诉求只是"我回头看一眼提示符，知道刚才那条失败了"，PS1 严格优于任何
wezterm 侧实现。** 这点你的判断是对的，不用改代码。

### 1.2 wezterm 侧唯一有增量价值的场景

只有一种：**你没在看那个 pane**。

- 12 个 pane 里跑批量巡检，其中一个失败；
- `kubectl logs -f` 挂了，但 pane 在别的 tmux 窗口；
- 训练任务里某台 worker OOM，日志滚过去了几百行。

这几种情况下你需要的是"**推**"：toast、tab 标题变色、状态栏计数。
PS1 做不到，因为它只能改自己那个提示符。

### 1.3 现状：退出码在 wezterm 里被丢掉了

查证结论（不是猜测）：

- shell integration **确实会发** `OSC 133;D;<status>;aid=<pid>`
  （`assets/shell-integration/wezterm.sh:482`：
  `printf "\033]133;D;%s;aid=%s\007" "$ret" "$$"`）；
- escape parser **确实解析了**它（`wezterm-escape-parser/src/osc.rs:743-746`
  → `FinalTermSemanticPrompt::CommandStatus { status, aid }`）；
- 但 `term/src/terminalstate/performer.rs:905-906` 是：

  ```rust
  OperatingSystemCommand::FinalTermSemanticPrompt(
      FinalTermSemanticPrompt::CommandStatus { .. },
  ) => {}
  ```

**退出码在这里被无声丢弃**，不进入 `Terminal` 模型，不产生 `Alert`，
Lua 侧完全拿不到。

要做"命令失败告警"，最小改动（约 30 行）：

1. `Terminal` 加 `last_command_status: Option<i32>`；
2. performer 那个空分支改成赋值 + `handler.alert(Alert::CommandStatus { status, pane })`；
3. `Alert` 枚举加一个 variant（`term/src/terminal.rs:47-74`）；
4. GUI 侧（`wezterm-gui/src/termwindow/mod.rs:1226` 附近那个大 match）加分支：
   `status != 0` → toast / 标题变色；
5. 顺手暴露 `pane:get_last_command_status()` 给 Lua。

> 注意：`tmux` 会**吞掉** OSC 133。实测：不包 passthrough 时外层终端收到 0 次；
> 包了 `\ePtmux;` 且 `allow-passthrough on` 才收到。
> 所以要用这个功能，远端 `~/.tmux.conf` 必须 `set -g allow-passthrough on`，
> 而且 shell integration 里的 OSC 133 输出也要包一层 —— 目前
> `wezterm.sh` 只给 `OSC 1337`（SetUserVar）包了 passthrough，OSC 133 没包。
> **这是这个功能真正的成本所在**，不是那 30 行 Rust。

---

## 2. "N 分钟无输出"为什么对你没用（你说得对）

"无输出"本身不是异常信号：

- 交互式 shell 等你想事，十几分钟没输出，完全正常；
- `top` / `tail -f` 无输出可能是好事；
- 反而是"**有**输出"更危险（刷屏掩盖了关键行）。

所以全局的"静默 N 分钟告警"只会制造噪音。**要么不做，要么做成"显式布防"。**

### 2.1 真正值得做的三类

| 类别 | 触发信号 | 为什么比"无输出"有用 |
|---|---|---|
| **流式输出断了** | 你对某个 `kubectl logs -f` / `tail -f` / `ssh` **显式布防**后，N 分钟无新增 | 这个 pane 本来就该持续有输出；断了说明 pod 死了 / 连接断了 / 进程卡死。**必须有"布防"动作**，否则就是噪音 |
| **进度不再推进** | 正则匹配到的数字（`step=(\d+)`）在 N 分钟内没变大 | 进程活着、还在打印，但训练卡了（NCCL 挂起、数据加载饿死）。这是 "静默" 唯一真正有价值的地方 |
| **命令失败** | 退出码非 0（见第 1 节） | 批量操作时最实用 |

第一、二类的关键区别：**布防范围由你指定，而不是全局**。
命令形式可以是：

```
:alert-arm --pane 3 --silence 300 --pattern 'step=(\d+)'
```

纯 Lua 就能做（不需要改代码），详见第 3 节。

### 2.2 你不该做的

- 全局"任何一个 pane 静默 N 分钟就告警"——必然被关掉；
- "有输出就告警"——更没用；
- 对 `vim` / `less` / REPL 做静默检测——它们本来就长期不动。

---

## 3. 纯 Lua 能做到什么（实测）

### 3.1 轮询点：`update-status`

`update-status` 每 `status_update_interval`（默认 1000ms）触发一次，
**与 tab bar 是否显示无关**（`wezterm-gui/src/termwindow/mod.rs:2110`
的 `schedule_next_status_update` 自己重排下一次），所以哪怕你把 tab bar 关了
（你的配置就是 `enable_tab_bar = false`）也照样 tick。

在 handler 里读 pane 文本是可行的，**包括非活动 pane**：

```
实测（split 成两个 pane，各自 echo 一行）：
  PANE1 id=0 as_text=[... echo HELLO-AAA | HELLO-AAA | prompt]
  PANE2 id=1 as_text=[... echo HELLO-BBB | HELLO-BBB | prompt]
```

用的是 `pane:get_lines_as_text(pane:get_dimensions().viewport_rows)`。
`get_text_from_region` 也能读，但会补齐大量空格。

### 3.2 边沿触发（必须做）

朴素的"每 tick 扫一遍、命中就告警"会**每 tick 都告警**：

```
实测（500ms tick，pane 里有一行 ERROR）：
  ALERT pane=1 rule=ERROR     ← 连着十几条，每秒两条
```

正确做法是记住"这个 pane 上一次是不是已经在这个状态"：

```lua
local RULES = { 'ERROR', 'FATAL', 'OOMKilled' }

wezterm.on('update-status', function(window, pane)
  local g = wezterm.GLOBAL
  g.seen = g.seen or {}
  for _, p in ipairs(window:mux_window():active_tab():panes()) do
    local id = tostring(p:pane_id())
    local d = p:get_dimensions()
    local ok, t = pcall(function() return p:get_lines_as_text(d.viewport_rows) end)
    if ok and t then
      local hit
      for _, k in ipairs(RULES) do
        if t:find(k, 1, true) then hit = k break end
      end
      local prev = g.seen[id]
      if hit and prev ~= hit then
        g.seen[id] = hit
        window:toast_notification('wezterm', 'pane ' .. id .. ': ' .. hit, nil, 3000)
      elseif not hit and prev then
        g.seen[id] = nil          -- 恢复正常，下次再命中会重新告警
      end
    end
  end
end)
```

实测：同样一行 ERROR，**只告警 1 次**；后续 tick 静默。

- `wezterm.GLOBAL` 用来存 `seen`，因为它跨配置热重载保留
  （配置会被非常频繁地重新求值，普通 Lua 全局变量会被清掉）。
- 用 `t:find(k, 1, true)`（plain find）而不是正则：快一个数量级。
  真需要正则时再 `t:find(pattern)`（Lua 模式，不是 PCRE）。

### 3.3 性能代价（实测，可信度有限但方向明确）

在 Xvfb 软件渲染下测 15 秒 CPU tick（`/proc/PID/stat` 的 utime+stime）：

| 配置 | ticks/15s |
|---|---|
| 空配置（1000ms status update） | 472 ~ 487 |
| 同上 + 每 tick 遍历 pane 读文本 | 964 ~ 1103 |

即**空闲 CPU 大约翻倍**。绝对数值不可信（Xvfb 软件 GL 本身就在空转），
但"每个 tick 把整个 viewport 的文本拉进 Lua 再扫一遍"这件事**确实不便宜**，
而且 tick 频率越高越贵。

可行的降低手段（都不改代码）：

- 用 `p:has_unseen_output()` 做前置闸门——但实测收益微弱
  （1103 → 964 ticks，仍在噪声内），因为 `true` 的机会很多；
- 把 `status_update_interval` 从 1000 调到 2000-3000（代价是延迟变长）；
- 只扫**你真正关心的那几个 pane**（用 `pane_id` 白名单，而不是遍历所有 pane）；
- 只读最后 2-3 行而不是整个 viewport：`pane:get_text_from_region(0, y, cols-1, y)`。

**结论：纯 Lua 轮询只适合"少量 pane + 低频率 + 简单子串匹配"。**
想做到"每个 pane 每 200ms 检查复杂正则"，应该改代码（下一节）。

### 3.4 零轮询的边沿：`bell`（实测可用）

```lua
wezterm.on('bell', function(window, pane)
  -- pane 里的程序发了 \a
end)
```

实测有效（`printf "\a"` → handler 收到 `pane=0`）。
这条路径不依赖 `update-status`，是真正的边沿触发。

不过它要求**远端程序主动响铃**，所以更适合做"远端侧告警"的接入口：
远端脚本检测到异常就 `printf '\a'`（或者 `printf '\033]9;消息\007'`，
wezterm 会把 OSC 9 变成桌面通知，见
`term/src/terminalstate/performer.rs:908-915`）。

> `OSC 9` 与 `OSC 777;notify` 都会走 `Alert::ToastNotification`。
> 实测它们**会被 tmux 吞掉**（不包 passthrough 时外层收到 0 次），
> 所以远端在 tmux 里要包 `\ePtmux;`。

---

## 4. 改代码的话，正确的架构是什么

**核心结论：告警必须挂在"输出事件"上，不能挂在"渲染"上。**

理由简单：窗口最小化 / 别的 workspace / 根本没在画的那个 pane，渲染代码根本不跑，
而告警恰恰最需要在这些时候工作。

可用的边沿：

| 边沿 | 位置 | 粒度 |
|---|---|---|
| `MuxNotification::PaneOutput(pane_id)` | `mux/src/lib.rs:122-128`（`send_actions_to_mux`，**每批解析出的 action** 触发一次，天然限流） | 每个 pane 每批 |
| `Alert::CommandStatus { status }` | 目前被丢弃（第 1.3 节） | 每条命令结束 |
| `Alert::Bell` | 已有 | 程序主动 |
| OSC 133 的 `MarkEndOfInputAndStartOfOutput` | `performer.rs:899-902` 已有处理 | 每条命令开始 |

推荐设计：

1. 新增 `config.alert_rules: Vec<AlertRule>`，字段
   `{ regex, name, cooldown_ms, scope = 'pane'|'tab'|'all' }`；
2. GUI 侧订阅 `MuxNotification::PaneOutput`，**只扫上次 seqno 之后变化的行**
   （`pane.get_changed_since(range, seqno)`，copy-mode / 渲染都在用这套）；
3. 用 `pane.get_current_seqno()` 做游标，天然避免重复扫；
4. 每 pane 维护 `last_fire: HashMap<rule_idx, Instant>`，冷却期去抖；
5. 命中 → `Alert::KeywordHit` → toast + tab 标题变色 + `update-status` 里可读计数；
6. 正则用 `regex` crate 的 `RegexSet`（比 `fancy_regex` 快一个数量级，且支持多规则一次扫）；
7. 看门狗（静默 / 进度不推进）作为**可选规则类型**，需要 `Instant` 记账：

   ```rust
   enum AlertRule {
       Pattern { regex, cooldown },
       Silence { after: Duration, armed_by: KeyAssignment },
       Progress { regex, every: Duration },
   }
   ```

---

## 5. 建议的落地顺序

| 步骤 | 内容 | 成本 | 价值 |
|---|---|---|---|
| 1 | 远端 `.bashrc` 的 PS1 失败染色 | 5 分钟，零代码 | ★★★（日常最高频） |
| 2 | 纯 Lua 边沿告警（第 3.2 节）+ 白名单 pane | 半个下午，零代码 | ★★（够用，CPU 可接受） |
| 3 | 改代码：`Alert::CommandStatus`（第 1.3 节） | ~30 行 Rust + 远端 tmux 配置 | ★★★（批量运维真正省事） |
| 4 | 改代码：`PaneOutput` 驱动的 `AlertRule` | 数百行 | ★★（替代第 2 步，长期正确） |
| 5 | 看门狗（静默 / 进度） | 在第 4 步之上 | ★（只有"显式布防"版本才有用） |

**不推荐现在做第 5 步**，除非你已经确认了"布防"的交互形态（谁布防、怎么解除、
超时怎么算）。否则做出来就是我们否掉的那个"全局静默 N 分钟"。

---

## 6. 一句话总结

- 退出码：**PS1 已经够了**，除非你要"推通知给没在看的 pane"——那才值得动 Rust。
- 静默看门狗：**全局版本无用**，只有"显式布防 + 期望进度"的版本有用。
- 纯 Lua：能做，边沿触发是必须的，代价是空闲 CPU 翻倍（实测）。
- 想低开销或要覆盖所有 pane：必须挂 `MuxNotification::PaneOutput`，那是代码活。
