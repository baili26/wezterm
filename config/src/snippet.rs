//! Configuration for the snippet palette (personal-fork feature).
//!
//! The snippet palette is a self-drawn modal listing text snippets stored as
//! files in a directory.  This module only carries the knobs; the scanning,
//! matching and rendering live in `wezterm-gui`.
use std::path::PathBuf;

use crate::color::RgbaColor;
use wezterm_dynamic::{FromDynamic, ToDynamic};

/// How the snippet palette is laid out and behaves.
#[derive(Debug, Clone, FromDynamic, ToDynamic)]
pub struct SnippetPanelConfig {
    /// Directory holding the snippet files.  `None` means
    /// `$XDG_CONFIG_HOME/wezterm/snippets`.
    #[dynamic(default)]
    pub dir: Option<PathBuf>,

    /// Width of the panel as a fraction of the terminal columns.
    #[dynamic(default = "default_snippet_panel_width")]
    pub width: f32,

    /// Height of the panel as a fraction of the terminal rows.
    #[dynamic(default = "default_snippet_panel_height")]
    pub height: f32,

    /// Fraction of the panel width taken by the list column; the rest is
    /// the preview column.  The default leaves the preview with 60%.
    #[dynamic(default = "default_snippet_panel_list_ratio")]
    pub list_ratio: f32,

    /// Panel background.  Opaque black by default, because a transparent
    /// panel is unreadable when the terminal behind it has text.
    #[dynamic(default = "default_snippet_panel_bg")]
    pub bg: RgbaColor,

    /// Panel foreground.  `None` inherits the terminal's foreground color.
    #[dynamic(default)]
    pub fg: Option<RgbaColor>,

    /// Background of the selected row.
    #[dynamic(default = "default_snippet_panel_selection_bg")]
    pub selection_bg: RgbaColor,

    /// Foreground of the selected row's text; this is the "inverted" color
    /// that makes the bar legible.
    #[dynamic(default = "default_snippet_panel_selection_fg")]
    pub selection_fg: RgbaColor,

    /// Color used for the characters that matched the search query.
    #[dynamic(default = "default_snippet_panel_match_fg")]
    pub match_fg: RgbaColor,

    /// Show a desktop notification for snippet actions.  Off by default:
    /// the panel's footer already reports what happened, and many desktop
    /// notification daemons do not honour the timeout.
    ///
    /// `danger` snippets still get a notification even when this is off,
    /// because by then the panel has closed and the footer is gone.
    #[dynamic(default)]
    pub desktop_notifications: bool,

    /// How long a desktop notification stays up, in milliseconds.
    #[dynamic(default = "default_snippet_panel_notify_ms")]
    pub notify_timeout_ms: u64,

    /// Foreground for the footer/help line.  A bright color by default so the
    /// key hints are actually noticed.
    #[dynamic(default = "default_snippet_panel_footer_fg")]
    pub footer_fg: RgbaColor,

    /// How many blank rows are left below the last preview line when the
    /// preview is scrolled all the way down.  `3` stops the text from being
    /// pushed off the top with nothing left to read; `0` allows scrolling one
    /// body-row less than a full screen further down.
    #[dynamic(default = "default_snippet_panel_keep_preview_rows")]
    pub keep_preview_rows: usize,

    /// Whether the preview column soft-wraps long lines.  Off by default:
    /// yaml/sql snippets are easier to read with their real indentation.
    #[dynamic(default)]
    pub preview_wrap: bool,

    /// Whether `Ctrl+D` moves the snippet to `<dir>/.trash/` instead of
    /// refusing to delete.
    #[dynamic(default = "crate::default_true")]
    pub enable_trash: bool,

    /// Treat `$|$` in a single-line snippet as the cursor landing spot.
    /// Turn this off if you'd rather always leave the cursor at the end of
    /// the inserted text.
    #[dynamic(default = "crate::default_true")]
    pub placeholder: bool,

    /// Show the metadata block (title/path/tags/desc/kind/cursor) above the
    /// snippet body in the preview column.  `Ctrl+I` toggles this at runtime.
    #[dynamic(default = "crate::default_true")]
    pub preview_meta: bool,

    /// How much of the key-hint footer to show at first: `'Hidden'`,
    /// `'Short'` (one line, default) or `'Full'` (every key).
    /// `Ctrl+H` cycles through the three at runtime.
    #[dynamic(default)]
    pub footer: SnippetFooter,
}

fn default_snippet_panel_width() -> f32 {
    0.80
}

/// How much of the snippet panel's key-hint footer is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, FromDynamic, ToDynamic)]
pub enum SnippetFooter {
    /// No footer at all; the panel is one row shorter and the list gets it.
    Hidden,
    /// One line of the most-used keys.
    Short,
    /// Every key, one line, truncated with `…` if the panel is narrow.
    Full,
}

impl Default for SnippetFooter {
    fn default() -> Self {
        Self::Short
    }
}

fn default_snippet_panel_height() -> f32 {
    0.62
}

fn default_snippet_panel_list_ratio() -> f32 {
    0.40
}

fn default_snippet_panel_bg() -> RgbaColor {
    (0x00, 0x00, 0x00).into()
}

fn default_snippet_panel_selection_bg() -> RgbaColor {
    (0x3a, 0x3a, 0x4d).into()
}

fn default_snippet_panel_selection_fg() -> RgbaColor {
    (0xf0, 0xf0, 0xf0).into()
}

fn default_snippet_panel_match_fg() -> RgbaColor {
    (0xff, 0xd7, 0x5f).into()
}

fn default_snippet_panel_notify_ms() -> u64 {
    10_000
}

fn default_snippet_panel_footer_fg() -> RgbaColor {
    (0x8b, 0xd8, 0xff).into()
}

fn default_snippet_panel_keep_preview_rows() -> usize {
    3
}

impl Default for SnippetPanelConfig {
    fn default() -> Self {
        Self {
            dir: None,
            width: default_snippet_panel_width(),
            height: default_snippet_panel_height(),
            list_ratio: default_snippet_panel_list_ratio(),
            bg: default_snippet_panel_bg(),
            fg: None,
            selection_bg: default_snippet_panel_selection_bg(),
            selection_fg: default_snippet_panel_selection_fg(),
            match_fg: default_snippet_panel_match_fg(),
            desktop_notifications: false,
            notify_timeout_ms: default_snippet_panel_notify_ms(),
            footer_fg: default_snippet_panel_footer_fg(),
            keep_preview_rows: default_snippet_panel_keep_preview_rows(),
            preview_wrap: false,
            enable_trash: true,
            placeholder: true,
            preview_meta: true,
            footer: SnippetFooter::default(),
        }
    }
}

impl SnippetPanelConfig {
    /// Resolved snippet directory, falling back to
    /// `$XDG_CONFIG_HOME/wezterm/snippets`.
    pub fn snippet_dir(&self) -> PathBuf {
        match &self.dir {
            Some(dir) => dir.clone(),
            None => crate::CONFIG_DIR.join("snippets"),
        }
    }
}
