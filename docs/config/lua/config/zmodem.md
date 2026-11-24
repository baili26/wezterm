---
tags:
  - zmodem
---
# `zmodem`

Settings for ZMODEM (`lrzsz` `rz`/`sz`) file transfer inside a pane.

This is a personal-fork feature; it is not part of upstream wezterm.
See [`docs-internal/zmodem.md`](https://github.com/baili26/wezterm/blob/local/docs-internal/zmodem.md)
for the design notes and measured behavior.

Defaults:

```lua
config.zmodem = {
  enabled = false,
  download_dir = nil,
  overwrite = false,
  show_progress = true,
  show_notification = true,
  timeout_secs = 60,
  upload_wait_secs = 120,
  upload_file_picker = 'Auto',
  accept_window_secs = 45,
  resume = false,
  upload_window_kib = 4096,
}
```

* `enabled` - master switch. Off by default: once enabled, a program running in
  a pane can make wezterm write files to local disk. Downloads are additionally
  gated on `allow_download_protocols`, the same switch that covers the iTerm2
  file-download escape sequence.
* `download_dir` - directory received files are written to. `nil` uses the
  platform download directory (usually `~/Downloads`). Unhelpful or hostile
  names from the peer are sanitized and collisions get a `.1`, `.2`, ... suffix.
* `overwrite` - overwrite an existing file of the same name instead of picking a
  fresh name.
* `show_progress` - print progress lines into the pane, including a live
  throughput figure and a per-file average on completion. The tab-bar progress
  indicator is updated either way.
* `show_notification` - raise a desktop notification when a transfer finishes.
  This only governs the ZMODEM toast; other notifications are still controlled
  by `notification_handling`.
* `timeout_secs` - how long the peer may go quiet before the transfer is
  cancelled (the cancel sequence is sent to the peer).
* `upload_wait_secs` - how long to wait for the user to choose files to send.
* `upload_file_picker` - where the upload file list comes from: `'Auto'` (native
  picker when a display is available, otherwise prompt in the pane), `'Native'`
  (native picker only) or `'Prompt'` (always prompt in the pane). Dropping files
  onto the window always works, regardless of this setting.
* `accept_window_secs` - how long the peer keeps accepting a file offer. lrzsz
  `rz` retries its handshake at 0/10/20/30/40 seconds and then stops listening
  for a file entirely (measured: it prints a bare `C` from 51 seconds and only
  exits at 161). The in-pane upload prompt counts down from this value and
  cancels while the peer can still be told to stop, so a slow file choice ends
  with a clear message instead of a stuck transfer. `0` disables the countdown.
* `resume` - if a partial file from an earlier attempt is in `download_dir`,
  ask the sender to continue from where it stopped (ZRPOS) instead of starting
  over.  Off by default because it reuses an existing file.
* `upload_window_kib` - how much data may be sent to an upload peer (`rz`)
  before waiting for its acknowledgement, in KiB (one subpacket is 1 KiB, so
  this is also the number of subpackets per window).  lrzsz `rz` advertises
  nonstop I/O, which lets the whole file go out as a single uninterrupted
  stream — and that makes a peer which stopped accepting data (a corrupt
  subpacket, a full disk, a killed `rz`) invisible until every byte has been
  written.  A bounded window costs one round trip per window, which is nothing
  over SSH, and turns that into one round trip of delay.  `0` restores the old
  unbounded behaviour (fastest on paper, but it cannot notice a dead peer).

```lua
local wezterm = require 'wezterm'
local config = wezterm.config_builder()

config.zmodem = {
  enabled = true,
  download_dir = '/home/me/incoming',
}

return config
```

Then, in a pane: `sz some-file` receives it into `download_dir`, and `rz`
prompts for local files to send.

The upload prompt resolves relative names against the directory the shell is
sitting in (the one you typed `rz` in), expands `~`, and accepts shell-style
globs, so `sub/notes.txt`, `~/report.pdf` and `*.log` all work. Dropping files
on the window does the same thing without typing.

The prompt has a few commands of its own - `ls [DIR]`, `cd [DIR]`, `pwd`,
`help` - which only affect the prompt, not the shell or the transfer. `Tab`
completes file names, `Up`/`Down` recall earlier lines, `Esc` cancels and
`Ctrl-C` aborts the transfer. The line is edited with a real cursor: arrows,
`Home`/`End`, `Ctrl-A`/`Ctrl-E`/`Ctrl-K`/`Ctrl-D`, `Ctrl-W` (delete the word
before the cursor) and `Delete` all behave the way they do in a shell, and the
cursor is drawn as a bar so it sits between characters instead of on one. The
prompt never wraps: a line wider than the pane scrolls horizontally with `<`
and `>` marking the hidden ends, so a long path cannot push the screen up.

The countdown in that prompt (`[45s]`) is the peer's own deadline, not a
wezterm invention: lrzsz `rz` stops listening for a file offer about fifty
seconds in, and from then on it ignores even the ZMODEM cancel sequence. If a
transfer does reach that point, wezterm sends a real `SIGINT` to the foreground
process - but only when that process is itself `rz`/`sz`/`lrz`/`lsz`/`trz`/
`tsz`/`rb`/`sb`, so a local `ssh` carrying a remote `rz` is never signalled.

Received files are written to `download_dir` and never overwrite an existing
file unless `overwrite` is set; a half-finished file left by a cancelled
transfer is removed.

ZMODEM cannot work inside a plain `tmux` pane (tmux consumes the binary stream
and only supports one-way output passthrough); it does work with `tmux -CC`.
