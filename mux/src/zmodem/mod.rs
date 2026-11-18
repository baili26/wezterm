//! ZMODEM (lrzsz `rz`/`sz`) file transfer support.
//!
//! This is a personal-fork feature; see `docs-internal/zmodem.md`.
//!
//! # Why this lives here
//!
//! Pane output reaches the terminal model only after being decoded by a VT
//! escape parser (`mux/src/lib.rs`, `parse_buffered_data`), and that decoding
//! destroys binary data.  ZMODEM is a raw 8-bit stream protocol, so it has to
//! be intercepted *before* the parser - which is exactly the seam this module
//! plugs into.
//!
//! # Shape
//!
//! * [`detector`] watches for the lrzsz handshake in the incoming byte stream
//!   and decides whether the terminal should act as receiver or sender.
//! * [`session`] drives the `zmodem2` state machine for the duration of the
//!   transfer, writing files to disk or reading them, and reporting progress.
//! * [`sink`] owns the untrusted-name-to-local-path rules, shared with the
//!   iTerm2 download path in `wezterm-gui`.
//!
//! This module also keeps a per-pane registry of live sessions so that key
//! input can be suppressed (and `Ctrl-C` turned into an abort) while a
//! transfer is in flight.

pub mod detector;
pub mod session;
pub mod sink;

use crate::pane::{Pane, PaneId};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

pub use detector::{Feed, Mode, ZmodemDetector};

lazy_static::lazy_static! {
    static ref SESSIONS: Mutex<HashMap<PaneId, Arc<ZmodemHandle>>> = Mutex::new(HashMap::new());
    /// Panes whose last session ended badly, and when.
    ///
    /// lrzsz keeps retrying its handshake for a while after a cancel, so without
    /// this a cancelled `rz` immediately re-triggers a new upload prompt - and
    /// the user cannot get back to their shell.  Triggers inside the window are
    /// answered with a cancel sequence instead of a new session.
    static ref RECENT_ABORT: Mutex<HashMap<PaneId, Instant>> = Mutex::new(HashMap::new());
    /// Set to false in processes that must not touch the local filesystem on
    /// behalf of a remote peer (the mux server, `wezterm cli proxy`).
    static ref LOCAL_FILE_ACCESS: AtomicBool = AtomicBool::new(true);
}

/// Whether this process may write files requested by a remote peer.
///
/// A WezTerm-multiplexed session runs the pane in the *mux server*; accepting
/// a download there would write the client's files on the server's disk.  The
/// mux server and proxy binaries turn this off at startup.
pub fn set_local_file_access_allowed(allowed: bool) {
    LOCAL_FILE_ACCESS.store(allowed, Ordering::Relaxed);
}

pub fn local_file_access_allowed() -> bool {
    LOCAL_FILE_ACCESS.load(Ordering::Relaxed)
}

/// Is the feature enabled by configuration right now?
pub fn enabled() -> bool {
    let config = config::configuration();
    config.zmodem.enabled && config.allow_download_protocols && local_file_access_allowed()
}

pub fn download_dir() -> anyhow::Result<PathBuf> {
    match config::configuration().zmodem.download_dir.clone() {
        Some(dir) => Ok(dir),
        None => sink::default_download_dir(),
    }
}

pub fn overwrite() -> bool {
    config::configuration().zmodem.overwrite
}

fn timeout() -> Duration {
    Duration::from_secs(config::configuration().zmodem.timeout_secs.max(1))
}

fn upload_wait() -> Duration {
    Duration::from_secs(config::configuration().zmodem.upload_wait_secs.max(1))
}

/// How long the peer keeps accepting a file offer, or `None` when the
/// countdown is turned off.
///
/// lrzsz `rz` retries its handshake for fifty seconds, then stops listening
/// for a file and spins on a bare `C` until its own give-up timer expires
/// (measured: five handshakes at 0/10/20/30/40s, first bare `C` at 51s, exit
/// at 161s).  Offering a file after that point cannot work, so the prompt
/// counts down and cancels rather than letting you pick files for a transfer
/// that is already impossible.
fn accept_window() -> Option<Duration> {
    let secs = config::configuration().zmodem.accept_window_secs;
    if secs == 0 {
        None
    } else {
        Some(Duration::from_secs(secs))
    }
}

fn show_progress() -> bool {
    config::configuration().zmodem.show_progress
}

pub fn resume_enabled() -> bool {
    config::configuration().zmodem.resume
}

/// Shared state between the transfer (reader thread) and the input path (GUI
/// thread), keyed by pane.
pub struct ZmodemHandle {
    pub pane_id: PaneId,
    cancel: AtomicBool,
    /// When the session was registered, i.e. when the peer's handshake was
    /// seen.  The accept-window countdown is measured from here, not from when
    /// the prompt appeared: the peer's clock started when it printed its
    /// handshake.
    created: Instant,
    upload: Mutex<UploadState>,
}

/// Session-local prompt state: the working directory the user is browsing and
/// the key history.
struct PromptInput {
    buf: String,
    /// Byte offset of the cursor inside `buf`.  Always on a char boundary.
    cursor: usize,
    /// Directory the prompt resolves relative names against.  Starts at the
    /// shell's cwd; `cd` inside the prompt changes it for this session only.
    cwd: PathBuf,
    /// Previously submitted lines, most recent last.
    history: Vec<String>,
    /// Index into `history` while browsing with Up/Down.
    history_pos: Option<usize>,
    /// Ambiguous completions from the last Tab, printed after the repaint.
    last_matches: Vec<String>,
}

impl PromptInput {
    /// Replace the whole line and put the cursor at its end, the way typing
    /// would.  Tests use it to set up a line without having to keep `cursor`
    /// in sync by hand.
    #[cfg(test)]
    fn set_line(&mut self, line: &str) {
        self.buf.clear();
        self.buf.push_str(line);
        self.cursor = self.buf.len();
    }
}

/// How long lrzsz keeps listening for a file offer, measured from when its
/// handshake appears.  Beyond this the cancel sequence is ignored, so a peer
/// that is still running has to be signalled.
const LRZSZ_ACCEPT_LIMIT: Duration = Duration::from_secs(50);

/// Why the upload should be abandoned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UploadGiveUp {
    /// The peer's handshake acceptance window has passed.  lrzsz `rz` stops
    /// listening for a file after about fifty seconds, so nothing the user
    /// picks now could be transferred.
    PeerStoppedAccepting,
    /// Nothing was chosen within `upload_wait_secs`.
    NoFilesChosen,
}

impl UploadGiveUp {
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::PeerStoppedAccepting => {
                "no files chosen in time; the peer stopped accepting a transfer"
            }
            Self::NoFilesChosen => "no files chosen, transfer cancelled",
        }
    }
}

struct UploadState {
    /// Files chosen by a drag-and-drop or a native picker, waiting to be sent.
    provided: Option<Vec<PathBuf>>,
    /// Typed paths, when the in-pane prompt is in use.
    prompt: Option<PromptInput>,
    /// When the current transfer started waiting for an upload choice.
    waiting_since: Option<Instant>,
    /// Last time the countdown was repainted, so the 50 ms driver loop does
    /// not redraw the prompt sixty times a second.
    last_tick: Instant,
    /// Set once the native picker has been spawned for this request.
    picker_spawned: bool,
}

impl ZmodemHandle {
    fn new(pane_id: PaneId) -> Self {
        Self {
            pane_id,
            cancel: AtomicBool::new(false),
            created: Instant::now(),
            upload: Mutex::new(UploadState {
                provided: None,
                prompt: None,
                waiting_since: None,
                last_tick: Instant::now(),
                picker_spawned: false,
            }),
        }
    }

    pub fn request_cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    fn cancel_requested(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// Called by the sender driver before it starts waiting for local files.
    fn begin_upload_prompt(&self) {
        let mut upload = self.upload.lock();
        if upload.waiting_since.is_none() {
            upload.waiting_since = Some(Instant::now());
        }
    }

    /// Time left before the peer stops accepting a file offer.
    ///
    /// Measured from when the handshake arrived, not from when the prompt
    /// appeared: the peer's own clock started at the handshake.
    fn accept_remaining(&self) -> Option<Duration> {
        accept_window().map(|w| w.saturating_sub(self.created.elapsed()))
    }

    /// True once the peer can no longer be given a file.
    fn peer_stopped_accepting(&self) -> bool {
        self.accept_remaining()
            .map(|left| left.is_zero())
            .unwrap_or(false)
    }

    /// True once the peer has left ZMODEM entirely.
    ///
    /// This is lrzsz's own timing, independent of `accept_window_secs`: after
    /// about fifty seconds `rz` stops parsing the wire at all and only prints a
    /// bare `C` once a second until it gives up at 161 s.  Measured on lrzsz
    /// 0.12.21rc with `-b` and without: handshakes at 0/10/20/30/40 s, first
    /// bare `C` at 51 s, exit at 161 s.  Past that point the ZMODEM cancel
    /// sequence is ignored and only a real `SIGINT` ends the process.
    fn peer_left_zmodem(&self) -> bool {
        self.created.elapsed() >= LRZSZ_ACCEPT_LIMIT
    }

    /// Restart the "waiting for the user" clock.
    ///
    /// Used when the native picker gives up and the in-pane prompt takes over:
    /// the user has only just been asked, so the time already spent staring at
    /// an invisible dialog must not count against them.  This never extends
    /// [`Self::accept_remaining`], which belongs to the peer.
    fn restart_upload_wait(&self) {
        let mut upload = self.upload.lock();
        if upload.provided.is_none() {
            upload.waiting_since = Some(Instant::now());
        }
    }

    /// Take files that were provided by a drop or a native picker.
    fn take_upload(&self) -> Option<Vec<PathBuf>> {
        self.upload.lock().provided.take()
    }

    /// How long the native picker may stay open.
    ///
    /// Two deadlines apply: the user's overall choice deadline
    /// (`upload_wait_secs`, of which the dialog gets half so the in-pane prompt
    /// still has time), and the peer's acceptance window.  A dialog answered
    /// after the peer has stopped listening produces a file nobody will take,
    /// so the window wins whenever it is earlier.
    fn picker_budget(&self) -> Duration {
        let by_wait = (upload_wait() / 2).clamp(Duration::from_secs(1), Duration::from_secs(30));
        match self.accept_remaining() {
            // Leave a couple of seconds to send the file offer itself.
            Some(left) => by_wait.min(
                left.saturating_sub(Duration::from_secs(2))
                    .max(Duration::from_secs(1)),
            ),
            None => by_wait,
        }
    }

    /// Why the upload should be abandoned, if it is time.
    pub(crate) fn upload_gave_up(&self) -> Option<UploadGiveUp> {
        if self.peer_stopped_accepting() {
            return Some(UploadGiveUp::PeerStoppedAccepting);
        }
        let upload = self.upload.lock();
        match upload.waiting_since {
            Some(since) if since.elapsed() > upload_wait() => Some(UploadGiveUp::NoFilesChosen),
            _ => None,
        }
    }

    /// Repaint the in-pane prompt at most once a second so the countdown
    /// moves.  No-op when the prompt is not on screen.
    pub(crate) fn tick_prompt(&self) {
        let mut upload = self.upload.lock();
        if upload.prompt.is_none() || upload.last_tick.elapsed() < Duration::from_secs(1) {
            return;
        }
        upload.last_tick = Instant::now();
        if let Some(prompt) = upload.prompt.as_ref() {
            emit_prompt(self, &prompt.cwd, &prompt.buf, prompt.cursor);
        }
    }

    fn provide_upload(&self, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        let mut upload = self.upload.lock();
        upload.provided = Some(paths);
        upload.prompt = None;
    }

    fn picker_already_spawned(&self) -> bool {
        let mut upload = self.upload.lock();
        let seen = upload.picker_spawned;
        upload.picker_spawned = true;
        seen
    }
}

/// Register a new session for `pane_id`, replacing any stale one.
pub fn begin_session(pane_id: PaneId) -> Arc<ZmodemHandle> {
    clear_abort_cooldown(pane_id);
    let handle = Arc::new(ZmodemHandle::new(pane_id));
    SESSIONS.lock().insert(pane_id, Arc::clone(&handle));
    handle
}

pub fn end_session(pane_id: PaneId) {
    SESSIONS.lock().remove(&pane_id);
}

/// How long a pane stays in the "just gave up" state.
const ABORT_COOLDOWN: Duration = Duration::from_secs(20);

fn note_abort(pane_id: PaneId) {
    RECENT_ABORT.lock().insert(pane_id, Instant::now());
}

/// True when a trigger should be answered with a cancel rather than a session.
pub fn in_abort_cooldown(pane_id: PaneId) -> bool {
    let mut map = RECENT_ABORT.lock();
    match map.get(&pane_id) {
        Some(when) if when.elapsed() < ABORT_COOLDOWN => true,
        Some(_) => {
            map.remove(&pane_id);
            false
        }
        None => false,
    }
}

/// Clear the cooldown, e.g. when the user explicitly starts a new transfer.
pub fn clear_abort_cooldown(pane_id: PaneId) {
    RECENT_ABORT.lock().remove(&pane_id);
}

pub fn session_handle(pane_id: PaneId) -> Option<Arc<ZmodemHandle>> {
    SESSIONS.lock().get(&pane_id).cloned()
}

pub fn is_active(pane_id: PaneId) -> bool {
    SESSIONS.lock().contains_key(&pane_id)
}

/// Ask the transfer on `pane_id` to stop.  Safe to call from any thread.
pub fn request_cancel(pane_id: PaneId) {
    if let Some(handle) = session_handle(pane_id) {
        handle.request_cancel();
    }
}

/// Signal a peer that has left ZMODEM and is no longer reading its input.
///
/// lrzsz `rz` switches its tty to raw mode, so a `^C` byte is not turned into
/// a signal any more (measured: writing ETX does nothing while `rz` is in its
/// `C` retry loop).  Once it stops listening for a file, the only way to end
/// it is a real SIGINT.  That is deliberately narrow:
///
/// * only when the foreground process really is an lrzsz binary, so this can
///   never kill an `ssh` (which is what the pgrp leader is when the remote
///   `rz` runs through a local ssh client), and
/// * via `kill(2)`, never by writing bytes, so nothing is left in the tty
///   input queue to be echoed as a stray `^C` on the restored prompt.
pub fn interrupt_peer(pane_id: PaneId) {
    #[cfg(unix)]
    {
        let Some(pane) = crate::Mux::try_get().and_then(|mux| mux.get_pane(pane_id)) else {
            return;
        };
        let Some(info) = pane.get_foreground_process_info(crate::pane::CachePolicy::FetchImmediate)
        else {
            return;
        };
        let name = info
            .executable
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let is_lrzsz = matches!(
            name.as_str(),
            "rz" | "sz" | "lrz" | "lsz" | "trz" | "tsz" | "rb" | "sb"
        );
        if !is_lrzsz {
            log::debug!("zmodem: peer on pane {pane_id} is {name:?}, not lrzsz; not signalling it");
            return;
        }
        // SAFETY: `kill` takes a pid and a signal number and has no other
        // preconditions.
        let rc = unsafe { libc::kill(info.pid as libc::pid_t, libc::SIGINT) };
        if rc == 0 {
            log::info!(
                "zmodem: sent SIGINT to {name} (pid {}) on pane {pane_id}",
                info.pid
            );
        } else {
            log::debug!(
                "zmodem: kill({}, SIGINT) failed on pane {pane_id}",
                info.pid
            );
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pane_id;
    }
}

/// Offer local files to an in-flight upload.
///
/// Returns true when a session took them (so the caller must *not* also paste
/// the paths into the terminal).  Also accepts files dropped before the
/// transfer started: they are stashed for the next upload on that pane.
pub fn offer_upload(pane_id: PaneId, paths: Vec<PathBuf>) -> bool {
    let Some(handle) = session_handle(pane_id) else {
        return false;
    };
    handle.provide_upload(paths);
    true
}

/// Feed pasted text to the upload prompt, one character at a time.
///
/// Returns true when a session consumed it; false means the caller should
/// handle the paste normally.
pub fn handle_paste(pane_id: PaneId, text: &str) -> bool {
    use wezterm_term::{KeyCode, KeyModifiers};
    if session_handle(pane_id).is_none() {
        return false;
    }
    for c in text.chars() {
        if c == '\r' || c == '\n' {
            handle_key(pane_id, KeyCode::Enter, KeyModifiers::NONE);
        } else if !c.is_control() {
            handle_key(pane_id, KeyCode::Char(c), KeyModifiers::NONE);
        }
    }
    true
}

/// Feed a key event to an in-flight transfer.
///
/// Returns true when the key was consumed and must not reach the child.
pub fn handle_key(
    pane_id: PaneId,
    key: wezterm_term::KeyCode,
    mods: wezterm_term::KeyModifiers,
) -> bool {
    use wezterm_term::{KeyCode, KeyModifiers};

    let Some(handle) = session_handle(pane_id) else {
        return false;
    };

    // Ctrl-C aborts the transfer.  It does *not* write an ETX into the pty:
    // the ZMODEM cancel sequence already makes lrzsz exit, and an extra 0x03
    // sits in the kernel input queue until the peer dies, at which point the
    // shell reads it and echoes `^C` on the restored prompt.
    if mods.contains(KeyModifiers::CTRL) && matches!(key, KeyCode::Char('c') | KeyCode::Char('C')) {
        handle.request_cancel();
        return true;
    }
    if key == KeyCode::Escape {
        handle.request_cancel();
        return true;
    }

    let mut upload = handle.upload.lock();
    if upload.provided.is_some() {
        // Waiting for the engine to pick the files up; swallow input so it
        // does not corrupt the protocol stream.
        return true;
    }
    if upload.waiting_since.is_none() {
        // Not waiting for files (yet): swallow input during the transfer so
        // typing does not corrupt the protocol stream.
        return true;
    }

    if upload.prompt.is_none() {
        upload.prompt = Some(PromptInput {
            buf: String::new(),
            cursor: 0,
            cwd: upload_base_dir(&handle),
            history: Vec::new(),
            history_pos: None,
            last_matches: Vec::new(),
        });
    }
    let prompt = upload.prompt.as_mut().expect("just set");

    let mut submit: Option<String> = None;
    match key {
        // Readline-style editing.  Ctrl-C is handled far above (it aborts the
        // transfer), so the other Ctrl keys are free for the prompt.  The
        // cursor is a real byte offset, so Ctrl-K really does mean "kill to
        // end of line" rather than "truncate the buffer".
        KeyCode::Char('w') | KeyCode::Char('W') if mods.contains(KeyModifiers::CTRL) => {
            delete_word_before_cursor(prompt);
            prompt.history_pos = None;
        }
        KeyCode::Char('u') | KeyCode::Char('U') if mods.contains(KeyModifiers::CTRL) => {
            prompt.buf.clear();
            prompt.cursor = 0;
            prompt.history_pos = None;
        }
        KeyCode::Char('k') | KeyCode::Char('K') if mods.contains(KeyModifiers::CTRL) => {
            prompt.buf.truncate(prompt.cursor);
            prompt.history_pos = None;
        }
        KeyCode::Char('a') | KeyCode::Char('A') if mods.contains(KeyModifiers::CTRL) => {
            prompt.cursor = 0;
            prompt.history_pos = None;
        }
        KeyCode::Char('e') | KeyCode::Char('E') if mods.contains(KeyModifiers::CTRL) => {
            prompt.cursor = prompt.buf.len();
            prompt.history_pos = None;
        }
        KeyCode::Char('d') | KeyCode::Char('D') if mods.contains(KeyModifiers::CTRL) => {
            delete_at_cursor(prompt);
            prompt.history_pos = None;
        }
        KeyCode::Char(c) if !mods.contains(KeyModifiers::CTRL) => {
            prompt.buf.insert(prompt.cursor, c);
            prompt.cursor += c.len_utf8();
            prompt.history_pos = None;
        }
        KeyCode::Backspace => {
            if let Some(prev) = prev_char_boundary(&prompt.buf, prompt.cursor) {
                prompt.buf.replace_range(prev..prompt.cursor, "");
                prompt.cursor = prev;
            }
            prompt.history_pos = None;
        }
        KeyCode::Delete => {
            delete_at_cursor(prompt);
            prompt.history_pos = None;
        }
        KeyCode::LeftArrow => {
            if let Some(prev) = prev_char_boundary(&prompt.buf, prompt.cursor) {
                prompt.cursor = prev;
            }
        }
        KeyCode::RightArrow => {
            prompt.cursor = next_char_boundary(&prompt.buf, prompt.cursor);
        }
        KeyCode::Home => {
            prompt.cursor = 0;
        }
        KeyCode::End => {
            prompt.cursor = prompt.buf.len();
        }
        KeyCode::Enter => {
            submit = Some(std::mem::take(&mut prompt.buf));
            prompt.cursor = 0;
            prompt.history_pos = None;
        }
        KeyCode::Tab => {
            complete_path(prompt);
        }
        KeyCode::UpArrow => {
            recall_history(prompt, -1);
        }
        KeyCode::DownArrow => {
            recall_history(prompt, 1);
        }
        _ => return true,
    }

    if let Some(line) = submit {
        let line = line.trim().to_string();
        if line.is_empty() {
            drop(upload);
            emit(&handle, &prompt_hint(&handle));
            prompt_in_pane(&handle);
            return true;
        }
        if !prompt.history.last().map(|h| h == &line).unwrap_or(false) {
            prompt.history.push(line.clone());
        }

        // Built-in commands.  They only affect this prompt: no shell is
        // spawned, nothing is sent to the peer, and the ZMODEM stream is
        // untouched.
        if let Some(reply) = run_prompt_command(&line, prompt) {
            let cwd = prompt.cwd.clone();
            let buf = prompt.buf.clone();
            let cursor = prompt.cursor;
            drop(upload);
            // Browsing counts as progress: restart the clock so a slow choice
            // does not get cancelled mid-`ls`.
            handle.restart_upload_wait();
            emit(&handle, &reply);
            emit_prompt(&handle, &cwd, &buf, cursor);
            return true;
        }

        let base = prompt.cwd.clone();
        let (paths, rejected) = parse_paths(&line, &base);
        if paths.is_empty() {
            drop(upload);
            let hint = if rejected.is_empty() {
                format!(
                    "\r\nzmodem: nothing to send. Type one or more paths, \
                     e.g. {}/some-file, or drop files on the window.\r\n",
                    base.display()
                )
            } else {
                let mut msg = format!(
                    "\r\nzmodem: no files matched (looked in {}):\r\n",
                    base.display()
                );
                for item in rejected.iter().take(5) {
                    msg.push_str(&format!("  {}\r\n", item));
                }
                msg
            };
            emit(&handle, &hint);
            prompt_in_pane(&handle);
        } else {
            if !rejected.is_empty() {
                emit(
                    &handle,
                    &format!(
                        "\r\nzmodem: ignoring {} that did not resolve\r\n",
                        rejected.len()
                    ),
                );
            }
            upload.provided = Some(paths);
            upload.prompt = None;
        }
        return true;
    }

    let cwd = prompt.cwd.clone();
    let buf = prompt.buf.clone();
    let cursor = prompt.cursor;
    let matches = std::mem::take(&mut prompt.last_matches);
    drop(upload);
    if !matches.is_empty() {
        // Column-major, the way a shell lists completions: a long directory
        // would otherwise scroll the whole pane away.
        emit(&handle, &format!("\r\n{}", format_matches(&matches)));
    }
    emit_prompt(&handle, &cwd, &buf, cursor);
    true
}

/// Draw the editable line into exactly one terminal row.
///
/// The line is deliberately *not* allowed to wrap.  A repaint starts with
/// `\r`, which only returns to the start of the current **physical** row; once
/// `prefix + buf` is wider than the pane the text has already wrapped onto a
/// new row, so the erase-and-rewrite leaves the previous row behind and every
/// keystroke - and every countdown tick - adds another line to the screen.
/// Instead the line is windowed horizontally so the caret stays visible, which
/// is what a shell does with a long command line.
///
/// The caret is a bar rather than the block a shell shows: a block sits *on*
/// the character under the caret, which reads as "this character is selected"
/// and makes Backspace look like it deletes the wrong thing.  `\x1b[0 q` at
/// the end of the session restores the configured shape.
fn emit_prompt(handle: &ZmodemHandle, cwd: &Path, buf: &str, cursor: usize) {
    let prefix = prompt_prefix(cwd, handle.accept_remaining());
    let caret =
        display_width(&prefix) + display_width(&buf[..char_boundary_at_or_before(buf, cursor)]);
    let (visible, col) = prompt_window(&prefix, buf, caret, pane_cols(handle));
    emit(handle, &format!("\r\x1b[2K\x1b[6 q{visible}\x1b[{}G", col));
}

/// Work out the one row of text to draw for the prompt, and where the caret
/// goes in it.
///
/// The line is deliberately *not* allowed to wrap.  A repaint starts with
/// `\r`, which only returns to the start of the current **physical** row; once
/// `prefix + buf` is wider than the pane, the text has already wrapped onto a
/// new row, so erase-and-rewrite leaves the previous row behind and every
/// keystroke - and every countdown tick - adds another line.  Instead the line
/// is windowed horizontally so the caret stays visible, which is what a shell
/// does with a long command line, complete with the `&lt;`/`&gt;` markers that
/// say text is hidden on that side.
///
/// `caret` is the caret's cell offset in the *unwindowed* line, `cols` the
/// pane width.  Returns the visible text and the caret's 1-based column.
fn prompt_window(prefix: &str, buf: &str, caret: usize, cols: usize) -> (String, usize) {
    // Never draw in the rightmost column: a character written there leaves
    // many terminals in a pending-wrap state, and the next repaint would start
    // on a fresh row - the very scroll we are avoiding.
    let width = cols.saturating_sub(1).max(1);
    let total = display_width(prefix) + display_width(buf);

    if total <= width {
        let mut visible = String::from(prefix);
        visible.push_str(buf);
        // `CSI G` is 1-based: column 1 is the left edge.
        return (visible, caret + 1);
    }

    // The line does not fit, so it has to be windowed.  A pane narrower than
    // three cells has no room for markers; squeeze the text instead.
    let marked = width >= 3;
    let content = if marked { width - 2 } else { width };
    // Keep the caret inside the window, and do not scroll past the end.
    let x0 = caret
        .saturating_sub(content.saturating_sub(1))
        .min(total.saturating_sub(content));
    let text = slice_cells(&format!("{prefix}{buf}"), x0, content);

    let mut visible = String::with_capacity(width);
    if marked {
        visible.push(if x0 > 0 { '<' } else { ' ' });
    }
    visible.push_str(&text);
    if marked {
        visible.push(if x0 + content < total { '>' } else { ' ' });
    }

    let col = caret - x0 + 1 + usize::from(marked);
    (visible, col.min(cols.max(1)))
}

/// The substring of `text` covering display cells `[skip, skip + take)`.
///
/// Cells are neither bytes nor chars: a CJK glyph occupies two, and the caller
/// works in terminal columns.  A wide glyph straddling the left edge is
/// dropped, which leaves a one-cell gap in the (all-ASCII in practice) prompt
/// prefix rather than shifting the caret.
fn slice_cells(text: &str, skip: usize, take: usize) -> String {
    if take == 0 {
        return String::new();
    }
    let end = skip + take;
    let mut out = String::new();
    let mut cell = 0;
    for c in text.chars() {
        if cell >= end {
            break;
        }
        if cell >= skip {
            out.push(c);
        }
        cell += display_width(&c.to_string());
    }
    out
}

/// Columns available in this pane, or 80 when the pane cannot be reached.
fn pane_cols(handle: &ZmodemHandle) -> usize {
    crate::Mux::try_get()
        .and_then(|mux| mux.get_pane(handle.pane_id))
        .map(|pane| pane.get_dimensions().cols)
        .unwrap_or(80)
}

fn char_boundary_at_or_before(text: &str, at: usize) -> usize {
    let mut at = at.min(text.len());
    while at > 0 && !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// The text in front of the editable part of the prompt.
///
/// While waiting for an upload it carries a countdown: lrzsz `rz` stops
/// listening for a file offer about fifty seconds in, so the number is the
/// real deadline, not decoration.
fn prompt_prefix(cwd: &Path, remaining: Option<Duration>) -> String {
    match remaining {
        Some(left) => format!(
            "zmodem: files to send (in {}) [{}s]> ",
            cwd.display(),
            left.as_secs() + u64::from(left.subsec_nanos() > 0)
        ),
        None => format!("zmodem: files to send (in {})> ", cwd.display()),
    }
}

/// Display width in terminal cells, so the cursor column is right when the
/// path contains wide characters.
fn display_width(text: &str) -> usize {
    use termwiz::cell::grapheme_column_width;
    text.chars()
        .map(|c| grapheme_column_width(&c.to_string(), None))
        .sum()
}

/// What to say when the user submits an empty line.
fn prompt_hint(handle: &ZmodemHandle) -> String {
    let cwd = session_cwd(handle);
    format!(
        "\r\nzmodem: type one or more paths (e.g. {}/some-file, or *.log), \
         or a command: ls, cd DIR, pwd, help.\r\n\
         You can also drop files on the window.\r\n",
        cwd.display()
    )
}

fn session_cwd(handle: &ZmodemHandle) -> PathBuf {
    handle
        .upload
        .lock()
        .prompt
        .as_ref()
        .map(|p| p.cwd.clone())
        .unwrap_or_else(|| upload_base_dir(handle))
}

/// Handle the prompt's own commands.  Returns the text to print when `line`
/// was a command, or `None` when it should be treated as a path list.
fn run_prompt_command(line: &str, prompt: &mut PromptInput) -> Option<String> {
    let words = shell_words::split(line).ok()?;
    let cmd = words.first()?.as_str();
    let arg = words.get(1).map(String::as_str);

    match cmd {
        "help" | "?" => Some(
            "\r\nzmodem prompt commands:\r\n\
             \x20 ls [DIR]      list files (DIR defaults to the current one)\r\n\
             \x20 cd [DIR]      change directory; `cd` alone goes home\r\n\
             \x20 pwd           print the current directory\r\n\
             \x20 help          this text\r\n\
             \x20 <paths>       send those files; globs and ~ work\r\n\
             keys: Tab completes, arrows move the cursor, Up/Down at the start\r\n\
             \x20      recalls history, Ctrl-W/U/K/A/E edit, Esc cancels,\r\n\
             \x20      Ctrl-C aborts the transfer\r\n"
                .to_string(),
        ),
        "pwd" => Some(format!("\r\n{}\r\n", prompt.cwd.display())),
        "cd" => {
            let target = match arg {
                None | Some("~") => dirs_next::home_dir().unwrap_or_else(|| prompt.cwd.clone()),
                Some(dir) => resolve_word(dir, &prompt.cwd),
            };
            match std::fs::canonicalize(&target) {
                Ok(path) if path.is_dir() => {
                    prompt.cwd = path;
                    Some(String::new())
                }
                Ok(_) => Some(format!(
                    "\r\nzmodem: {} is not a directory\r\n",
                    target.display()
                )),
                Err(err) => Some(format!("\r\nzmodem: {}: {err}\r\n", target.display())),
            }
        }
        "ls" => {
            // `ls` alone lists the cwd; `ls pattern` lists matching entries,
            // and `ls file` reports that one file.
            let entries: Vec<PathBuf> = words
                .iter()
                .skip(1)
                .flat_map(|word| {
                    if word.contains('*') || word.contains('?') {
                        expand_glob(word, &prompt.cwd)
                    } else {
                        vec![resolve_word(word, &prompt.cwd)]
                    }
                })
                .collect();
            if entries.is_empty() {
                Some(format!("\r\n{}", list_dir(&prompt.cwd)))
            } else if entries.len() == 1 && entries[0].is_file() {
                Some(format!("\r\n{}", describe_file(&entries[0])))
            } else {
                let mut out = String::from("\r\n");
                let mut any = false;
                for entry in &entries {
                    if entry.is_dir() {
                        out.push_str(&format!("{}/\r\n", entry.display()));
                    } else if entry.is_file() {
                        out.push_str(&describe_file(entry));
                        any = true;
                    }
                }
                if !any && out.lines().count() <= 2 {
                    out.push_str("(nothing matched)\r\n");
                }
                Some(out)
            }
        }
        _ => None,
    }
}

/// One file, with the size a user actually wants when choosing what to send.
fn describe_file(path: &Path) -> String {
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    format!("  {}  {}\r\n", human_size_column(size), path.display())
}

fn human_size_column(n: u64) -> String {
    format!("{:>10}", session::human_bytes(n))
}

/// `ls`-alike for the prompt: names, sizes, directories first.
fn list_dir(dir: &Path) -> String {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) => return format!("zmodem: {}: {err}\r\n", dir.display()),
    };

    let mut rows: Vec<(bool, String, u64)> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // Hidden files are noise when picking something to send.
        if name.starts_with('.') {
            continue;
        }
        let meta = entry.metadata().ok();
        let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
        let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
        rows.push((is_dir, name, size));
    }
    if rows.is_empty() {
        return format!("(no files in {})\r\n", dir.display());
    }
    rows.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));

    // Same column layout as completion output: a directory with hundreds of
    // files must not scroll the prompt off screen.
    let names: Vec<String> = rows
        .iter()
        .map(|(is_dir, name, size)| {
            if *is_dir {
                format!("{}/", name)
            } else {
                format!("{} ({})", name, session::human_bytes(*size))
            }
        })
        .collect();
    format_matches(&names)
}

/// Tab completion over the current directory.
///
/// Completes the token the cursor is inside (not the last token of the line: on
/// a line like `cd /t` the cursor may sit before the end after an edit), and
/// leaves the cursor at the end of the insertion the way a shell does.  A
/// unique match is inserted (directories get a trailing `/`); several matches
/// extend the line to their common prefix and are listed.
fn complete_path(prompt: &mut PromptInput) -> bool {
    let cursor = prompt.cursor.min(prompt.buf.len());
    let start = prompt.buf[..cursor]
        .rfind(char::is_whitespace)
        .map(|i| i + 1)
        .unwrap_or(0);
    let prefix = prompt.buf[start..cursor].to_string();
    // Complete the basename but keep whatever directory part was typed.
    let (dir_part, file_part) = match prefix.rfind('/') {
        Some(idx) => (prefix[..=idx].to_string(), prefix[idx + 1..].to_string()),
        None => (String::new(), prefix),
    };
    let scan_dir = resolve_word(
        if dir_part.is_empty() { "." } else { &dir_part },
        &prompt.cwd,
    );

    let Ok(entries) = std::fs::read_dir(&scan_dir) else {
        return false;
    };
    // `cd`/`ls DIR` want directories only; a bare path prompt wants everything.
    let want_dirs_only = buffer_asks_for_dir(&prompt.buf, start);
    let mut matches: Vec<String> = entries
        .flatten()
        .map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            (name, is_dir)
        })
        .filter(|(name, _)| name.starts_with(&file_part))
        .filter(|(_, is_dir)| !want_dirs_only || *is_dir)
        .map(|(name, is_dir)| if is_dir { format!("{}/", name) } else { name })
        .collect();
    matches.sort();
    matches.dedup();

    match matches.len() {
        0 => false,
        1 => {
            let completion = format!("{}{}", dir_part, matches[0]);
            prompt.cursor = splice(&mut prompt.buf, start, cursor, &completion);
            true
        }
        _ => {
            // Fill in whatever prefix the candidates share, then show them:
            // typing `z` in a directory of `z1.png`, `z2.png` should both
            // extend the line as far as it can go and print the options.
            let common = longest_common_prefix(&matches);
            if common.len() > file_part.len() {
                let completion = format!("{}{}", dir_part, common);
                prompt.cursor = splice(&mut prompt.buf, start, cursor, &completion);
            }
            prompt.last_matches = matches;
            true
        }
    }
}

/// Replace `buf[start..end]` with `text`, returning the cursor position after
/// the insertion.  Completion uses this instead of truncating the line, so a
/// cursor that is not at the end does not throw away what follows it.
fn splice(buf: &mut String, start: usize, end: usize, text: &str) -> usize {
    buf.replace_range(start..end, text);
    start + text.len()
}

/// Lay candidates out in columns that fit `columns` characters.
///
/// Listing one name per line pushed the prompt off screen in a big directory;
/// this keeps the whole answer to a handful of rows by widening each column to
/// the longest name (capped) and flowing down-then-across like `ls`.
fn format_matches(matches: &[String]) -> String {
    const COLUMNS: usize = 80;
    const MAX_ROWS: usize = 10;
    const MAX_SHOWN: usize = 200;

    let shown = &matches[..matches.len().min(MAX_SHOWN)];
    let width = shown
        .iter()
        .map(|m| m.chars().count())
        .max()
        .unwrap_or(0)
        .min(40)
        + 2;
    let per_row = (COLUMNS / width.max(1)).max(1);

    let rows = shown.len().div_ceil(per_row).min(MAX_ROWS);
    let mut out = String::new();
    for row in 0..rows {
        for col in 0..per_row {
            // Down-then-across: index = col * rows + row.
            let Some(name) = shown.get(col * rows + row) else {
                break;
            };
            out.push_str(name);
            if col + 1 < per_row && col * rows + row + rows < shown.len() {
                let pad = width.saturating_sub(name.chars().count());
                out.push_str(&" ".repeat(pad));
            }
        }
        out.push_str("\r\n");
    }
    if matches.len() > shown.len() {
        out.push_str(&format!("  ... {} more\r\n", matches.len() - shown.len()));
    } else if rows * per_row < shown.len() {
        out.push_str("  ... (too many to show; keep typing)\r\n");
    }
    out
}

/// True when the token being completed is the argument of `cd`, so only
/// directories make sense as candidates.
fn buffer_asks_for_dir(buf: &str, token_start: usize) -> bool {
    let before = buf[..token_start].trim_end();
    matches!(before.split_whitespace().next(), Some("cd"))
}

fn longest_common_prefix(items: &[String]) -> String {
    let mut prefix = items.first().cloned().unwrap_or_default();
    for item in items.iter().skip(1) {
        while !item.starts_with(&prefix) {
            prefix.pop();
            if prefix.is_empty() {
                return prefix;
            }
        }
    }
    prefix
}

fn prev_char_boundary(buf: &str, at: usize) -> Option<usize> {
    if at == 0 {
        return None;
    }
    let mut i = at - 1;
    while i > 0 && !buf.is_char_boundary(i) {
        i -= 1;
    }
    Some(i)
}

fn next_char_boundary(buf: &str, at: usize) -> usize {
    if at >= buf.len() {
        return buf.len();
    }
    let mut i = at + 1;
    while i < buf.len() && !buf.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Delete the character at the cursor (Ctrl-D / Delete).
fn delete_at_cursor(prompt: &mut PromptInput) {
    if prompt.cursor < prompt.buf.len() {
        let end = next_char_boundary(&prompt.buf, prompt.cursor);
        prompt.buf.replace_range(prompt.cursor..end, "");
    }
}

/// Delete the word before the cursor (Ctrl-W), like a shell does.
fn delete_word_before_cursor(prompt: &mut PromptInput) {
    let mut start = prompt.cursor;
    // Skip trailing whitespace immediately behind the cursor.
    while let Some(prev) = prev_char_boundary(&prompt.buf, start) {
        if prompt.buf[prev..start].starts_with(char::is_whitespace) {
            start = prev;
        } else {
            break;
        }
    }
    while let Some(prev) = prev_char_boundary(&prompt.buf, start) {
        let ch = &prompt.buf[prev..start];
        if ch.starts_with(char::is_whitespace) || ch == "/" {
            break;
        }
        start = prev;
    }
    prompt.buf.replace_range(start..prompt.cursor, "");
    prompt.cursor = start;
}

fn recall_history(prompt: &mut PromptInput, delta: i32) {
    if prompt.history.is_empty() {
        return;
    }
    let len = prompt.history.len() as isize;
    let pos: isize = match prompt.history_pos {
        None => {
            if delta < 0 {
                len - 1
            } else {
                return;
            }
        }
        Some(p) => (p as isize + delta as isize).clamp(0, len - 1),
    };
    prompt.history_pos = Some(pos as usize);
    prompt.buf = prompt.history[pos as usize].clone();
    prompt.cursor = prompt.buf.len();
}

/// Split a user-typed path list into readable regular files.
///
/// Relative names are resolved against `base` (the directory the shell is in),
/// `~` is expanded, and a word containing `*` or `?` is treated as a glob and
/// expanded against `base` too.  Names that do not resolve are reported back
/// to the caller so the prompt can say what went wrong instead of silently
/// dropping them.
fn parse_paths(line: &str, base: &Path) -> (Vec<PathBuf>, Vec<String>) {
    let words = shell_words::split(line)
        .unwrap_or_else(|_| line.split_whitespace().map(|s| s.to_string()).collect());

    let mut paths = Vec::new();
    let mut rejected = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for word in words {
        if word.contains('*') || word.contains('?') {
            let matches = expand_glob(&word, base);
            if matches.is_empty() {
                rejected.push(word);
            }
            for path in matches {
                if seen.insert(path.clone()) {
                    paths.push(path);
                }
            }
            continue;
        }

        let path = resolve_word(&word, base);
        match std::fs::metadata(&path) {
            Ok(meta) if meta.is_file() => {
                if seen.insert(path.clone()) {
                    paths.push(path);
                }
            }
            Ok(_) => rejected.push(format!("{} (not a regular file)", word)),
            Err(_) => rejected.push(word),
        }
    }

    (paths, rejected)
}

/// Expand `~`, and anchor relative names at `base`.
fn resolve_word(word: &str, base: &Path) -> PathBuf {
    if word == "~" {
        return dirs_next::home_dir().unwrap_or_else(|| PathBuf::from(word));
    }
    let expanded = match word.strip_prefix("~/") {
        Some(rest) => match dirs_next::home_dir() {
            Some(home) => home.join(rest),
            None => return PathBuf::from(word),
        },
        None => PathBuf::from(word),
    };
    if expanded.is_absolute() {
        expanded
    } else {
        base.join(expanded)
    }
}

/// Expand a glob against `base`, matching the way a shell would for a simple
/// `*.ext` style pattern: only the last component may contain wildcards.
fn expand_glob(pattern: &str, base: &Path) -> Vec<PathBuf> {
    let resolved = resolve_word(pattern, base);
    let (dir, file_pattern) = match resolved.parent() {
        Some(dir) => (dir.to_path_buf(), resolved.file_name()),
        None => (base.to_path_buf(), resolved.file_name()),
    };
    let Some(file_pattern) = file_pattern else {
        return Vec::new();
    };
    let file_pattern = file_pattern.to_string_lossy().into_owned();
    if !file_pattern.contains('*') && !file_pattern.contains('?') {
        return Vec::new();
    }

    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if wildcard_match(&file_pattern, &name) {
            let path = entry.path();
            if path.is_file() {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// Minimal `*`/`?` matcher: `*` spans any characters, `?` exactly one.
fn wildcard_match(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    // Iterative backtracking; patterns here are tiny, so simple is fine.
    let (mut pi, mut ni) = (0usize, 0usize);
    let (mut star, mut star_ni) = (None::<usize>, 0usize);
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            star_ni = ni;
            pi += 1;
        } else if let Some(sp) = star {
            pi = sp + 1;
            star_ni += 1;
            ni = star_ni;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Directory the in-pane upload prompt resolves relative paths against.
///
/// The pane's foreground process directory is the one the user is looking at
/// (they just typed `rz` there), so it is the least surprising answer; when it
/// cannot be determined we fall back to this process's working directory.
pub fn upload_base_dir(handle: &ZmodemHandle) -> PathBuf {
    if let Some(pane) = crate::Mux::try_get().and_then(|mux| mux.get_pane(handle.pane_id)) {
        // Ask the process group leader directly.  `get_current_working_dir`
        // also consults OSC 7 and a cache captured when the pane was created,
        // either of which can report wezterm's own directory instead of the
        // one the user is sitting in when they typed `rz`.
        if let Some(info) =
            pane.get_foreground_process_info(crate::pane::CachePolicy::FetchImmediate)
        {
            if info.cwd.is_absolute() && info.cwd.is_dir() {
                return info.cwd;
            }
        }
        // Fall back to the pane's own notion of the cwd (OSC 7 from the shell).
        if let Some(url) = pane.get_current_working_dir(crate::pane::CachePolicy::FetchImmediate) {
            if let Ok(path) = url.to_file_path() {
                return path;
            }
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn emit(handle: &ZmodemHandle, text: &str) {
    crate::localpane::emit_output_for_pane(handle.pane_id, text);
}

/// Emit text and wait briefly for the terminal model to consume it.
///
/// Used for the closing lines of a transfer, which must not race with the
/// child process exiting and the shell redrawing its prompt.
fn emit_sync(handle: &ZmodemHandle, text: &str) {
    crate::localpane::emit_output_for_pane_sync(handle.pane_id, text);
}

/// Start the native file picker for an upload, falling back to the in-pane
/// prompt when it is unavailable.
///
/// `zenity` needs a display server; in a headless session it fails
/// immediately, which is the common case for automated tests.
pub fn spawn_upload_picker(pane_id: PaneId) {
    let Some(handle) = session_handle(pane_id) else {
        return;
    };
    if handle.picker_already_spawned() {
        return;
    }

    let use_native = match config::configuration().zmodem.upload_file_picker {
        config::UploadFilePicker::Prompt => false,
        config::UploadFilePicker::Native | config::UploadFilePicker::Auto => {
            std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some()
        }
    };

    if use_native {
        let weak = Arc::downgrade(&handle);
        let budget = handle.picker_budget();
        std::thread::spawn(move || match run_zenity(budget) {
            Ok(paths) if !paths.is_empty() => {
                if let Some(handle) = weak.upgrade() {
                    handle.provide_upload(paths);
                }
            }
            Ok(_) => {
                // Cancelled: treat as an abort rather than hanging.
                if let Some(handle) = weak.upgrade() {
                    handle.request_cancel();
                }
            }
            Err(err) => {
                log::debug!("zmodem: native picker unavailable ({err}); prompting in the pane");
                if let Some(handle) = weak.upgrade() {
                    prompt_in_pane(&handle);
                }
            }
        });
    } else {
        prompt_in_pane(&handle);
    }
}

fn prompt_in_pane(handle: &ZmodemHandle) {
    handle.restart_upload_wait();
    let base = upload_base_dir(handle);
    log::debug!(
        "zmodem: upload prompt base dir = {} (pane {})",
        base.display(),
        handle.pane_id
    );
    // Install the editing state up front rather than on the first keystroke:
    // the countdown has to start moving immediately, and it is driven by a
    // repaint of this state.
    {
        let mut upload = handle.upload.lock();
        if upload.prompt.is_none() {
            upload.prompt = Some(PromptInput {
                buf: String::new(),
                cursor: 0,
                cwd: base.clone(),
                history: Vec::new(),
                history_pos: None,
                last_matches: Vec::new(),
            });
        }
        upload.last_tick = Instant::now();
    }
    emit_prompt(handle, &base, "", 0);
}

/// Ask the desktop file picker for files.
///
/// `budget` is how long the dialog may stay open: the caller has already
/// capped it by both `upload_wait_secs` and whatever is left of the peer's
/// acceptance window, because a dialog answered after either deadline can only
/// produce a file nobody will take.  On timeout the child is killed and the
/// caller falls back to the in-pane prompt.
fn run_zenity(budget: Duration) -> anyhow::Result<Vec<PathBuf>> {
    use std::process::Stdio;

    let mut child = std::process::Command::new("zenity")
        .args([
            "--file-selection",
            "--multiple",
            "--separator=\n",
            "--title=Select files to send",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;

    let deadline = Instant::now() + budget;
    loop {
        match child.try_wait()? {
            Some(status) => {
                if !status.success() {
                    anyhow::bail!("zenity exited with {}", status);
                }
                break;
            }
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!("zenity timed out");
            }
            None => std::thread::sleep(Duration::from_millis(100)),
        }
    }

    let mut text = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        use std::io::Read;
        let _ = stdout.read_to_string(&mut text);
    }
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .collect())
}

/// Entry point used by the pane reader thread once the detector fires.
///
/// Consumes the rest of the stream, then prints the outcome into the pane.
pub fn run(
    pane_id: PaneId,
    pane: Weak<dyn Pane>,
    rx: &mut filedescriptor::FileDescriptor,
    dead: &Arc<AtomicBool>,
    mode: Mode,
    seed: Vec<u8>,
) {
    let handle = begin_session(pane_id);
    let session = session::Session::new(
        pane_id,
        pane,
        rx,
        dead,
        Arc::clone(&handle),
        timeout(),
        show_progress(),
    );

    let outcome = match mode {
        Mode::Download => session.run_download(seed),
        Mode::Upload => session.run_send(seed),
    };

    end_session(pane_id);

    // These lines are emitted synchronously.  The child (`sz`/`rz`) exits as
    // the session winds down, so the shell redraws its prompt at about the same
    // time; an asynchronous inject lands *after* the prompt and leaves the
    // cursor stranded mid-line, which reads as "press Enter to continue".
    match outcome {
        session::Outcome::Completed(done) => {
            let mut lines: Vec<String> = done
                .files
                .iter()
                .map(|p| format!("zmodem: {}", p.display()))
                .collect();
            lines.extend(
                done.skipped
                    .iter()
                    .map(|p| format!("zmodem: {} was skipped by the peer", p.display())),
            );
            if lines.is_empty() {
                lines.push("zmodem: transfer complete".to_string());
            }
            let stats = format!(
                "{} in {} at {}/s",
                session::human_bytes(done.bytes),
                session::human_duration(done.elapsed),
                session::human_bytes(session::avg_speed(done.bytes, done.elapsed))
            );
            let summary = lines.join("; ");
            log::info!("zmodem: pane {pane_id}: {summary}");
            log::info!("zmodem: pane {pane_id}: {stats}");
            for line in &lines {
                emit_sync(&handle, &format!("\r\x1b[2K{line}\r\n"));
            }
            // A transfer where nothing moved (everything was skipped) has no
            // meaningful rate to report, and deserves no desktop popup.
            if done.bytes > 0 {
                emit_sync(&handle, &format!("\r\x1b[2Kzmodem: {stats}\r\n"));
            }
            if done.bytes > 0 && config::configuration().zmodem.show_notification {
                crate::Mux::notify_from_any_thread(crate::MuxNotification::Alert {
                    pane_id,
                    alert: wezterm_term::Alert::ToastNotification {
                        title: Some("ZMODEM transfer complete".to_string()),
                        body: format!("{summary} ({stats})"),
                        focus: true,
                    },
                });
            }
        }
        session::Outcome::Aborted { reason } => {
            note_abort(pane_id);
            log::info!("zmodem: pane {pane_id}: transfer cancelled");
            // The session prints its own explanation when it gave up for a
            // specific reason (peer stopped accepting, nothing chosen...);
            // saying "cancelled" again underneath just repeats it.
            if let Some(reason) = reason {
                emit_sync(&handle, &format!("\r\x1b[2Kzmodem: {reason}\r\n"));
            } else {
                emit_sync(&handle, "\r\x1b[2Kzmodem: transfer cancelled\r\n");
            }
        }
        session::Outcome::TimedOut => {
            note_abort(pane_id);
            log::warn!("zmodem: pane {pane_id}: timed out");
            emit_sync(
                &handle,
                "\r\x1b[2Kzmodem: timed out, transfer cancelled\r\n",
            );
        }
        session::Outcome::Failed(msg) => {
            log::error!("zmodem: pane {pane_id}: {msg}");
            emit_sync(&handle, &format!("\r\x1b[2K{msg}\r\n"));
        }
    }

    // Hand the cursor shape back.  The upload prompt switches to a bar so the
    // caret sits between characters; leaving that set would change the shape
    // of the user's shell prompt too.  `CSI 0 SP q` means "back to whatever
    // the user configured", i.e. `default_cursor_style`.
    emit_sync(&handle, "\x1b[0 q");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zmodem::session::human_duration;

    fn touch(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, b"x").unwrap();
        path
    }

    #[test]
    fn relative_names_resolve_against_base() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "a.txt");
        let (paths, rejected) = parse_paths("a.txt", dir.path());
        assert_eq!(paths, vec![dir.path().join("a.txt")]);
        assert!(rejected.is_empty());
    }

    #[test]
    fn absolute_names_ignore_base() {
        let dir = tempfile::tempdir().unwrap();
        let file = touch(dir.path(), "b.txt");
        let (paths, _) = parse_paths(file.to_str().unwrap(), Path::new("/nonexistent"));
        assert_eq!(paths, vec![file]);
    }

    #[test]
    fn globs_expand_in_base() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "one.log");
        touch(dir.path(), "two.log");
        touch(dir.path(), "three.txt");
        let (paths, rejected) = parse_paths("*.log", dir.path());
        assert_eq!(paths.len(), 2);
        assert!(paths.iter().all(|p| p.extension().unwrap() == "log"));
        assert!(rejected.is_empty());
    }

    #[test]
    fn unmatched_glob_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let (paths, rejected) = parse_paths("*.nothing", dir.path());
        assert!(paths.is_empty());
        assert_eq!(rejected, vec!["*.nothing".to_string()]);
    }

    #[test]
    fn directories_are_rejected_not_sent() {
        let dir = tempfile::tempdir().unwrap();
        let (paths, rejected) = parse_paths(".", dir.path());
        assert!(paths.is_empty());
        assert_eq!(rejected.len(), 1);
    }

    #[test]
    fn quoted_names_with_spaces_work() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "with space.txt");
        let (paths, _) = parse_paths("'with space.txt'", dir.path());
        assert_eq!(paths, vec![dir.path().join("with space.txt")]);
    }

    #[test]
    fn wildcard_matcher_matches_shell_semantics() {
        assert!(wildcard_match("*.log", "a.log"));
        assert!(wildcard_match("a?c", "abc"));
        assert!(!wildcard_match("a?c", "ac"));
        assert!(wildcard_match("*", "anything"));
        assert!(wildcard_match("a*b*c", "axxbyyc"));
        assert!(!wildcard_match("a*b*c", "axxbyy"));
    }

    fn prompt_at(dir: &Path) -> PromptInput {
        PromptInput {
            buf: String::new(),
            cursor: 0,
            cwd: dir.to_path_buf(),
            history: Vec::new(),
            history_pos: None,
            last_matches: Vec::new(),
        }
    }

    #[test]
    fn prompt_cd_changes_the_session_directory() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let mut prompt = prompt_at(dir.path());
        assert!(run_prompt_command("cd sub", &mut prompt).is_some());
        assert_eq!(prompt.cwd, std::fs::canonicalize(&sub).unwrap());
    }

    #[test]
    fn prompt_cd_rejects_files() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "a.txt");
        let mut prompt = prompt_at(dir.path());
        let reply = run_prompt_command("cd a.txt", &mut prompt).unwrap();
        assert!(reply.contains("not a directory"), "{}", reply);
        assert_eq!(prompt.cwd, dir.path());
    }

    #[test]
    fn prompt_ls_lists_files_and_hides_dotfiles() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "visible.txt");
        touch(dir.path(), ".hidden");
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let mut prompt = prompt_at(dir.path());
        let reply = run_prompt_command("ls", &mut prompt).unwrap();
        assert!(reply.contains("visible.txt"), "{}", reply);
        assert!(reply.contains("sub/"), "{}", reply);
        assert!(!reply.contains(".hidden"), "{}", reply);
    }

    #[test]
    fn prompt_answers_help_and_pwd() {
        let dir = tempfile::tempdir().unwrap();
        let mut prompt = prompt_at(dir.path());
        assert!(run_prompt_command("help", &mut prompt)
            .unwrap()
            .contains("cd"));
        assert!(run_prompt_command("pwd", &mut prompt)
            .unwrap()
            .contains(&dir.path().display().to_string()));
        // A path is not a command.
        assert!(run_prompt_command("a.txt", &mut prompt).is_none());
    }

    #[test]
    fn tab_completes_a_unique_name() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "report.txt");
        let mut prompt = prompt_at(dir.path());
        prompt.set_line("rep");
        assert!(complete_path(&mut prompt));
        assert_eq!(prompt.buf, "report.txt");
    }

    #[test]
    fn tab_completes_directories_with_a_slash() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("documents")).unwrap();
        let mut prompt = prompt_at(dir.path());
        prompt.set_line("doc");
        assert!(complete_path(&mut prompt));
        assert_eq!(prompt.buf, "documents/");
    }

    #[test]
    fn tab_fills_common_prefix_and_lists_alternatives() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "log-one.txt");
        touch(dir.path(), "log-two.txt");
        let mut prompt = prompt_at(dir.path());
        prompt.set_line("log");
        // `true` = extend the line *and* show the candidates.
        assert!(complete_path(&mut prompt));
        assert_eq!(prompt.buf, "log-");
        assert_eq!(
            prompt.last_matches,
            vec!["log-one.txt".to_string(), "log-two.txt".to_string()]
        );
    }

    #[test]
    fn ls_output_is_compact() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..80 {
            touch(dir.path(), &format!("f{i:02}.dat"));
        }
        let listing = list_dir(dir.path());
        let rows = listing.matches("\r\n").count();
        assert!(rows <= 12, "ls took {} rows:\n{}", rows, listing);
    }

    #[test]
    fn match_listing_is_compact() {
        let many: Vec<String> = (0..60).map(|i| format!("file-{i:02}.png")).collect();
        let listing = format_matches(&many);
        let rows = listing.matches("\r\n").count();
        assert!(rows <= 12, "listing took {} rows:\n{}", rows, listing);
        // Every name must still appear exactly once.
        for name in &many {
            assert_eq!(
                listing.matches(name.as_str()).count(),
                1,
                "missing {}",
                name
            );
        }
    }

    #[test]
    fn cd_completion_offers_directories_only() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("docs")).unwrap();
        touch(dir.path(), "data.txt");
        let mut prompt = prompt_at(dir.path());
        prompt.set_line("cd d");
        assert!(complete_path(&mut prompt));
        assert_eq!(prompt.buf, "cd docs/");
    }

    #[test]
    fn cursor_editing_works_mid_line() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = prompt_at(dir.path());
        p.buf = "abc".into();
        p.cursor = 1;
        // Insert in the middle.
        p.buf.insert(p.cursor, 'X');
        p.cursor += 1;
        assert_eq!(p.buf, "aXbc");
        // Ctrl-K kills to end of line only.
        p.buf.truncate(p.cursor);
        assert_eq!(p.buf, "aX");
        // Ctrl-W removes the word before the cursor.
        p.buf = "/tmp/foo.txt".into();
        p.cursor = p.buf.len();
        delete_word_before_cursor(&mut p);
        assert_eq!(p.buf, "/tmp/");
        assert_eq!(p.cursor, 5);
    }

    #[test]
    fn history_recalls_previous_lines() {
        let dir = tempfile::tempdir().unwrap();
        let mut prompt = prompt_at(dir.path());
        prompt.history = vec!["one".into(), "two".into()];
        recall_history(&mut prompt, -1);
        assert_eq!(prompt.buf, "two");
        recall_history(&mut prompt, -1);
        assert_eq!(prompt.buf, "one");
        recall_history(&mut prompt, 1);
        assert_eq!(prompt.buf, "two");
    }

    #[test]
    fn prompt_never_exceeds_one_row() {
        // The reported bug: a line wider than the pane wrapped, so `\r` came
        // back to the start of the *second* physical row, the erase missed the
        // first, and every keystroke (and every countdown tick) added a line.
        let prefix = "zmodem: files to send (in /tmp) [45s]> ";
        let widths = [20usize, 40, 79, 80, 81, 200, 1000];
        for cols in widths {
            let buf = "a".repeat(300);
            // Caret at the end, in the middle, and at the start.
            for caret_cells in [0usize, 40, 300] {
                let caret = display_width(prefix) + caret_cells;
                let (visible, col) = prompt_window(prefix, &buf, caret, cols);
                let width = display_width(&visible);
                assert!(
                    width <= cols.saturating_sub(1).max(1),
                    "cols={} caret={}: drew {} cells ({:?})",
                    cols,
                    caret_cells,
                    width,
                    visible
                );
                // The caret must be on the drawn line, and not past its end.
                assert!(
                    col >= 1 && col <= width + 1,
                    "cols={} caret={}: col={} width={}",
                    cols,
                    caret_cells,
                    col,
                    width
                );
            }
        }
    }

    #[test]
    fn prompt_window_scrolls_to_keep_the_caret_visible() {
        let prefix = "p> ";
        let buf = "0123456789";
        // Caret at the very end of a line that does not fit: the tail is
        // shown, the start is marked as hidden, and the caret lands on the
        // trailing marker.
        let caret = display_width(prefix) + buf.len();
        let (visible, col) = prompt_window(prefix, buf, caret, 8);
        // Nothing is hidden past the caret, so the trailing marker is blank;
        // the leading one says the start has scrolled off.
        assert_eq!(visible, "<56789 ");
        assert_eq!(
            display_width(&visible),
            7,
            "one column is kept free for wrapping"
        );
        assert_eq!(col, 7, "caret is one past the 9");

        // Caret at the start: the beginning is shown, still one row.  The
        // leading marker is blank because nothing is hidden on that side, but
        // it still occupies a column - the layout must not shift sideways by
        // one cell the moment the marker appears or disappears.
        let (visible, col) = prompt_window(prefix, buf, 0, 8);
        assert_eq!(visible, " p> 01>");
        assert_eq!(display_width(&visible), 7);
        assert_eq!(col, 2, "column 1 is the blank marker");
    }

    #[test]
    fn prompt_window_does_not_move_when_the_line_fits() {
        let (visible, col) = prompt_window("p> ", "abc", 6, 80);
        assert_eq!(visible, "p> abc");
        assert_eq!(col, 7, "caret after the `c`");
    }

    #[test]
    fn prompt_window_handles_a_zero_width_pane() {
        // A pane can report zero columns before it has been laid out.
        let (visible, col) = prompt_window("p> ", "abc", 3, 0);
        assert_eq!(col, 1);
        assert!(display_width(&visible) <= 1);
    }

    #[test]
    fn slice_cells_counts_columns_not_chars() {
        // A CJK glyph is two cells wide; slicing by char would produce a line
        // wider than the pane and reintroduce the wrap.
        assert_eq!(slice_cells("目录abc", 0, 2), "目");
        // Starting inside a wide glyph drops it rather than widening the slice.
        assert_eq!(slice_cells("目录abc", 1, 3), "录");
        assert_eq!(slice_cells("目录abc", 2, 10), "录abc");
        assert_eq!(slice_cells("abc", 3, 5), "");
    }

    #[test]
    fn completion_leaves_the_cursor_after_the_insertion() {
        // `cd /t` + Tab used to complete to `cd /tmp/` with the cursor still
        // sitting on the `t`, so the next keystroke landed in the middle of
        // the path.
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("tmp")).unwrap();
        let mut prompt = prompt_at(root.path());
        prompt.set_line("cd t");
        assert!(complete_path(&mut prompt));
        assert_eq!(prompt.buf, "cd tmp/", "completed");
        assert_eq!(prompt.cursor, prompt.buf.len(), "cursor follows the text");
    }

    #[test]
    fn completion_with_a_cursor_mid_line_keeps_the_tail() {
        let root = tempfile::tempdir().unwrap();
        touch(root.path(), "report.txt");
        let mut prompt = prompt_at(root.path());
        prompt.set_line("rep  --dry-run");
        prompt.cursor = 3;
        assert!(complete_path(&mut prompt));
        assert_eq!(prompt.buf, "report.txt  --dry-run");
        assert_eq!(prompt.cursor, "report.txt".len());
    }

    #[test]
    fn prompt_prefix_counts_down() {
        let dir = Path::new("/tmp");
        assert_eq!(
            prompt_prefix(dir, None),
            "zmodem: files to send (in /tmp)> "
        );
        // Rounds up, so `[1s]` is shown for the final second rather than 0.
        assert_eq!(
            prompt_prefix(dir, Some(Duration::from_millis(1500))),
            "zmodem: files to send (in /tmp) [2s]> "
        );
        assert_eq!(
            prompt_prefix(dir, Some(Duration::ZERO)),
            "zmodem: files to send (in /tmp) [0s]> "
        );
    }

    #[test]
    fn cursor_column_is_one_based_and_wide_char_aware() {
        // `CSI G` addresses columns from 1, and a CJK path counts as two
        // cells per character, so the emitted G value must not count bytes.
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("目录"), 4);
    }

    #[test]
    fn human_duration_formats() {
        use std::time::Duration;
        assert_eq!(human_duration(Duration::from_millis(400)), "0.4s");
        assert_eq!(human_duration(Duration::from_secs(12)), "12s");
        assert_eq!(human_duration(Duration::from_secs(64)), "1m04s");
    }
}
