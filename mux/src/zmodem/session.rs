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
/// How long the writer thread waits for the peer to drain before it takes the
/// pane's writer guard.
///
/// Short of the driver's `WRITE_TIMEOUT` so the decision to give up is made
/// while holding *nothing*: the mutex this check protects is the one the
/// terminal model and every keystroke need.  A peer that cannot accept one
/// 1 KiB subpacket in this long is wedged, not merely slow - ssh drains the pty
/// at link speed, and the driver stops every few MiB to wait for an ack anyway.
const WAIT_WRITABLE: Duration = Duration::from_secs(10);
const PROGRESS_INTERVAL: Duration = Duration::from_millis(200);
/// After an abort or a timeout, how long to keep swallowing the peer's bytes
/// before handing the byte stream back to the VT parser.  A peer that is still
/// streaming (or still retrying its handshake) would otherwise paint binary on
/// the restored prompt.  See `Session::drain_after_abort`.
const ABORT_DRAIN: Duration = Duration::from_secs(2);
/// Cap on the bytes discarded by `drain_after_abort`, so a peer that never
/// stops cannot hold the pane's parser thread for long.
const ABORT_DRAIN_BUDGET: usize = 4 * 1024 * 1024;
/// Quiet period that ends `drain_after_abort` early: the peer has nothing more
/// to say, so there is no more garbage to swallow.
const ABORT_DRAIN_QUIET: Duration = Duration::from_millis(300);
/// The byte every terminal multiplexer eats.
///
/// `ssh` scans the bytes it forwards to the remote pty for a line break
/// followed by its escape character (`~`, 0x7e).  After `\n~.` it closes the
/// connection; after `\n~^Z` it suspends itself.  Nothing in the protocol
/// forbids those two bytes from occurring inside a frame, and on a 380 MiB
/// payload the odds of a `\n~` pair are overwhelming - which is exactly why
/// uploads failed only over `ssh` and never against a local `rz`.
const LF: u8 = 0x0a;
const TILDE: u8 = 0x7e;
const ZDLE: u8 = 0x18;

/// ZDLE escape of a byte: the spec, and lrzsz's `zdlread`, decode the byte
/// after ZDLE as `byte ^ 0x40`.
const fn zesc(b: u8) -> u8 {
    b ^ 0x40
}

/// Rewrite an outgoing frame so an `ssh` client cannot mistake part of it for
/// its own escape sequence.
///
/// `ssh` watches the bytes it forwards to the remote pty for a line break
/// followed by its escape character (`~`).  `\n~.` closes the connection and
/// `\n~^Z` suspends the client, and the ZMODEM codec sends both a line feed
/// and a tilde verbatim, so an ordinary payload can end the transfer with no
/// protocol-level explanation.  On 380 MiB the odds of such a pair are
/// overwhelming - which is why this only ever showed up over `ssh`.
///
/// The rewrite is a ZDLE escape of the offending line feed, which every ZMODEM
/// receiver decodes back to the original byte (checked against a real `rz`).
/// A line feed at the very end of a frame is escaped too, because the next
/// frame's first byte could be the tilde that completes the pair.  Nothing
/// else is touched.
fn defuse_ssh_escape(bytes: &[u8]) -> Vec<u8> {
    let risky = |i: usize| bytes[i] == LF && matches!(bytes.get(i + 1), None | Some(&TILDE));
    if !(0..bytes.len()).any(risky) {
        return bytes.to_vec();
    }
    let mut out = Vec::with_capacity(bytes.len() + 8);
    for i in 0..bytes.len() {
        if risky(i) {
            out.push(ZDLE);
            out.push(zesc(bytes[i]));
        } else {
            out.push(bytes[i]);
        }
    }
    out
}

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
    /// Set by [`Self::drain_after_abort`] when the discarding phase saw a new
    /// handshake: the caller must not treat those bytes as lost, and should
    /// start the next session from them (see `SessionEnd::retrigger`).
    retrigger: Option<(crate::zmodem::Mode, Vec<u8>, bool)>,
    /// The pane writer, driven by one long-lived thread instead of a fresh one
    /// per protocol write (an upload emits ~380 000 writes for a 380 MiB
    /// file; spawning that many threads was both slow and, once one of them
    /// blocked, a permanent leak holding the pane's writer lock).
    writer: Option<WireWriter>,
}

enum WriteMsg {
    Data(Vec<u8>),
    Stop,
}

enum WriteResult {
    Written,
    /// The peer never became writable, so nothing was written and no lock was
    /// taken.  The session can give up with the pane still usable.
    Unwritable,
    Failed(String),
    /// A write is stuck inside the kernel, holding the pane's writer mutex.
    Blocked,
}

/// What the pane's parser thread needs to know once a session has finished.
#[derive(Debug, Default)]
pub struct SessionEnd {
    /// A new handshake arrived while we were discarding the abandoned stream's
    /// leftovers.  The parser thread must feed it to the detector path again
    /// instead of starting a session blindly, so that the abort cooldown still
    /// applies.
    ///
    /// The flag is the detector's `fresh`: whether a program printed something
    /// of its own before the signature.  A bare retry of a cancelled handshake
    /// is not fresh and must not reopen a session.
    pub retrigger: Option<(crate::zmodem::Mode, Vec<u8>, bool)>,
    /// True when the session ran to completion rather than being aborted,
    /// timed out or failed.
    ///
    /// Only then may the caller arm the detector's `OO` epilogue collector: the
    /// peer only prints it after a clean sign-off, and arming it after an abort
    /// would eat the first two `O` bytes of whatever comes next.
    pub completed: bool,
}

/// The pane, as far as the writer thread is concerned.
///
/// Abstracted so the tests can substitute a sink that blocks on demand: a real
/// `Pane` would need a whole pty to reproduce "the peer stopped reading".
trait WireSink: Send + Sync + 'static {
    /// Write one protocol frame.  Takes the pane's writer guard for the
    /// duration of the call and drops it immediately.
    fn write_all(&self, bytes: &[u8]) -> std::io::Result<()>;
    /// End the child.  Used when a write cannot be cancelled and the pane's
    /// writer mutex would otherwise stay locked for the rest of the process.
    fn kill(&self);
}

struct PaneSink {
    pane: Weak<dyn Pane>,
    /// Set for panes whose input fd we can poll (local and ssh panes).
    ///
    /// The fd is used only to ask "would a write block?" *before* taking the
    /// pane's writer mutex: a `write(2)` that blocks once the peer stops reading
    /// would otherwise hold that mutex - which the terminal model and every
    /// keystroke share - for as long as the peer stays stuck, freezing the whole
    /// GUI.  Polling first means the driver can give up while holding nothing.
    #[cfg(unix)]
    fd: Option<std::os::unix::io::RawFd>,
}

#[cfg(unix)]
fn wait_writable(fd: std::os::unix::io::RawFd, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        let mut pfd = [pollfd {
            fd,
            events: filedescriptor::POLLOUT,
            revents: 0,
        }];
        match poll(&mut pfd, Some(remaining.min(Duration::from_millis(250)))) {
            Ok(0) => continue,
            Ok(_) => {
                return pfd[0].revents & (filedescriptor::POLLERR | filedescriptor::POLLHUP) == 0
            }
            Err(_) => return false,
        }
    }
}

impl WireSink for PaneSink {
    fn write_all(&self, bytes: &[u8]) -> std::io::Result<()> {
        let Some(pane) = self.pane.upgrade() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "pane is gone",
            ));
        };
        // Give the peer a moment to drain before taking the writer guard.
        // `WAIT_WRITABLE` is much shorter than the driver's deadline: the point
        // is to fail *without* the mutex, not to wait forever holding it.
        #[cfg(unix)]
        if let Some(fd) = self.fd {
            if !wait_writable(fd, WAIT_WRITABLE) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "the peer is not reading its input",
                ));
            }
        }
        // No flush(): a pty write is unbuffered already, and the SSH shim's
        // flush can block waiting for a writer.
        let mut writer = pane.writer();
        // An `ssh` client scanning this pty for its escape character would
        // otherwise eat a `\n~` pair out of the payload; see
        // `defuse_ssh_escape`.
        writer.write_all(&defuse_ssh_escape(bytes))
    }

    fn kill(&self) {
        if let Some(pane) = self.pane.upgrade() {
            pane.kill();
        }
    }
}

/// A dedicated thread that owns the pane writer for the whole session.
///
/// The writer guard is still taken per write and dropped immediately, exactly
/// as before: the terminal model and key input share that mutex, and holding it
/// across a poll would stall them.  What changes is that the thread is created
/// once, and that a blocked write is reported rather than silently abandoned.
struct WireWriter {
    sink: Arc<dyn WireSink>,
    tx: std::sync::mpsc::SyncSender<WriteMsg>,
    result: std::sync::mpsc::Receiver<std::io::Result<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl WireWriter {
    fn spawn(sink: Arc<dyn WireSink>, pane_id: PaneId) -> Option<Self> {
        let (tx, rx) = std::sync::mpsc::sync_channel::<WriteMsg>(1);
        let (result_tx, result) = std::sync::mpsc::channel::<std::io::Result<()>>();
        let worker_sink = Arc::clone(&sink);
        let thread = std::thread::Builder::new()
            .name(format!("zmodem-write-{pane_id}"))
            .spawn(move || {
                while let Ok(msg) = rx.recv() {
                    let payload = match msg {
                        WriteMsg::Stop => break,
                        WriteMsg::Data(bytes) => bytes,
                    };
                    let outcome = worker_sink.write_all(&payload);
                    if result_tx.send(outcome).is_err() {
                        // The session gave up on us (it is about to kill the
                        // pane); keep going only until the channel closes.
                        break;
                    }
                }
            })
            .ok()?;
        Some(Self {
            sink,
            tx,
            result,
            thread: Some(thread),
        })
    }

    fn write_all(&self, bytes: &[u8], wait: Duration) -> WriteResult {
        if self.tx.send(WriteMsg::Data(bytes.to_vec())).is_err() {
            return WriteResult::Failed("the writer thread has exited".to_string());
        }
        match self.result.recv_timeout(wait) {
            Ok(Ok(())) => WriteResult::Written,
            Ok(Err(err)) if err.kind() == std::io::ErrorKind::WouldBlock => WriteResult::Unwritable,
            Ok(Err(err)) => WriteResult::Failed(format!("{err:#}")),
            Err(_) => WriteResult::Blocked,
        }
    }

    fn kill(&self) {
        self.sink.kill();
    }
}

impl Drop for WireWriter {
    fn drop(&mut self) {
        // Ask the thread to finish; do not join.  A write that is stuck inside
        // the kernel cannot be cancelled, and joining would put the very
        // hang this type exists to avoid back on the session thread.  The
        // session has already killed the pane in that case, so the write is
        // about to fail with EIO and the thread will exit on its own.
        let _ = self.tx.try_send(WriteMsg::Stop);
        if let Some(thread) = self.thread.take() {
            drop(thread);
        }
    }
}

/// What the pane's parser thread needs to know once a session has finished.
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
            retrigger: None,
            writer: None,
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
                "\r\x1b[2Kzmodem: {} - {} - {}/s{}",
                note,
                amount,
                human_bytes(speed as u64),
                self.wait_note()
            ));
        }
    }

    /// Why the transfer is momentarily not moving, if it is not.
    ///
    /// A frozen `0% of 380.2 MiB` used to be indistinguishable from "the link
    /// is slow"; saying what the driver is waiting for turns the next report of
    /// this kind into a diagnosis instead of a guess.
    fn wait_note(&self) -> String {
        let blocked = self.last_wire_write.elapsed();
        if blocked > Duration::from_secs(2) {
            format!(" - write blocked for {:.0}s", blocked.as_secs_f64())
        } else {
            String::new()
        }
    }

    fn clear_progress(&self) {
        self.status("\x1b]9;4;0\x1b\\");
        if self.show_progress {
            self.status("\r\x1b[2K");
        }
    }

    /// Write protocol bytes to the child's stdin.
    fn write_wire(&mut self, bytes: &[u8]) -> bool {
        self.write_wire_bytes(bytes)
    }

    fn write_wire_bytes(&mut self, bytes: &[u8]) -> bool {
        if self.writer.is_none() {
            // Ask the pane for a writable fd once; when it has one, protocol
            // writes bypass `Pane::writer()` and its mutex entirely.
            #[cfg(unix)]
            let fd = self.pane.upgrade().and_then(|pane| pane.pty_master_fd());
            let sink: Arc<dyn WireSink> = Arc::new(PaneSink {
                pane: Weak::clone(&self.pane),
                #[cfg(unix)]
                fd,
            });
            match WireWriter::spawn(sink, self.pane_id) {
                Some(writer) => self.writer = Some(writer),
                None => {
                    log::error!("zmodem: could not spawn the pane writer thread");
                    return false;
                }
            }
        }
        let wait = WRITE_TIMEOUT.min(self.timeout);
        let Some(writer) = self.writer.as_ref() else {
            return false;
        };
        match writer.write_all(bytes, wait) {
            WriteResult::Written => {
                self.last_wire_write = Instant::now();
                true
            }
            WriteResult::Failed(err) => {
                log::error!("zmodem: write to pane {} failed: {err:#}", self.pane_id);
                self.writer = None;
                false
            }
            WriteResult::Unwritable => {
                // The check before the write caught it, so nothing is holding
                // the pane's writer mutex: the terminal stays usable and the
                // session can end with a plain message.  This is the common
                // shape of "the remote rz stopped reading" once the upload
                // window is bounded.
                log::warn!(
                    "zmodem: pane {} did not accept input for {:?}; giving up on this transfer",
                    self.pane_id,
                    WAIT_WRITABLE
                );
                self.writer = None;
                false
            }
            WriteResult::Blocked => {
                // A write made it into the kernel before the peer went away, so
                // it is stuck holding the pane's writer mutex - shared with the
                // terminal model and every keystroke - and `write(2)` cannot be
                // cancelled.  End the child: when the pty's slave end closes the
                // blocked write fails with EIO, the thread exits and the lock is
                // released, so the GUI is never frozen.
                log::warn!(
                    "zmodem: write to pane {} blocked for {:?}; ending the pane so the terminal cannot hang",
                    self.pane_id,
                    wait
                );
                let writer = self.writer.take();
                if let Some(writer) = writer {
                    writer.kill();
                }
                false
            }
        }
    }

    /// Explain why a protocol write failed, in terms the user can act on.
    fn write_failure_message(&mut self) -> String {
        if self.pane_upgradable() {
            format!(
                "zmodem: the peer stopped reading its input for {}s; transfer cancelled",
                WAIT_WRITABLE.as_secs()
            )
        } else {
            "zmodem: lost the connection".to_string()
        }
    }

    /// True while the pane still exists, i.e. we did not end it.
    fn pane_upgradable(&self) -> bool {
        self.pane.upgrade().is_some()
    }

    /// Discard whatever the peer still has in flight, so it is not parsed as
    /// terminal output once the session hands the byte stream back.
    ///
    /// A `sz` that never saw our cancel sequence keeps streaming for a while,
    /// and that binary used to land on the restored prompt as garbage (and as
    /// a flood of "impossible C0/C1 control code" errors).  Swallow it here,
    /// but keep watching for a *new* handshake: if one shows up, the bytes from
    /// that point on belong to the next session and are handed back to the
    /// caller rather than eaten.
    fn drain_after_abort(&mut self) {
        let mut detector = crate::zmodem::ZmodemDetector::new();
        let mut buf = vec![0u8; READ_CHUNK];
        let deadline = Instant::now() + ABORT_DRAIN;
        let mut dropped = 0usize;
        let mut quiet_since = Instant::now();

        while Instant::now() < deadline && dropped < ABORT_DRAIN_BUDGET {
            let slice = POLL_SLICE.min(deadline.saturating_duration_since(Instant::now()));
            if !self.wait_readable(slice) {
                if quiet_since.elapsed() >= ABORT_DRAIN_QUIET {
                    break;
                }
                continue;
            }
            match self.rx.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    dropped += n;
                    quiet_since = Instant::now();
                    let feed = detector.feed(&buf[..n]);
                    if let Some(mode) = feed.trigger {
                        // The peer (or the user's next command) started a new
                        // handshake.  Hand the detector's buffered bytes to the
                        // caller: it decides whether the abort cooldown applies.
                        let seed = detector.take_seed();
                        self.retrigger = Some((mode, seed, feed.fresh));
                        break;
                    }
                    // Everything the detector did not claim is the abandoned
                    // stream, and is dropped on purpose.
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }

        if dropped > 0 {
            log::info!(
                "zmodem: pane {}: discarded {dropped} leftover bytes after the session ended",
                self.pane_id
            );
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

    /// True when the transfer made no progress for too long.
    ///
    /// An upload is a *write-only* stream: `rz` says nothing while a frame is
    /// in flight, so measuring silence on the read side alone declared every
    /// upload slower than `timeout` a stall - a 380 MiB file at 7.8 MiB/s takes
    /// 49 s against a 60 s default.  Progress in either direction counts.
    fn stalled(&self) -> bool {
        stalled(self.last_activity, self.last_wire_write, self.timeout)
    }

    // ---- download -------------------------------------------------------

    /// Run a download session and describe what the pane's parser thread needs
    /// to know afterwards.
    pub fn run_download(mut self, seed: Vec<u8>) -> (Outcome, SessionEnd) {
        let outcome = self.drive_download(seed);
        if !matches!(outcome, Outcome::Completed(_)) {
            // The peer may still be streaming (or still retrying a handshake we
            // gave up on).  Swallow that instead of letting the VT parser draw
            // it on the restored prompt.
            self.drain_after_abort();
        }
        let end = SessionEnd {
            retrigger: self.retrigger.take(),
            completed: matches!(outcome, Outcome::Completed(_)),
        };
        (outcome, end)
    }

    fn drive_download(&mut self, seed: Vec<u8>) -> Outcome {
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
                            return Outcome::Failed(self.write_failure_message());
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

    /// Run an upload session and describe what the pane's parser thread needs
    /// to know afterwards.
    pub fn run_send(mut self, seed: Vec<u8>) -> (Outcome, SessionEnd) {
        let outcome = self.drive_send(seed);
        if !matches!(outcome, Outcome::Completed(_)) {
            // `rz` that missed our cancel sequence keeps waiting in raw mode and
            // may print a retried handshake; the abandoned binary must not reach
            // the VT parser.
            self.drain_after_abort();
        }
        let end = SessionEnd {
            retrigger: self.retrigger.take(),
            completed: matches!(outcome, Outcome::Completed(_)),
        };
        (outcome, end)
    }

    /// Abort the upload *and* actually transmit the cancel sequence.
    ///
    /// `Sender::abort()` only queues the 10 CAN + 10 BS bytes; leaving them in
    /// the engine means the peer keeps waiting in raw mode forever, which is
    /// what made a failed upload look like a dropped SSH session.
    fn abort_upload(&mut self, sender: &mut Sender) {
        sender.abort();
        self.flush_cancel_sender(sender);
    }

    fn drive_send(&mut self, seed: Vec<u8>) -> Outcome {
        let mut sender = match Sender::new() {
            Ok(s) => s,
            Err(err) => return Outcome::Failed(format!("zmodem: {err}")),
        };
        // Bounded by default.  `zmodem2` does not read the reverse channel while
        // a file read is outstanding, so an unbounded window turns *every* peer
        // problem into "write a whole file into a dead link".
        sender.set_streaming_window(crate::zmodem::upload_window());

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
                self.abort_upload(&mut sender);
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
                    self.abort_upload(&mut sender);
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
                            self.abort_upload(&mut sender);
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
                            return Outcome::Failed(self.write_failure_message());
                        }
                    }
                    Action::ReadFile { offset, max_len } => {
                        idle = false;
                        let Some(entry) = open.as_mut() else {
                            self.abort_upload(&mut sender);
                            return Outcome::Failed(
                                "zmodem: peer asked for data but no file is open".to_string(),
                            );
                        };
                        let off = offset.get() as u64;
                        if let Err(err) = entry.file.seek(SeekFrom::Start(off)) {
                            let msg = format!("zmodem: seeking {}: {err}", entry.path.display());
                            self.abort_upload(&mut sender);
                            return Outcome::Failed(msg);
                        }
                        let mut buf = vec![0u8; max_len];
                        let n = match entry.file.read(&mut buf) {
                            Ok(n) => n,
                            Err(err) => {
                                let msg =
                                    format!("zmodem: reading {}: {err}", entry.path.display());
                                self.abort_upload(&mut sender);
                                return Outcome::Failed(msg);
                            }
                        };
                        if n == 0 {
                            let msg = format!(
                                "zmodem: {} is shorter than advertised",
                                entry.path.display()
                            );
                            self.abort_upload(&mut sender);
                            return Outcome::Failed(msg);
                        }
                        entry.done += n as u64;
                        self.payload_bytes += n as u64;
                        // Reading the file is progress too: the bytes are about
                        // to go out, and an upload whose file cursor is moving
                        // is not a stall even if the peer stays quiet.
                        self.last_wire_write = Instant::now();
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
                            self.abort_upload(&mut sender);
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
                                        self.abort_upload(&mut sender);
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
                        self.abort_upload(&mut sender);
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
                            self.abort_upload(&mut sender);
                            return Outcome::Failed(msg);
                        }
                    }
                } else if let Some(why) = self.handle.upload_gave_up() {
                    // Give up *before* the peer's window closes, so the
                    // cancel sequence still reaches it: `rz` ignores ZMODEM
                    // once it switches to printing a bare `C`, and would then
                    // sit there for another two minutes.
                    self.abort_upload(&mut sender);
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
                self.abort_upload(&mut sender);
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
                self.last_wire_write = Instant::now();
            } else if self.stalled() {
                log::warn!(
                    "zmodem: upload stalled: payload={} backlog={} open={:?} last_read={:?} last_write={:?}",
                    self.payload_bytes,
                    self.wire.len() - self.wire_pos,
                    open.as_ref().map(|o| (o.done, o.total)),
                    self.last_activity.elapsed(),
                    self.last_wire_write.elapsed(),
                );
                self.abort_upload(&mut sender);
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

/// True when neither side of the link moved for `timeout`.
///
/// Split out of `Session` so the rule is testable without a pane: an upload
/// only ever writes, so the read clock alone says "stalled" for every transfer
/// that outlives the timeout.
fn stalled(last_read: Instant, last_write: Instant, timeout: Duration) -> bool {
    last_read.max(last_write).elapsed() > timeout
}

// ---- tests ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;

    /// Stands in for a pty whose reader has stopped consuming.
    struct BlockingSink {
        /// Flipped to true to let a parked write finish.  An atomic rather than
        /// a mutex: the parked write must not hold the lock the test needs to
        /// release it, or the test would deadlock with itself.
        open: Arc<AtomicBool>,
        writes: AtomicUsize,
        killed: AtomicBool,
    }

    impl WireSink for BlockingSink {
        fn write_all(&self, _bytes: &[u8]) -> std::io::Result<()> {
            while !self.open.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(5));
            }
            self.writes.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        fn kill(&self) {
            self.killed.store(true, Ordering::Relaxed);
        }
    }

    fn sink(writer_blocked: bool) -> (Arc<AtomicBool>, Arc<BlockingSink>, WireWriter) {
        let open = Arc::new(AtomicBool::new(!writer_blocked));
        let sink = Arc::new(BlockingSink {
            open: Arc::clone(&open),
            writes: AtomicUsize::new(0),
            killed: AtomicBool::new(false),
        });
        struct Shared(Arc<BlockingSink>);
        impl WireSink for Shared {
            fn write_all(&self, bytes: &[u8]) -> std::io::Result<()> {
                self.0.write_all(bytes)
            }
            fn kill(&self) {
                self.0.kill()
            }
        }
        let writer = WireWriter::spawn(Arc::new(Shared(Arc::clone(&sink))), 1).unwrap();
        (open, sink, writer)
    }

    #[test]
    fn ssh_escape_pairs_never_reach_the_wire() {
        // `\n~.` and `\n~^Z` are ssh's "close the connection" and "suspend"
        // sequences; ssh only recognises them directly after a newline.  The
        // pair must not survive, and the bytes added must be ZDLE escapes that
        // lrzsz decodes back to the originals.
        let frame = b"\x00a\n~.\xffb\r\n~c\nd";
        let out = defuse_ssh_escape(frame);
        assert!(
            !out.windows(2).any(|w| w[0] == b'\n' && w[1] == b'~'),
            "a line feed must never be followed by a raw tilde: {:02x?}",
            out
        );
        assert_eq!(
            out.len(),
            frame.len() + 2,
            "only the two line feeds before a tilde are escaped"
        );

        // ZDLE + (byte ^ 0x40) is what the receiver decodes back to.
        let mut decoded = Vec::new();
        let mut it = out.iter().copied().peekable();
        while let Some(b) = it.next() {
            if b == 0x18 {
                if let Some(next) = it.next() {
                    decoded.push(next ^ 0x40);
                }
            } else {
                decoded.push(b);
            }
        }
        assert_eq!(decoded, frame);
    }

    #[test]
    fn defuse_leaves_frames_without_line_breaks_alone() {
        let frame = b"abcdef";
        assert_eq!(defuse_ssh_escape(frame), frame);
    }

    #[test]
    fn stall_counts_progress_in_either_direction() {
        let now = Instant::now();
        let timeout = Duration::from_millis(100);
        // Silence on both sides is a stall.
        let old = now - Duration::from_secs(5);
        assert!(stalled(old, old, timeout));
        // An upload writes but hears nothing: that is progress, not a stall.
        assert!(!stalled(old, now, timeout));
        // And a download reads while never writing.
        assert!(!stalled(now, old, timeout));
    }

    #[test]
    fn a_wedged_write_is_reported_instead_of_hanging() {
        let (open, sink, writer) = sink(true);
        let start = Instant::now();
        // The sink is parked, so the writer reports `Blocked` at the deadline
        // instead of blocking the caller forever.
        assert!(matches!(
            writer.write_all(b"payload", Duration::from_millis(50)),
            WriteResult::Blocked
        ));
        assert!(start.elapsed() < Duration::from_secs(5));
        assert!(!sink.killed.load(Ordering::Relaxed));

        // The session ends a pane whose write cannot be cancelled, so the
        // blocked write fails and the writer mutex is released.
        writer.kill();
        assert!(sink.killed.load(Ordering::Relaxed));

        // Let the parked write finish so the thread can be reaped.
        open.store(true, Ordering::Relaxed);
    }

    #[test]
    fn writes_are_delivered_in_order_over_one_thread() {
        let (open, sink, writer) = sink(false);
        open.store(true, Ordering::Relaxed);
        for i in 0..64u8 {
            assert!(matches!(
                writer.write_all(&[i], Duration::from_secs(5)),
                WriteResult::Written
            ));
        }
        assert_eq!(sink.writes.load(Ordering::Relaxed), 64);
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
