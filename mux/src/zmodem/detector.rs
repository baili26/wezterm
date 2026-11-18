//! Byte-level detection of a ZMODEM session start.
//!
//! WezTerm feeds pane output through a VT escape-sequence parser before the
//! terminal model ever sees it, and that parser necessarily destroys arbitrary
//! 8-bit binary data (a `char`-wise `Print` action, control bytes turned into
//! `ControlCode`s).  ZMODEM needs the raw byte stream, so the only place a
//! session can be intercepted is *before* the parser.
//!
//! This module implements that interception.  It is deliberately I/O-free and
//! has no dependency on panes, so it can be unit tested exhaustively.
//!
//! # What lrzsz actually sends
//!
//! Measured with lrzsz 0.12.21rc:
//!
//! * `sz file` emits `rz\r` (3 bytes, ordinary text) followed by
//!   `**\x18B` + `00000000000000` + `\r\n\x8a\x11` - a ZRQINIT header.  The
//!   header asks the terminal to *receive*, so this is a **download**.
//! * `rz` emits `rz waiting to receive.` (text) followed by
//!   `**\x18B` + `0100000023be50` + `\r\n\x8a\x11` - a ZRINIT header, asking
//!   the terminal to *send*, so this is an **upload**.
//!
//! Both are ZHEX-encoded headers: `**` (ZPAD), `\x18` (ZDLE), `B` (ZHEX),
//! then fourteen hex digits (5 payload bytes + 2 CRC-16 bytes).  The first
//! payload byte is the frame type: 0 = ZRQINIT, 1 = ZRINIT, 8 = ZFIN.
//!
//! Text before the signature (and any bytes of the signature itself) is handed
//! to the VT parser so the user keeps seeing what the program printed.  Byte
//! sequences *after* the signature start the protocol stream and are fed to
//! the ZMODEM engine instead.

use std::borrow::Cow;
use std::ops::Range;

/// Which side of the transfer we are on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Peer is `sz`: it sends, we receive and write to local disk.
    Download,
    /// Peer is `rz`: it receives, we read local files and send.
    Upload,
}

const ZPAD: u8 = b'*';
const ZDLE: u8 = 0x18;
const ZHEX: u8 = b'B';

/// `**\x18B` + 14 hex digits.
const SIG_LEN: usize = 4 + 14;

fn is_hex(b: u8) -> bool {
    b.is_ascii_digit() || (b'a'..=b'f').contains(&b) || (b'A'..=b'F').contains(&b)
}

/// Whether `b` is a plausible frame type for a transfer start.
fn mode_for_type_byte(b: u8) -> Option<Mode> {
    match b {
        b'0' => Some(Mode::Download),
        b'1' => Some(Mode::Upload),
        _ => None,
    }
}

/// Is `bytes` a prefix of a signature that could still trigger?
///
/// `bytes` is short (at most [`SIG_LEN`]) and is always a *suffix* of the data
/// seen so far.
fn is_possible_prefix(bytes: &[u8]) -> bool {
    let pattern = [ZPAD, ZPAD, ZDLE, ZHEX];
    for (idx, b) in bytes.iter().enumerate() {
        if idx < pattern.len() {
            if *b != pattern[idx] {
                return false;
            }
        } else if !is_hex(*b) {
            return false;
        }
    }
    // Once the frame-type digits are known, anything but ZRQINIT/ZRINIT can no
    // longer trigger, so there is no point holding the bytes back.
    if bytes.len() >= 6 {
        if bytes[4] != b'0' || !matches!(bytes[5], b'0' | b'1') {
            return false;
        }
    }
    true
}

/// Look for a complete signature in `hay`.  Returns the byte range of the
/// signature and the direction of the transfer.
fn find_signature(hay: &[u8]) -> Option<(Range<usize>, Mode)> {
    if hay.len() < SIG_LEN {
        return None;
    }
    let mut i = 0;
    while i + SIG_LEN <= hay.len() {
        if hay[i] == ZPAD && hay[i + 1] == ZPAD && hay[i + 2] == ZDLE && hay[i + 3] == ZHEX {
            let hex = &hay[i + 4..i + SIG_LEN];
            if hex.iter().all(|b| is_hex(*b)) {
                if let Some(mode) = mode_for_type_byte(hex[1]) {
                    if hex[0] == b'0' {
                        return Some((i..i + SIG_LEN, mode));
                    }
                }
            }
        }
        i += 1;
    }
    None
}

/// The result of feeding a chunk to the detector.
pub struct Feed<'a> {
    /// Bytes that belong to normal terminal output and must go to the VT
    /// parser, in order.
    pub passthrough: Cow<'a, [u8]>,
    /// Set when a session starts.  `take_seed()` then yields the raw bytes
    /// (starting with the signature frame) that must be fed to the engine.
    pub trigger: Option<Mode>,
    /// Program output that arrived in the same chunk *before* the signature.
    ///
    /// lrzsz prints `rz waiting to receive.` the first time it starts, and
    /// `sz` prints a bare `rz\r`; a shell echoes the typed command.  A peer
    /// merely retrying an already-cancelled handshake prints nothing at all -
    /// measured: the retries are exactly the 21 bytes `**\x18B0100...\r\x8a\x11`.
    /// The parse loop uses this to tell "the user started a transfer" from
    /// "the previous one is still babbling".
    pub fresh: bool,
}

/// Per-pane ZMODEM sniffing state.
///
/// Feed every chunk read from the pane's PTY.  Normal output is passed
/// through; a session trigger is reported once, and from then on the caller
/// owns the byte stream until the session finishes and calls [`reset`].
///
/// [`reset`]: ZmodemDetector::reset
#[derive(Default)]
pub struct ZmodemDetector {
    /// Bytes withheld from the previous chunk because they might be the start
    /// of a signature.
    held: Vec<u8>,
    /// Scratch buffer reused for `held + input`.
    scratch: Vec<u8>,
    /// Raw bytes after (and including) the signature, for the engine.
    seed: Vec<u8>,
    trigger: Option<Mode>,
    /// lrzsz writes `OO` (over and out) after the last frame.  If we let it
    /// reach the VT parser it is drawn as stray text on the prompt line, which
    /// survives the next `Enter` and looks like corruption.  Armed when a
    /// session ends; the next leading `O` bytes are dropped.
    epilogue_run: usize,
}

impl ZmodemDetector {
    pub fn new() -> Self {
        Self::default()
    }

    /// True when a trigger is pending and has not been taken yet.
    pub fn is_triggered(&self) -> bool {
        self.trigger.is_some()
    }

    /// Hand a chunk of pane output to the detector.
    ///
    /// `passthrough` borrows `input` in the common case and is owned when a
    /// partial signature had to be held back, so the returned value never
    /// borrows the detector itself.
    pub fn feed<'a>(&mut self, input: &'a [u8]) -> Feed<'a> {
        debug_assert!(self.trigger.is_none(), "feed() after a trigger");

        // Drop the `OO` epilogue a finished session leaves behind.  Only a
        // leading run of `O`s is removed, and only when armed, so ordinary
        // output is never touched.
        let mut input = input;
        if self.epilogue_run > 0 {
            let run = input.iter().take_while(|b| **b == b'O').count();
            let skipped = run.min(self.epilogue_run);
            input = &input[skipped..];
            self.epilogue_run -= skipped;
            // If anything that is not `O` follows, stop waiting.  A chunk that
            // ended inside the run keeps the counter armed for the next one.
            if !input.is_empty() {
                self.epilogue_run = 0;
            }
        }

        // Fast path: nothing held back and no byte that could start a
        // signature.  A lone trailing `*` is not enough to start one, so it is
        // safe to pass through here; the slow path is what holds a suffix back.
        if self.held.is_empty() && !input.contains(&ZPAD) {
            return Feed {
                passthrough: Cow::Borrowed(input),
                trigger: None,
                fresh: false,
            };
        }

        self.scratch.clear();
        self.scratch.extend_from_slice(&self.held);
        self.scratch.extend_from_slice(input);
        self.held.clear();

        if let Some((range, mode)) = find_signature(&self.scratch) {
            self.seed.clear();
            self.seed.extend_from_slice(&self.scratch[range.start..]);
            self.trigger = Some(mode);

            // Whether the program said anything of its own before the
            // handshake.  Computed *before* the preamble is stripped below:
            // `sz` prints exactly `rz\r` and nothing else, so measuring after
            // the strip made every genuine `sz` look like a bare retry of a
            // cancelled handshake - and within the abort cooldown it was
            // answered with CAN instead of starting a download.
            let spoke_first = range.start > 0;

            // lrzsz prints a literal `rz\r` before the handshake so that a
            // terminal which does not understand ZMODEM shows something
            // meaningful.  We do understand it and immediately replace it with
            // our own status lines, so drop it rather than leaving three bytes
            // of noise on the line.
            let mut end = range.start;
            for preamble in [&b"rz\r"[..], &b"rz\n"[..]] {
                if end >= preamble.len() && self.scratch[end - preamble.len()..end] == *preamble {
                    end -= preamble.len();
                    break;
                }
            }
            return Feed {
                passthrough: Cow::Owned(self.scratch[..end].to_vec()),
                trigger: Some(mode),
                fresh: spoke_first,
            };
        }

        // No complete signature.  Hold back the longest suffix that could
        // still grow into one, and pass the rest through.
        let len = self.scratch.len();
        let max_hold = SIG_LEN.min(len);
        let mut hold = 0;
        for n in (1..=max_hold).rev() {
            if is_possible_prefix(&self.scratch[len - n..]) {
                hold = n;
                break;
            }
        }
        let split = len - hold;
        self.held.extend_from_slice(&self.scratch[split..]);
        Feed {
            passthrough: Cow::Owned(self.scratch[..split].to_vec()),
            trigger: None,
            fresh: false,
        }
    }

    /// Take the pending direction, if any.  Must be called after a [`Feed`]
    /// that reported a trigger.
    pub fn take_trigger(&mut self) -> Option<Mode> {
        self.trigger.take()
    }

    /// Raw bytes that start the protocol stream (the signature frame followed
    /// by whatever shared a chunk with it).
    pub fn take_seed(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.seed)
    }

    /// Release any bytes still held back; used at EOF and when the feature is
    /// disabled mid-stream.
    pub fn flush(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.held)
    }

    /// Forget everything, ready for the next session.  lrzsz re-sends its
    /// handshake periodically, so a failed or cancelled session must reset the
    /// detector to be able to pick it up again.
    pub fn reset(&mut self) {
        self.held.clear();
        self.scratch.clear();
        self.seed.clear();
        self.trigger = None;
    }

    /// Expect (and drop) the peer's `OO` end-of-session marker.
    pub fn expect_epilogue(&mut self) {
        self.epilogue_run = 2;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOWNLOAD_SIG: &[u8] = b"**\x18B00000000000000";
    const UPLOAD_SIG: &[u8] = b"**\x18B0100000023be50";

    fn collect(mut input: Vec<Vec<u8>>) -> (Vec<u8>, Option<Mode>, Vec<u8>) {
        let mut detector = ZmodemDetector::new();
        let mut passthrough = Vec::new();
        let mut trigger = None;
        let mut seed = Vec::new();
        for chunk in input.drain(..) {
            let feed = detector.feed(&chunk);
            passthrough.extend_from_slice(&feed.passthrough);
            if feed.trigger.is_some() {
                trigger = detector.take_trigger();
                seed = detector.take_seed();
                break;
            }
        }
        if trigger.is_none() {
            passthrough.extend_from_slice(&detector.flush());
        }
        (passthrough, trigger, seed)
    }

    #[test]
    fn detects_download_in_one_chunk() {
        let mut input = b"rz\r".to_vec();
        input.extend_from_slice(DOWNLOAD_SIG);
        input.extend_from_slice(b"\r\n\x8a\x11");
        let (pass, trigger, seed) = collect(vec![input]);
        assert!(pass.is_empty(), "the `rz\\r` preamble is dropped");
        assert_eq!(trigger, Some(Mode::Download));
        assert!(seed.starts_with(DOWNLOAD_SIG));
        assert!(seed.ends_with(b"\r\n\x8a\x11"));
    }

    #[test]
    fn detects_upload_in_one_chunk() {
        let mut input = b"rz waiting to receive.".to_vec();
        input.extend_from_slice(UPLOAD_SIG);
        let (pass, trigger, seed) = collect(vec![input]);
        assert_eq!(pass, b"rz waiting to receive.");
        assert_eq!(trigger, Some(Mode::Upload));
        assert!(seed.starts_with(UPLOAD_SIG));
    }

    #[test]
    fn detects_signature_split_at_every_boundary() {
        let mut input = b"hello".to_vec();
        input.extend_from_slice(UPLOAD_SIG);
        input.extend_from_slice(b"\r\n");
        for split in 0..=input.len() {
            let chunks = vec![input[..split].to_vec(), input[split..].to_vec()];
            let (pass, trigger, seed) = collect(chunks);
            assert_eq!(pass, b"hello", "split at {}", split);
            assert_eq!(trigger, Some(Mode::Upload), "split at {}", split);
            assert!(seed.starts_with(UPLOAD_SIG), "split at {}", split);
        }
    }

    #[test]
    fn detects_signature_fed_one_byte_at_a_time() {
        let mut input = b"x".to_vec();
        input.extend_from_slice(DOWNLOAD_SIG);
        let chunks: Vec<Vec<u8>> = input.iter().map(|b| vec![*b]).collect();
        let (pass, trigger, seed) = collect(chunks);
        assert_eq!(pass, b"x");
        assert_eq!(trigger, Some(Mode::Download));
        assert!(seed.starts_with(DOWNLOAD_SIG));
    }

    #[test]
    fn two_signatures_in_one_chunk_uses_the_first() {
        let mut input = b"a".to_vec();
        input.extend_from_slice(UPLOAD_SIG);
        input.extend_from_slice(b"b");
        input.extend_from_slice(DOWNLOAD_SIG);
        let (pass, trigger, seed) = collect(vec![input]);
        assert_eq!(pass, b"a");
        assert_eq!(trigger, Some(Mode::Upload));
        assert!(seed.windows(UPLOAD_SIG.len()).any(|w| w == UPLOAD_SIG));
    }

    #[test]
    fn zfin_does_not_trigger() {
        let mut input = b"".to_vec();
        input.extend_from_slice(b"**\x18B08");
        input.extend_from_slice(b"0000000e0e2c");
        let (pass, trigger, _) = collect(vec![input.clone()]);
        assert_eq!(trigger, None);
        assert_eq!(pass, input);
    }

    #[test]
    fn wrong_zdle_does_not_trigger() {
        let input = b"**\x19B0100000023be50".to_vec();
        let (pass, trigger, _) = collect(vec![input.clone()]);
        assert_eq!(trigger, None);
        assert_eq!(pass, input);
    }

    #[test]
    fn truncated_signature_is_held_then_flushed() {
        let input = b"**\x18B0100".to_vec();
        let (pass, trigger, _) = collect(vec![input.clone()]);
        assert_eq!(trigger, None);
        assert_eq!(pass, input);
    }

    #[test]
    fn non_hex_payload_does_not_trigger() {
        let input = b"**\x18B0100000023bezz".to_vec();
        let (pass, trigger, _) = collect(vec![input.clone()]);
        assert_eq!(trigger, None);
        assert_eq!(pass, input);
    }

    #[test]
    fn cancel_sequence_does_not_trigger() {
        let input = b"\x18\x18\x18\x18\x18\x08\x08\x08\x08\x08".to_vec();
        let (pass, trigger, _) = collect(vec![input.clone()]);
        assert_eq!(trigger, None);
        assert_eq!(pass, input);
    }

    #[test]
    fn plain_text_is_passed_through_unmodified() {
        let text = b"the quick brown fox\r\njumps over 12 lazy dogs\r\n";
        let (pass, trigger, _) = collect(vec![text.to_vec()]);
        assert_eq!(trigger, None);
        assert_eq!(pass, text);
    }

    #[test]
    fn lone_asterisks_are_not_held() {
        // A trailing single '*' cannot start a signature, so it must not be
        // withheld.
        let (pass, trigger, _) = collect(vec![b"100% *".to_vec()]);
        assert_eq!(trigger, None);
        assert_eq!(pass, b"100% *");
    }

    #[test]
    fn swallows_the_oo_epilogue() {
        let mut detector = ZmodemDetector::new();
        detector.expect_epilogue();
        // `OO` arriving together with the next prompt must be dropped...
        let feed = detector.feed(b"OOuser@host:~$ ");
        assert_eq!(&*feed.passthrough, b"user@host:~$ ");
        // ...and the arming is one-shot.
        let feed = detector.feed(b"OOnot-a-marker");
        assert_eq!(&*feed.passthrough, b"OOnot-a-marker");
    }

    #[test]
    fn oo_split_across_chunks_is_swallowed() {
        let mut detector = ZmodemDetector::new();
        detector.expect_epilogue();
        let mut out = Vec::new();
        out.extend_from_slice(&detector.feed(b"O").passthrough);
        out.extend_from_slice(&detector.feed(b"O$ ").passthrough);
        assert_eq!(out, b"$ ");
    }

    #[test]
    fn drops_the_rz_preamble() {
        let mut input = b"rz\r".to_vec();
        input.extend_from_slice(UPLOAD_SIG);
        let (pass, trigger, seed) = collect(vec![input]);
        assert_eq!(trigger, Some(Mode::Upload));
        assert!(
            pass.is_empty(),
            "the `rz\\r` preamble must not reach the screen"
        );
        assert!(seed.starts_with(UPLOAD_SIG));
    }

    #[test]
    fn sz_preamble_still_counts_as_fresh() {
        // `sz` prints only `rz\r` before its handshake.  Measuring freshness
        // after stripping that preamble made a real `sz` indistinguishable
        // from a peer re-sending a handshake we had just cancelled, so within
        // the abort cooldown it was answered with CAN and the download never
        // started.
        let mut detector = ZmodemDetector::new();
        let mut input = b"rz\r".to_vec();
        input.extend_from_slice(DOWNLOAD_SIG);
        let feed = detector.feed(&input);
        assert_eq!(feed.trigger, Some(Mode::Download));
        assert!(
            feed.fresh,
            "`sz` printed its preamble, so this is a new session"
        );

        // A bare retry has no preamble at all and must not count as fresh.
        let mut detector = ZmodemDetector::new();
        let feed = detector.feed(UPLOAD_SIG);
        assert_eq!(feed.trigger, Some(Mode::Upload));
        assert!(
            !feed.fresh,
            "a bare handshake is a retry, not a new session"
        );
    }

    #[test]
    fn reset_clears_state() {
        let mut detector = ZmodemDetector::new();
        let mut input = DOWNLOAD_SIG.to_vec();
        input.extend_from_slice(b"\r\n");
        let feed = detector.feed(&input);
        assert!(feed.trigger.is_some());
        assert_eq!(detector.take_trigger(), Some(Mode::Download));
        detector.reset();
        assert!(!detector.is_triggered());
        assert!(detector.take_seed().is_empty());

        // A second, independent session in the same pane is detected.
        let feed = detector.feed(UPLOAD_SIG);
        assert_eq!(feed.trigger, Some(Mode::Upload));
    }
}
