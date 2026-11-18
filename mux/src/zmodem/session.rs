//! The ZMODEM session driver.
//!
//! Runs on the pane's reader thread, immediately after the detector sees a
//! trigger sequence.  It owns the raw byte stream from that point until the
//! session finishes: bytes are handed to the `zmodem2` state machine, and its
//! `Action`s are serviced (write to the child, write to disk, read from disk).
//!
//! Ordering rules learned the hard way (see `docs-internal/zmodem.md`):
//!
//! * Drain everything already available on the socket before servicing
//!   actions.  Servicing first can leave both sides waiting on each other.
//! * `poll()` reports queued `Event`s *before* queued outgoing bytes, so a
//!   `SessionCompleted` event does not mean the final ZFIN/`OO` has been
//!   written.  Never stop on an event; stop on `Idle` after seeing one.
//! * Never hold the pane writer guard across a poll/read, and never flush it.
//! * Only accept files manually, so a local failure (bad name, no disk space)
//!   can be answered with `skip_file()` instead of a broken transfer.

use crate::pane::{Pane, PaneId};
use crate::zmodem::{sink, ZmodemHandle};
use filedescriptor::{poll, pollfd, AsRawSocketDescriptor, FileDescriptor, POLLIN};
use std::convert::TryFrom;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};
use zmodem2::{Action, Event, FileInfo, Position, Receiver, Sender};

const READ_CHUNK: usize = 64 * 1024;
/// How much to pull from the socket per loop iteration.  Reading the socket
/// dry would buffer the whole transfer in RAM (and defeat the backpressure the
/// 1 MiB socketpair provides); reading too little starves the engine.
const READ_BUDGET: usize = 256 * 1024;
const POLL_SLICE: Duration = Duration::from_millis(50);
/// How long a single protocol write may block before we treat the link as dead.
const WRITE_TIMEOUT: Duration = Duration::from_secs(15);
const PROGRESS_INTERVAL: Duration = Duration::from_millis(200);

/// What a completed session moved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completed {
    /// Files written (download) or sent (upload).
    pub files: Vec<PathBuf>,
    /// Files the peer refused to accept, so no bytes were sent.
    pub skipped: Vec<PathBuf>,
    /// Payload bytes moved, excluding protocol overhead.
    pub bytes: u64,
    /// Wall-clock duration of the session.
    pub elapsed: Duration,
}

/// How the session ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The session ran to completion.
    Completed(Completed),
    /// The user cancelled, or the peer cancelled.  `reason` is set when we
    /// gave up for a specific cause, so the caller can say *why* instead of
    /// printing a bare "cancelled" underneath the session's own message.
    Aborted { reason: Option<&'static str> },
    /// Something went wrong; the message is shown in the pane.
    Failed(String),
    /// The peer went quiet for `timeout`.
    TimedOut,
}

struct OpenFile {
    path: PathBuf,
    file: File,
    done: u64,
    total: u64,
    started: Instant,
    /// True only for the side that *created* the file (a download).  Cleanup on
    /// abort is restricted to that case: an upload opens a pre-existing file
    /// the user owns, and removing it would destroy their data.  This bug
    /// deleted local files whenever an upload was cancelled; never widen it
    /// without re-reading this comment.
    created_here: bool,
    /// Set once this file's transfer finished cleanly.
    keep: bool,
}

impl Drop for OpenFile {
    fn drop(&mut self) {
        if self.keep || !self.created_here {
            return;
        }
        let Some(meta) = self.file.metadata().ok() else {
            return;
        };
        if meta.len() == 0 {
            let _ = std::fs::remove_file(&self.path);
            return;
        }
        // Keep the partial unless the user asked for resume, in which case the
        // writer is responsible for parking it somewhere reusable.
        if crate::zmodem::resume_enabled() {
            return;
        }
        match std::fs::remove_file(&self.path) {
            Ok(()) => log::info!("zmodem: removed partial download {}", self.path.display()),
            Err(err) => log::warn!(
                "zmodem: could not remove partial download {}: {err:#}",
                self.path.display()
            ),
        }
    }
}

pub struct Session<'a> {
    pane_id: PaneId,
    pane: Weak<dyn Pane>,
    rx: &'a mut FileDescriptor,
    dead: &'a Arc<AtomicBool>,
    handle: Arc<ZmodemHandle>,
    timeout: Duration,
    show_progress: bool,
    finished_files: Vec<PathBuf>,
    /// Files the peer declined (ZSKIP), e.g. because it already has them.
    skipped_files: Vec<PathBuf>,
    wire: Vec<u8>,
    last_progress: Instant,
    progress_bytes: u64,
    last_activity: Instant,
    last_timeout_poke: Instant,
    /// When we last handed bytes to the child successfully.  Distinguishes
    /// "the peer stopped talking" from "our own write blocked".
    last_wire_write: Instant,
    payload_bytes: u64,
    /// When the first payload byte moved, for the average speed in the
    /// summary.  Deliberately not the session start: an upload may spend a
    /// while waiting for the user to pick files, and counting that time would
    /// report a speed that has nothing to do with the link.
    started_at: Option<Instant>,
    /// Exponentially smoothed throughput, so the progress line does not
    /// flicker between wildly different numbers tick to tick.
    speed_ema: f64,
    /// Read cursor into `wire`.  `Vec::drain` on a multi-megabyte backlog is an
    /// O(n) memmove per subpacket, which turns large transfers quadratic; we
    /// advance an index instead and compact only occasionally.
    wire_pos: usize,
}

impl<'a> Session<'a> {
    pub fn new(
        pane_id: PaneId,
        pane: Weak<dyn Pane>,
        rx: &'a mut FileDescriptor,
        dead: &'a Arc<AtomicBool>,
        handle: Arc<ZmodemHandle>,
        timeout: Duration,
        show_progress: bool,
    ) -> Self {
        let now = Instant::now();
        Self {
            pane_id,
            pane,
            rx,
            dead,
            handle,
            timeout,
            show_progress,
            finished_files: Vec::new(),
            skipped_files: Vec::new(),
            wire: Vec::new(),
            last_progress: now,
            progress_bytes: 0,
            last_activity: now,
            last_timeout_poke: now,
            last_wire_write: now,
            payload_bytes: 0,
            started_at: None,
            speed_ema: 0.0,
            wire_pos: 0,
        }
    }

    fn cancelled(&self) -> bool {
        self.handle.cancel_requested()
            || self.dead.load(Ordering::Relaxed)
            || self.pane.upgrade().is_none()
    }

    /// Print a line into the pane without disturbing the transfer.
    fn status(&self, text: &str) {
        crate::localpane::emit_output_for_pane(self.pane_id, text);
    }

    /// Throughput to show in the progress line.
    ///
    /// `done` is the byte count of the current file and `file_started` when
    /// that file began, so the number reflects this file rather than a
    /// lifetime average that includes idle time spent waiting for the user.
    fn update_speed(&mut self, done: u64, file_started: Instant) -> f64 {
        let elapsed = file_started.elapsed().as_secs_f64();
        if elapsed < 0.2 {
            return self.speed_ema;
        }
        let instant = done as f64 / elapsed;
        // Smooth so the line is readable: 30% new sample, 70% history.
        self.speed_ema = if self.speed_ema == 0.0 {
            instant
        } else {
            self.speed_ema * 0.7 + instant * 0.3
        };
        self.speed_ema
    }

    fn report_progress(
        &mut self,
        done: u64,
        total: Option<u64>,
        note: &str,
        file_started: Instant,
    ) {
        let grew = done.saturating_sub(self.progress_bytes);
        if grew < 64 * 1024 && self.last_progress.elapsed() < PROGRESS_INTERVAL {
            return;
        }
        self.last_progress = Instant::now();
        self.progress_bytes = done;

        let speed = self.update_speed(done, file_started);

        let pct = total
            .filter(|t| *t > 0)
            .map(|t| (done.saturating_mul(100) / t).min(100) as u8);

        // Tab-bar progress travels as OSC 9;4, which the terminal model already
        // understands and the tab bar already renders.
        match pct {
            Some(pct) => self.status(&format!("\x1b]9;4;1;{}\x1b\\", pct)),
            None => self.status("\x1b]9;4;3\x1b\\"),
        }

        if self.show_progress {
            let amount = match (pct, total) {
                (Some(pct), Some(t)) => format!("{}% of {}", pct, human_bytes(t)),
                _ => human_bytes(done),
            };
            self.status(&format!(
                "\r\x1b[2Kzmodem: {} - {} - {}/s",
                note,
                amount,
                human_bytes(speed as u64)
            ));
        }
    }

    fn clear_progress(&self) {
        self.status("\x1b]9;4;0\x1b\\");
        if self.show_progress {
            self.status("\r\x1b[2K");
        }
    }

    /// Write protocol bytes to the child's stdin.
    ///
    /// The guard is taken per call and dropped immediately: the same writer is
    /// used by the terminal model for replies and by the GUI for key input, so
    /// holding it across a poll would stall them.
    fn write_wire(&mut self, bytes: &[u8]) -> bool {
        self.write_wire_bytes(bytes)
    }

    fn write_wire_bytes(&mut self, bytes: &[u8]) -> bool {
        let Some(pane) = self.pane.upgrade() else {
            return false;
        };
        // A pty write blocks once the peer stops reading.  Do it on a helper
        // thread with a deadline so a wedged child cannot pin the pane's
        // reader thread (which is also the thread that services Ctrl-C).
        let (tx, rx) = std::sync::mpsc::channel();
        let payload = bytes.to_vec();
        let spawned = std::thread::Builder::new()
            .name(format!("zmodem-write-{}", self.pane_id))
            .spawn(move || {
                let result = {
                    // No flush(): a pty write is unbuffered already, and the
                    // SSH shim's flush can block waiting for a writer.
                    let mut writer = pane.writer();
                    writer.write_all(&payload)
                };
                let _ = tx.send(result);
            });

        if spawned.is_err() {
            log::error!("zmodem: could not spawn a writer thread");
            return false;
        }

        let wait = WRITE_TIMEOUT.min(self.timeout);
        match rx.recv_timeout(wait) {
            Ok(Ok(())) => {
                self.last_wire_write = Instant::now();
                true
            }
            Ok(Err(err)) => {
                log::error!("zmodem: write to pane {} failed: {err:#}", self.pane_id);
                false
            }
            Err(_) => {
                // The helper thread keeps the write pending; abandon it.  The
                // session aborts, and the pane can be killed normally.
                log::warn!(
                    "zmodem: write to pane {} blocked for {:?}; giving up on this session",
                    self.pane_id,
                    wait
                );
                false
            }
        }
    }

    /// Read everything currently available into `self.wire`.
    ///
    /// Returns false when the stream reported EOF/error.
    fn drain_available(&mut self) -> bool {
        let mut buf = vec![0u8; READ_CHUNK];
        let mut budget = READ_BUDGET;
        loop {
            if budget == 0 {
                return true;
            }
            let mut pfd = [pollfd {
                fd: self.rx.as_socket_descriptor(),
                events: POLLIN,
                revents: 0,
            }];
            match poll(&mut pfd, Some(Duration::from_millis(0))) {
                Ok(1) => {}
                Ok(_) => return true,
                Err(err) => {
                    log::error!("zmodem: poll on pane {} failed: {err:#}", self.pane_id);
                    return false;
                }
            }
            match self.rx.read(&mut buf) {
                Ok(0) => return false,
                Ok(n) => {
                    self.wire.extend_from_slice(&buf[..n]);
                    budget = budget.saturating_sub(n);
                    self.last_activity = Instant::now();
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => return true,
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(err) => {
                    log::error!("zmodem: read on pane {} failed: {err:#}", self.pane_id);
                    return false;
                }
            }
        }
    }

    /// Bytes not yet handed to the engine.
    fn buffered(&self) -> &[u8] {
        &self.wire[self.wire_pos..]
    }

    fn consume(&mut self, n: usize) {
        self.wire_pos += n;
        if self.wire_pos >= self.wire.len() {
            self.wire.clear();
            self.wire_pos = 0;
        } else if self.wire_pos >= 1024 * 1024 {
            self.wire.drain(..self.wire_pos);
            self.wire_pos = 0;
        }
    }

    fn has_buffered(&self) -> bool {
        self.wire_pos < self.wire.len()
    }

    fn wait_readable(&mut self, timeout: Duration) -> bool {
        let mut pfd = [pollfd {
            fd: self.rx.as_socket_descriptor(),
            events: POLLIN,
            revents: 0,
        }];
        matches!(poll(&mut pfd, Some(timeout)), Ok(1))
    }

    /// True when the peer has been silent for too long.
    fn stalled(&self) -> bool {
        self.last_activity.elapsed() > self.timeout
    }

    // ---- download -------------------------------------------------------

    pub fn run_download(mut self, seed: Vec<u8>) -> Outcome {
        let mut receiver = match Receiver::with_flow_control(0, true) {
            Ok(r) => r,
            Err(err) => return Outcome::Failed(format!("zmodem: {err}")),
        };
        // Manual accept lets a local failure decline a file cleanly.
        receiver.set_manual_file_accept(true);

        self.wire = seed;
        self.wire_pos = 0;

        let mut open: Option<OpenFile> = None;
        let mut saw_end = false;
        let mut cancelled = false;

        loop {
            if self.cancelled() && !cancelled {
                cancelled = true;
                let _ = receiver.abort();
                // Push the cancel sequence out immediately, otherwise the peer
                // keeps streaming until its own timeout.
                self.flush_cancel(&mut receiver);
                break;
            }

            if !self.drain_available() {
                if !saw_end {
                    let _ = receiver.abort();
                    if !cancelled {
                        return Outcome::Failed("zmodem: connection closed".to_string());
                    }
                }
                break;
            }

            loop {
                let consumed = {
                    let buffered = self.buffered();
                    match receiver.submit_wire(buffered) {
                        Ok(n) => n,
                        Err(err) => {
                            let _ = receiver.abort();
                            return Outcome::Failed(format!("zmodem: {err}"));
                        }
                    }
                };
                if consumed == 0 {
                    break;
                }
                self.consume(consumed);
            }

            let mut idle = true;
            loop {
                match receiver.poll() {
                    Action::WriteWire(bytes) => {
                        idle = false;
                        let n = bytes.len();
                        let ok = self.write_wire(bytes);
                        receiver.wire_written(n);
                        if !ok {
                            return Outcome::Failed("zmodem: lost the connection".to_string());
                        }
                    }
                    Action::WriteFile(bytes) => {
                        idle = false;
                        let n = bytes.len();
                        if let Some(entry) = open.as_mut() {
                            if let Err(err) = entry.file.write_all(bytes) {
                                let _ = receiver.abort();
                                return Outcome::Failed(format!(
                                    "zmodem: writing {}: {err}",
                                    entry.path.display()
                                ));
                            }
                            entry.done += n as u64;
                            self.payload_bytes += n as u64;
                            self.started_at.get_or_insert_with(Instant::now);
                            let note = entry
                                .path
                                .file_name()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .into_owned();
                            let (done, total, started) = (entry.done, entry.total, entry.started);
                            self.report_progress(
                                done,
                                Some(total).filter(|t| *t > 0),
                                &note,
                                started,
                            );
                        }
                        if let Err(err) = receiver.file_written(n) {
                            let _ = receiver.abort();
                            return Outcome::Failed(format!("zmodem: {err}"));
                        }
                    }
                    Action::Event(Event::FileStarted(info)) => {
                        idle = false;
                        match self.open_download(&info) {
                            Some((entry, offset)) => {
                                open = Some(entry);
                                if let Err(err) = receiver.accept_file_at(offset) {
                                    let _ = receiver.abort();
                                    return Outcome::Failed(format!("zmodem: {err}"));
                                }
                            }
                            None => {
                                // Refused (bad name, unwritable directory...).
                                if let Err(err) = receiver.skip_file() {
                                    let _ = receiver.abort();
                                    return Outcome::Failed(format!("zmodem: {err}"));
                                }
                            }
                        }
                    }
                    Action::Event(Event::FileCompleted) => {
                        idle = false;
                        if let Some(mut entry) = open.take() {
                            let _ = entry.file.flush();
                            let _ = entry.file.sync_all();
                            entry.keep = true;
                            self.progress_bytes = 0;
                            self.status(&format!(
                                "\r\x1b[2Kzmodem: received {} ({} in {}) at {}/s\r\n",
                                entry.path.display(),
                                human_bytes(entry.done),
                                human_duration(entry.started.elapsed()),
                                human_bytes(avg_speed(entry.done, entry.started.elapsed()))
                            ));
                            self.finished_files.push(entry.path.clone());
                        }
                    }
                    Action::Event(Event::SessionCompleted) => {
                        idle = false;
                        saw_end = true;
                    }
                    Action::Event(Event::Aborted) => {
                        if !cancelled {
                            return Outcome::Aborted { reason: None };
                        }
                    }
                    Action::Event(_) => {}
                    Action::Idle => break,
                    _ => break,
                }
            }

            if saw_end && idle {
                break;
            }

            if self.stalled() {
                log::warn!(
                    "zmodem: download stalled: wire={} open={:?} done={} wrote_to_disk={}",
                    self.wire.len(),
                    open.as_ref().map(|o| (o.done, o.total)),
                    self.progress_bytes,
                    self.payload_bytes,
                );
                let _ = receiver.abort();
                self.flush_cancel(&mut receiver);
                self.clear_progress();
                return Outcome::TimedOut;
            }

            // Keep the handshake alive while idle.
            if idle && self.last_timeout_poke.elapsed() > Duration::from_secs(2) {
                self.last_timeout_poke = Instant::now();
                let _ = receiver.timeout();
            }

            // Only sleep when there is nothing left in hand: the engine
            // accepts one subpacket per `submit_wire` burst, so waiting here
            // with a backlog would throttle the transfer to one subpacket per
            // poll slice (tens of KB/s).
            if !self.has_buffered() {
                self.wait_readable(POLL_SLICE);
            } else if idle {
                self.wait_readable(Duration::from_millis(1));
            }
        }

        self.clear_progress();
        if cancelled {
            Outcome::Aborted { reason: None }
        } else {
            Outcome::Completed(Completed {
                files: std::mem::take(&mut self.finished_files),
                skipped: std::mem::take(&mut self.skipped_files),
                bytes: self.payload_bytes,
                elapsed: self.started_at.map(|t| t.elapsed()).unwrap_or_default(),
            })
        }
    }

    fn flush_cancel_sender(&mut self, sender: &mut Sender) {
        loop {
            match sender.poll() {
                Action::WriteWire(bytes) => {
                    let n = bytes.len();
                    let ok = self.write_wire(bytes);
                    sender.wire_written(n);
                    if !ok {
                        break;
                    }
                }
                Action::Idle => break,
                _ => break,
            }
        }
    }

    fn flush_cancel(&mut self, receiver: &mut Receiver) {
        loop {
            match receiver.poll() {
                Action::WriteWire(bytes) => {
                    let n = bytes.len();
                    let ok = self.write_wire(bytes);
                    receiver.wire_written(n);
                    if !ok {
                        break;
                    }
                }
                Action::Idle => break,
                _ => break,
            }
        }
    }

    /// Prepare the destination for an incoming file.
    ///
    /// Returns the open file plus the offset to request from the peer: zero for
    /// a fresh transfer, or the size of a partial file left by an earlier
    /// attempt when resume is enabled.
    fn open_download(&mut self, info: &FileInfo) -> Option<(OpenFile, u32)> {
        let raw = String::from_utf8_lossy(info.name);
        let dir = match crate::zmodem::download_dir() {
            Ok(dir) => dir,
            Err(err) => {
                self.status(&format!("\r\x1b[2Kzmodem: {err}\r\n"));
                return None;
            }
        };
        let total = info.size.map(|s| s.get() as u64).unwrap_or(0);

        if crate::zmodem::resume_enabled() && total > 0 {
            if let Ok(partial) = sink::open_for_resume(&dir, Some(&raw), total) {
                let (path, file, have) = partial;
                if have < total {
                    self.status(&format!(
                        "\r\x1b[2Kzmodem: resuming {} at {} of {}\r\n",
                        path.display(),
                        human_bytes(have),
                        human_bytes(total)
                    ));
                    self.progress_bytes = have;
                    return Some((
                        OpenFile {
                            path,
                            file,
                            done: have,
                            total,
                            started: Instant::now(),
                            created_here: true,
                            keep: false,
                        },
                        have as u32,
                    ));
                }
            }
        }

        match sink::create_file(&dir, Some(&raw), crate::zmodem::overwrite()) {
            Ok((path, file)) => {
                self.status(&format!(
                    "\r\x1b[2Kzmodem: receiving {} ({})\r\n",
                    path.display(),
                    human_bytes(total)
                ));
                self.progress_bytes = 0;
                Some((
                    OpenFile {
                        path,
                        file,
                        done: 0,
                        total,
                        started: Instant::now(),
                        created_here: true,
                        keep: false,
                    },
                    0,
                ))
            }
            Err(err) => {
                self.status(&format!("\r\x1b[2Kzmodem: {err:#}\r\n"));
                None
            }
        }
    }

    // ---- upload ---------------------------------------------------------

    pub fn run_send(mut self, seed: Vec<u8>) -> Outcome {
        let mut sender = match Sender::new() {
            Ok(s) => s,
            Err(err) => return Outcome::Failed(format!("zmodem: {err}")),
        };
        sender.set_streaming_window(usize::MAX);

        self.wire = seed;
        self.wire_pos = 0;

        let mut queued: Option<Vec<PathBuf>> = None;
        let mut open: Option<OpenFile> = None;
        let mut saw_end = false;
        let mut cancelled = false;
        let mut asked_for_files = false;

        loop {
            if self.cancelled() && !cancelled {
                cancelled = true;
                sender.abort();
                self.flush_cancel_sender(&mut sender);
                break;
            }

            // Ask the user for files once.  Until they answer, keep servicing
            // the handshake so the peer does not give up.
            if !asked_for_files {
                asked_for_files = true;
                self.handle.begin_upload_prompt();
                crate::zmodem::spawn_upload_picker(self.pane_id);
            }

            if !self.drain_available() {
                if !saw_end {
                    sender.abort();
                    if !cancelled {
                        return Outcome::Failed("zmodem: connection closed".to_string());
                    }
                }
                break;
            }

            loop {
                let consumed = {
                    let buffered = self.buffered();
                    match sender.submit_wire(buffered) {
                        Ok(n) => n,
                        Err(err) => {
                            sender.abort();
                            return Outcome::Failed(format!("zmodem: {err}"));
                        }
                    }
                };
                if consumed == 0 {
                    break;
                }
                self.consume(consumed);
            }

            let mut idle = true;
            loop {
                match sender.poll() {
                    Action::WriteWire(bytes) => {
                        idle = false;
                        let n = bytes.len();
                        let ok = self.write_wire(bytes);
                        sender.wire_written(n);
                        if !ok {
                            return Outcome::Failed("zmodem: lost the connection".to_string());
                        }
                    }
                    Action::ReadFile { offset, max_len } => {
                        idle = false;
                        let Some(entry) = open.as_mut() else {
                            sender.abort();
                            return Outcome::Failed(
                                "zmodem: peer asked for data but no file is open".to_string(),
                            );
                        };
                        let off = offset.get() as u64;
                        if let Err(err) = entry.file.seek(SeekFrom::Start(off)) {
                            let msg = format!("zmodem: seeking {}: {err}", entry.path.display());
                            sender.abort();
                            return Outcome::Failed(msg);
                        }
                        let mut buf = vec![0u8; max_len];
                        let n = match entry.file.read(&mut buf) {
                            Ok(n) => n,
                            Err(err) => {
                                let msg =
                                    format!("zmodem: reading {}: {err}", entry.path.display());
                                sender.abort();
                                return Outcome::Failed(msg);
                            }
                        };
                        if n == 0 {
                            let msg = format!(
                                "zmodem: {} is shorter than advertised",
                                entry.path.display()
                            );
                            sender.abort();
                            return Outcome::Failed(msg);
                        }
                        entry.done += n as u64;
                        self.payload_bytes += n as u64;
                        self.started_at.get_or_insert_with(Instant::now);
                        let note = entry
                            .path
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned();
                        let (done, total, started) = (entry.done, entry.total, entry.started);
                        self.report_progress(done, Some(total).filter(|t| *t > 0), &note, started);
                        if let Err(err) = sender.submit_file(&buf[..n]) {
                            sender.abort();
                            return Outcome::Failed(format!("zmodem: {err}"));
                        }
                    }
                    Action::Event(Event::FileCompleted) => {
                        idle = false;
                        if let Some(mut entry) = open.take() {
                            entry.keep = true;
                            self.progress_bytes = 0;
                            if entry.done == 0 {
                                // The peer answered ZSKIP without asking for a
                                // single byte: lrzsz `rz` does this when the
                                // destination already exists.  Saying "sent"
                                // here would be a lie, and reporting "0 B at
                                // 0 B/s" was actively confusing.
                                self.status(&format!(
                                    "\r\x1b[2Kzmodem: {} was skipped by the peer \
                                     (it already has that file; use `sz -E` on the \
                                     remote side, or remove it there, to overwrite)\r\n",
                                    entry.path.display()
                                ));
                                self.skipped_files.push(entry.path.clone());
                            } else {
                                self.status(&format!(
                                    "\r\x1b[2Kzmodem: sent {} ({} in {}) at {}/s\r\n",
                                    entry.path.display(),
                                    human_bytes(entry.done),
                                    human_duration(entry.started.elapsed()),
                                    human_bytes(avg_speed(entry.done, entry.started.elapsed()))
                                ));
                                self.finished_files.push(entry.path.clone());
                            }
                        }
                        match queued.as_mut() {
                            Some(paths) if !paths.is_empty() => {
                                let next = paths.remove(0);
                                match self.begin_send_file(&mut sender, next) {
                                    Ok(entry) => open = Some(entry),
                                    Err(msg) => {
                                        sender.abort();
                                        return Outcome::Failed(msg);
                                    }
                                }
                            }
                            _ => {
                                let _ = sender.finish();
                            }
                        }
                    }
                    Action::Event(Event::SessionCompleted) => {
                        idle = false;
                        saw_end = true;
                    }
                    Action::Event(Event::Aborted) => {
                        if !cancelled {
                            return Outcome::Aborted { reason: None };
                        }
                    }
                    Action::Event(_) => {}
                    Action::Idle => break,
                    _ => break,
                }
            }

            // Files chosen by the user (drop, picker, or typed paths)?
            if open.is_none() && queued.is_none() {
                if let Some(mut paths) = self.handle.take_upload() {
                    if paths.is_empty() {
                        sender.abort();
                        self.clear_progress();
                        return Outcome::Aborted { reason: None };
                    }
                    let first = paths.remove(0);
                    match self.begin_send_file(&mut sender, first) {
                        Ok(entry) => {
                            open = Some(entry);
                            queued = Some(paths);
                        }
                        Err(msg) => {
                            sender.abort();
                            return Outcome::Failed(msg);
                        }
                    }
                } else if let Some(why) = self.handle.upload_gave_up() {
                    // Give up *before* the peer's window closes, so the
                    // cancel sequence still reaches it: `rz` ignores ZMODEM
                    // once it switches to printing a bare `C`, and would then
                    // sit there for another two minutes.
                    sender.abort();
                    self.flush_cancel_sender(&mut sender);
                    if self.handle.peer_left_zmodem() {
                        // Too late even for the cancel sequence; a SIGINT is
                        // the only thing this peer still reacts to.
                        crate::zmodem::interrupt_peer(self.pane_id);
                    }
                    self.clear_progress();
                    return Outcome::Aborted {
                        reason: Some(why.message()),
                    };
                } else {
                    // Keep the countdown in the prompt moving.
                    self.handle.tick_prompt();
                }
            }

            // A peer that has left ZMODEM prints one `C` a second and ignores
            // everything else, so this loop would otherwise spin until the
            // user's own `upload_wait_secs` expires.  Cancel as soon as the
            // deadline has passed; the countdown in the prompt has already
            // told the user.
            if self.handle.peer_left_zmodem() && open.is_none() && queued.is_none() {
                sender.abort();
                self.flush_cancel_sender(&mut sender);
                crate::zmodem::interrupt_peer(self.pane_id);
                self.clear_progress();
                return Outcome::Aborted {
                    reason: Some(crate::zmodem::UploadGiveUp::PeerStoppedAccepting.message()),
                };
            }

            if saw_end && idle {
                break;
            }

            // While the user is still choosing files, a quiet peer is
            // expected - `rz` only pings every ten seconds or so.  Counting
            // that as a stall cancelled transfers out from under anyone who
            // spent a while in `ls`, so the inactivity clock only applies once
            // the transfer is actually under way.
            let waiting_for_user = open.is_none() && queued.is_none();
            if waiting_for_user {
                self.last_activity = Instant::now();
            } else if self.stalled() {
                log::warn!(
                    "zmodem: upload stalled: payload={} backlog={} open={:?} last_read={:?}",
                    self.payload_bytes,
                    self.wire.len() - self.wire_pos,
                    open.as_ref().map(|o| (o.done, o.total)),
                    self.last_activity.elapsed(),
                );
                sender.abort();
                self.clear_progress();
                return Outcome::TimedOut;
            }

            if idle && self.last_timeout_poke.elapsed() > Duration::from_secs(2) {
                self.last_timeout_poke = Instant::now();
                let _ = sender.timeout();
            }

            // Only sleep when there is nothing left in hand: the engine
            // accepts one subpacket per `submit_wire` burst, so waiting here
            // with a backlog would throttle the transfer to one subpacket per
            // poll slice (tens of KB/s).
            if !self.has_buffered() {
                self.wait_readable(POLL_SLICE);
            } else if idle {
                self.wait_readable(Duration::from_millis(1));
            }
        }

        self.clear_progress();
        if cancelled {
            Outcome::Aborted { reason: None }
        } else {
            Outcome::Completed(Completed {
                files: std::mem::take(&mut self.finished_files),
                skipped: std::mem::take(&mut self.skipped_files),
                bytes: self.payload_bytes,
                elapsed: self.started_at.map(|t| t.elapsed()).unwrap_or_default(),
            })
        }
    }

    fn begin_send_file(&mut self, sender: &mut Sender, path: PathBuf) -> Result<OpenFile, String> {
        let meta =
            std::fs::metadata(&path).map_err(|err| format!("zmodem: {}: {err}", path.display()))?;
        if !meta.is_file() {
            return Err(format!("zmodem: {} is not a regular file", path.display()));
        }
        let size = meta.len();
        if size > u32::MAX as u64 {
            return Err(format!(
                "zmodem: {} is larger than 4 GiB, which ZMODEM cannot express",
                path.display()
            ));
        }
        let file = File::open(&path).map_err(|err| format!("zmodem: {}: {err}", path.display()))?;
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".to_string());
        let mut info = FileInfo::new(name.as_bytes(), Some(Position::new(size as u32)));
        // Preserve the modification time the way lrzsz does; without it the
        // receiver invents a bogus one.  Deliberately no mode: the remote end
        // should not get to chmod based on our input, and vice versa.
        if let Ok(modified) = meta.modified() {
            if let Ok(since) = modified.duration_since(std::time::UNIX_EPOCH) {
                if let Ok(secs) = u32::try_from(since.as_secs()) {
                    info = info.with_modified(secs);
                }
            }
        }
        sender
            .start_file(info)
            .map_err(|err| format!("zmodem: starting {}: {err}", path.display()))?;
        self.progress_bytes = 0;
        self.status(&format!(
            "\r\x1b[2Kzmodem: sending {} ({})\r\n",
            path.display(),
            human_bytes(size)
        ));
        Ok(OpenFile {
            path,
            file,
            done: 0,
            total: size,
            started: Instant::now(),
            created_here: false,
            keep: false,
        })
    }
}

/// Bytes per second, guarding against a zero duration.
pub fn avg_speed(bytes: u64, elapsed: Duration) -> u64 {
    let secs = elapsed.as_secs_f64();
    if secs < 0.001 {
        bytes
    } else {
        (bytes as f64 / secs) as u64
    }
}

/// Compact duration: `0.4s`, `12s`, `1m04s`, `1h02m`.
pub fn human_duration(d: Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 10.0 {
        format!("{:.1}s", secs)
    } else if secs < 60.0 {
        format!("{}s", secs as u64)
    } else if secs < 3600.0 {
        format!("{}m{:02}s", (secs as u64) / 60, (secs as u64) % 60)
    } else {
        format!(
            "{}h{:02}m",
            (secs as u64) / 3600,
            ((secs as u64) % 3600) / 60
        )
    }
}

pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", n, UNITS[unit])
    } else {
        format!("{:.1} {}", value, UNITS[unit])
    }
}
