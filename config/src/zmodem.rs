//! Configuration for ZMODEM (lrzsz `rz`/`sz`) file transfer.
//!
//! This is a personal-fork feature.  The protocol engine and the driver loop
//! live in `mux::zmodem`; this module only carries the knobs.
//!
//! ZMODEM is an *inbound* protocol: a program running in the pane (typically
//! `sz` on a remote host reached through `ssh`) asks the terminal to receive a
//! file, and the terminal writes it to local disk.  Because that lets a remote
//! process create local files, `enabled` defaults to `false` and downloads are
//! additionally gated on `allow_download_protocols`.
use std::path::PathBuf;

use wezterm_dynamic::{FromDynamic, ToDynamic};

/// Where the local file list for an upload comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, FromDynamic, ToDynamic)]
pub enum UploadFilePicker {
    /// Use a native picker when one is available and fall back to prompting in
    /// the pane.  This is the default.
    Auto,
    /// Always spawn the native picker (`zenity`).  If it cannot run, the
    /// transfer is cancelled rather than falling back.
    Native,
    /// Never spawn a native picker; always prompt for paths in the pane.
    Prompt,
}

impl Default for UploadFilePicker {
    fn default() -> Self {
        Self::Auto
    }
}

/// ZMODEM transfer behavior.
#[derive(Debug, Clone, FromDynamic, ToDynamic)]
pub struct ZmodemConfig {
    /// Master switch.  Off by default: an enabled terminal will write files to
    /// local disk when a program in a pane sends a ZMODEM trigger sequence.
    #[dynamic(default)]
    pub enabled: bool,

    /// Directory that received files are written to.  `None` means the
    /// platform download directory (`dirs_next::download_dir()`), falling back
    /// to `$HOME` if that is unavailable.
    #[dynamic(default)]
    pub download_dir: Option<PathBuf>,

    /// Overwrite an existing file of the same name instead of picking a
    /// non-conflicting name (`foo.1`, `foo.2`, ...).
    #[dynamic(default)]
    pub overwrite: bool,

    /// Whether to print transfer progress into the pane.  Turning this off
    /// only silences the text; the tab-bar progress indicator still updates.
    #[dynamic(default = "crate::default_true")]
    pub show_progress: bool,

    /// Whether to raise a desktop notification when a transfer finishes.
    ///
    /// This only controls the ZMODEM completion toast; other notifications
    /// (shell OSC 9, bell, and so on) are governed by
    /// `notification_handling` as usual.
    #[dynamic(default = "crate::default_true")]
    pub show_notification: bool,

    /// How long to wait for the peer before giving up, in seconds.  Applies to
    /// the initial handshake and to gaps in the middle of a transfer.  On
    /// expiry the cancel sequence is sent to the peer and normal terminal
    /// output resumes.
    ///
    /// An upload that is still waiting for you to choose files is exempt: the
    /// clock only starts once a transfer is under way.
    #[dynamic(default = "default_zmodem_timeout_secs")]
    pub timeout_secs: u64,

    /// How long to wait for the user to choose files for an upload, in
    /// seconds.  If nothing is chosen in time the transfer is cancelled.
    #[dynamic(default = "default_zmodem_upload_wait_secs")]
    pub upload_wait_secs: u64,

    /// Where the upload file list comes from.
    #[dynamic(default)]
    pub upload_file_picker: UploadFilePicker,

    /// How long the peer keeps accepting a file offer, in seconds.
    ///
    /// This is a property of the peer, not of WezTerm: lrzsz `rz` retries its
    /// handshake at 0/10/20/30/40 seconds, sends its last one around 50, and
    /// from then on only prints a bare `C` — it is no longer listening for a
    /// file at all.  The in-pane upload prompt therefore counts down from this
    /// value and cancels while the peer can still be told to stop, instead of
    /// letting you pick files for a transfer that can no longer happen.
    ///
    /// The default leaves a few seconds of margin before the measured ~50s
    /// cut-off.  Set to 0 to disable the countdown and rely on
    /// `upload_wait_secs` alone.
    #[dynamic(default = "default_zmodem_accept_window_secs")]
    pub accept_window_secs: u64,

    /// Resume interrupted downloads.
    ///
    /// When a partial file from an earlier attempt is found in `download_dir`,
    /// ask the sender to continue from where it stopped instead of starting
    /// over.  The peer must support ZRPOS (lrzsz `sz` does).  Off by default
    /// because it reuses an existing file rather than writing a fresh one.
    #[dynamic(default)]
    pub resume: bool,

    /// How much data may be sent to an upload peer before waiting for its
    /// acknowledgement, in KiB.  One subpacket is 1 KiB, so this is also the
    /// number of subpackets per window.
    ///
    /// ZMODEM allows a sender to stream a whole file without ever waiting when
    /// the receiver advertises nonstop I/O (lrzsz `rz` does: buffer length 0
    /// plus `CANOVIO`).  `zmodem2` cannot sample the reverse channel while a
    /// file read is outstanding, so streaming without a window means a peer
    /// that has stopped accepting data - a corrupt subpacket, a full disk, a
    /// killed `rz` - is invisible until the whole file has been written out.
    /// A bounded window costs one round trip per window (negligible on SSH)
    /// and turns that into one round trip of latency.
    ///
    /// `0` restores the old unbounded behaviour (fastest, but deaf).
    #[dynamic(default = "default_zmodem_upload_window_kib")]
    pub upload_window_kib: u64,
}

fn default_zmodem_timeout_secs() -> u64 {
    60
}

fn default_zmodem_upload_wait_secs() -> u64 {
    120
}

fn default_zmodem_accept_window_secs() -> u64 {
    45
}

fn default_zmodem_upload_window_kib() -> u64 {
    4096
}

impl Default for ZmodemConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            download_dir: None,
            overwrite: false,
            show_progress: true,
            show_notification: true,
            timeout_secs: default_zmodem_timeout_secs(),
            upload_wait_secs: default_zmodem_upload_wait_secs(),
            upload_file_picker: UploadFilePicker::default(),
            accept_window_secs: default_zmodem_accept_window_secs(),
            resume: false,
            upload_window_kib: default_zmodem_upload_window_kib(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wezterm_dynamic::{FromDynamic, ToDynamic, Value};

    #[test]
    fn defaults_are_off_and_generous() {
        let cfg = ZmodemConfig::default();
        assert!(!cfg.enabled, "ZMODEM must be opt-in");
        assert_eq!(cfg.download_dir, None);
        assert!(!cfg.overwrite);
        assert!(cfg.show_progress);
        assert!(cfg.show_notification);
        assert_eq!(cfg.timeout_secs, 60);
        assert_eq!(cfg.upload_file_picker, UploadFilePicker::Auto);
        assert_eq!(cfg.accept_window_secs, 45);
        assert!(!cfg.resume);
        assert_eq!(cfg.upload_window_kib, 4096);
        assert_ne!(
            cfg.upload_window_kib, 0,
            "an unbounded upload window makes the sender deaf to the peer"
        );
    }

    #[test]
    fn upload_window_is_configurable() {
        let value = Value::Object(
            vec![("upload_window_kib".to_dynamic(), 512u64.to_dynamic())]
                .into_iter()
                .collect(),
        );
        let cfg = ZmodemConfig::from_dynamic(&value, Default::default()).unwrap();
        assert_eq!(cfg.upload_window_kib, 512);

        let value = Value::Object(
            vec![("upload_window_kib".to_dynamic(), 0u64.to_dynamic())]
                .into_iter()
                .collect(),
        );
        let cfg = ZmodemConfig::from_dynamic(&value, Default::default()).unwrap();
        assert_eq!(cfg.upload_window_kib, 0, "0 must stay expressible");
    }

    #[test]
    fn parses_from_lua_table() {
        let value = Value::Object(
            vec![
                ("enabled".to_dynamic(), true.to_dynamic()),
                ("download_dir".to_dynamic(), "/tmp/dl".to_dynamic()),
                ("upload_file_picker".to_dynamic(), "Prompt".to_dynamic()),
                ("timeout_secs".to_dynamic(), 5u64.to_dynamic()),
            ]
            .into_iter()
            .collect(),
        );
        let cfg = ZmodemConfig::from_dynamic(&value, Default::default()).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.download_dir, Some(PathBuf::from("/tmp/dl")));
        assert_eq!(cfg.upload_file_picker, UploadFilePicker::Prompt);
        assert_eq!(cfg.timeout_secs, 5);
    }

    #[test]
    fn round_trips_through_dynamic() {
        let cfg = ZmodemConfig {
            enabled: true,
            download_dir: Some(PathBuf::from("/tmp/dl")),
            upload_file_picker: UploadFilePicker::Prompt,
            ..ZmodemConfig::default()
        };
        let back = ZmodemConfig::from_dynamic(&cfg.to_dynamic(), Default::default()).unwrap();
        assert!(back.enabled);
        assert_eq!(back.download_dir, cfg.download_dir);
        assert_eq!(back.upload_file_picker, UploadFilePicker::Prompt);
    }
}
