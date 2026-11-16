//! A generic two-column modal: a searchable list on the left, a preview of
//! the selected item on the right.
//!
//! This is a personal-fork addition.  The snippet palette is the first
//! consumer; an alert list or an AI-context picker could reuse it by
//! implementing [`ListPreviewSource`].
//!
//! Visual rules:
//!   * the panel is an opaque box with a thin rounded border and two rules
//!     (under the input line and above the footer), so it never blends into
//!     the terminal text behind it;
//!   * the selection bar covers only the list column, and its text switches
//!     to `selection_fg` (inverted), like a theme's selection;
//!   * match highlighting applies to the title only - preview and metadata
//!     keep their own colors;
//!   * the preview column can be drag-selected and copies to the clipboard.
use crate::overlay::selector::{matcher_indices, matcher_pattern};
use crate::termwindow::box_model::*;
use crate::termwindow::modal::Modal;
use crate::termwindow::render::corners::{
    BOTTOM_LEFT_ROUNDED_CORNER, BOTTOM_RIGHT_ROUNDED_CORNER, TOP_LEFT_ROUNDED_CORNER,
    TOP_RIGHT_ROUNDED_CORNER,
};
use crate::termwindow::{DimensionContext, TermWindow};
use config::Dimension;
use finl_unicode::grapheme_clusters::Graphemes;
use mux::pane::{Pane, PaneId};
use std::cell::{Ref, RefCell};
use std::sync::Arc;
use termwiz::cell::grapheme_column_width;
use wezterm_term::input::ClickPosition;
use wezterm_term::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use window::color::{LinearRgba, SrgbaTuple};
use window::WindowOps;

/// Width of `s` in terminal cells.
pub fn display_width(s: &str) -> usize {
    Graphemes::new(s)
        .map(|g| grapheme_column_width(g, None))
        .sum()
}

/// Truncate `s` so that it fits in `width` cells, appending `…` if it had
/// to be cut.  Widths are display widths, so CJK and emoji are handled.
pub fn truncate_to_width(s: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if display_width(s) <= width {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for g in Graphemes::new(s) {
        let gw = grapheme_column_width(g, None);
        if w + gw + 1 > width {
            break;
        }
        out.push_str(g);
        w += gw;
    }
    out.push('…');
    out
}

/// Truncate to `width` cells and pad with spaces so that the result is
/// exactly `width` cells wide.
pub fn pad_to_width(s: &str, width: usize) -> String {
    let mut out = truncate_to_width(s, width);
    let w = display_width(&out);
    for _ in w..width {
        out.push(' ');
    }
    out
}

/// Take the characters of `s` that occupy display columns `[start, end)`.
fn slice_columns(s: &str, start: usize, end: usize) -> String {
    if start >= end {
        return String::new();
    }
    let mut out = String::new();
    let mut col = 0;
    for g in Graphemes::new(s) {
        let gw = grapheme_column_width(g, None);
        let next = col + gw;
        if next > start && col < end {
            out.push_str(g);
        }
        col = next;
        if col >= end {
            break;
        }
    }
    out
}

/// A run of text with one style.
#[derive(Debug, Clone)]
pub struct Span {
    pub text: String,
    /// Render dimmer than the normal foreground
    pub dim: bool,
    /// Render with the "matched a search query" color
    pub highlight: bool,
}

impl Span {
    pub fn normal(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            dim: false,
            highlight: false,
        }
    }

    pub fn dim(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            dim: true,
            highlight: false,
        }
    }

    pub fn highlight(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            dim: false,
            highlight: true,
        }
    }

    fn width(&self) -> usize {
        display_width(&self.text)
    }
}

/// A preview line: a run of styled spans.
pub type PreviewLine = Vec<Span>;

/// One row of the list column.
#[derive(Debug, Clone, Default)]
pub struct ListRowText {
    /// Left gutter character: danger `!`, pin `★` or a space
    pub gutter: String,
    pub title: String,
    /// Right-aligned text inside the list column, e.g. tags
    pub badges: String,
}

/// What the modal is currently doing besides browsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputMode {
    Browse,
    /// Collecting a one-line answer; `prompt` is rendered in the title row.
    Prompt {
        prompt: String,
        text: String,
        /// Caret position, in characters
        cursor: usize,
        /// Item the answer applies to.  Fixed when the prompt opens so a
        /// stray click on another row cannot redirect the edit.
        target: Option<usize>,
    },
    /// Asking for confirmation; `prompt` is rendered in the title row.
    Confirm {
        prompt: String,
    },
}

/// Context handed to the source when it wants to affect the pane.
pub struct AcceptCtx<'a> {
    pub term_window: &'a mut TermWindow,
    /// The pane that had focus when the modal opened; insertions go here
    /// regardless of which pane looks active now.
    pub pane: Option<Arc<dyn Pane>>,
    /// Whether `?` in a single-line snippet moves the cursor
    pub placeholder: bool,
    /// Messages to surface as desktop notifications after the callback
    pub messages: Vec<String>,
    /// Messages that must be shown as a desktop notification even when the
    /// user turned them off (the panel is already gone by then).
    pub urgent_messages: Vec<String>,
    /// Set by the source to switch the modal into a sub-mode.
    pub mode_request: Option<InputMode>,
    /// Set by the source when the modal should close after this key.
    pub close_requested: bool,
    /// Set by the source to select a particular list item afterwards.
    pub select_request: Option<usize>,
    /// Message to render in the footer instead of the key hints.
    pub inline_message: Option<String>,
}

impl<'a> AcceptCtx<'a> {
    /// Insert text as a bracketed paste (no execution, newlines preserved).
    pub fn paste(&mut self, text: &str) -> bool {
        match &self.pane {
            Some(pane) => match pane.send_paste(text) {
                Ok(()) => true,
                Err(err) => {
                    log::error!("snippet: send_paste failed: {err:#}");
                    false
                }
            },
            None => false,
        }
    }

    /// Move the cursor left by `n` cells using the left-arrow sequence.
    ///
    /// This is deliberately sent a moment after the paste: shells process a
    /// bracketed paste as one chunk, and arrow bytes that arrive while the
    /// paste is still being handled are swallowed.  A short delay makes the
    /// landing spot reliable across bash/zsh/psql/vim.
    pub fn move_cursor_left(&mut self, n: usize) {
        if n == 0 {
            return;
        }
        if let Some(pane) = self.pane.as_ref().map(Arc::clone) {
            promise::spawn::spawn(async move {
                smol::Timer::after(std::time::Duration::from_millis(60)).await;
                let mut bytes = Vec::with_capacity(n * 3);
                for _ in 0..n {
                    bytes.extend_from_slice(b"\x1b[D");
                }
                let mut writer = pane.writer();
                if let Err(err) = writer.write_all(&bytes).and_then(|()| writer.flush()) {
                    log::error!("snippet: cursor move failed: {err:#}");
                }
            })
            .detach();
        }
    }

    /// Submit the line: a stray `\r` written immediately after a bracketed
    /// paste is sometimes swallowed together with the paste terminator, so
    /// deliver it on a short timer instead.
    pub fn submit_line(&mut self) {
        if let Some(pane) = self.pane.as_ref().map(Arc::clone) {
            promise::spawn::spawn(async move {
                smol::Timer::after(std::time::Duration::from_millis(80)).await;
                let mut writer = pane.writer();
                if let Err(err) = writer.write_all(b"\r").and_then(|()| writer.flush()) {
                    log::error!("snippet: submit failed: {err:#}");
                }
            })
            .detach();
        }
    }

    /// Queue a desktop notification, subject to
    /// `snippet_panel.desktop_notifications`.
    ///
    /// The snippet source only uses [`Self::notify_inline`] and
    /// [`Self::notify_always`] today; this stays as the API for sources that
    /// outlive the panel (an alert list, for example).
    #[allow(dead_code)]
    pub fn notify(&mut self, message: impl Into<String>) {
        self.messages.push(message.into());
    }

    /// Show `message` in the panel's footer.  No desktop notification.
    pub fn notify_inline(&mut self, message: impl Into<String>) {
        self.inline_message = Some(message.into());
    }

    /// Always show a desktop notification, regardless of
    /// `snippet_panel.desktop_notifications`.
    pub fn notify_always(&mut self, message: impl Into<String>) {
        self.urgent_messages.push(message.into());
    }

    /// Ask the modal to enter a sub-mode (used by management actions).
    pub fn request_mode(&mut self, mode: InputMode) {
        self.mode_request = Some(mode);
    }

    /// Open a one-line prompt whose answer applies to `target`.
    pub fn prompt_for(&mut self, prompt: String, text: String, target: Option<usize>) {
        let cursor = text.chars().count();
        self.mode_request = Some(InputMode::Prompt {
            prompt,
            text,
            cursor,
            target,
        });
    }

    /// Ask the modal to close after the current key is handled.
    pub fn request_close(&mut self) {
        self.close_requested = true;
    }
}

/// A data source for the list/preview modal.
///
/// Implementors keep their own state and return owned values, because the
/// modal may call these while holding a `&mut TermWindow`.
pub trait ListPreviewSource {
    /// Shown at the left of the input row.
    fn source_name(&self) -> &str;

    /// Re-read the underlying data (bound to Ctrl+R).
    fn rescan(&self);

    /// Bumped whenever the underlying data changed.  The modal re-runs
    /// [`Self::filter`] when this differs from the results it holds, so a
    /// rescan or a create/delete shows up immediately.
    fn generation(&self) -> u64;

    /// Indices of the items matching `query`, already sorted for display.
    fn filter(&self, query: &str) -> Vec<usize>;

    /// Total number of items, shown as the right-hand side of `n/total`.
    fn total(&self) -> usize;

    fn row(&self, idx: usize) -> ListRowText;

    /// Preview lines for `idx`, already truncated/wrapped to `width` cells.
    /// These are also what a drag-selection copies.
    ///
    /// `show_meta` is the Ctrl+I toggle: sources may render a header when true.
    fn preview_lines(
        &self,
        idx: usize,
        width: usize,
        wrap: bool,
        show_meta: bool,
    ) -> Vec<PreviewLine>;

    /// Insert (and possibly run) the item.  `exec` is true for Ctrl+Enter.
    fn accept(&self, ctx: &mut AcceptCtx, idx: usize, exec: bool);

    /// Handle a key that the modal doesn't use itself.  Return true if the
    /// key was consumed.
    fn custom_key(
        &self,
        ctx: &mut AcceptCtx,
        idx: Option<usize>,
        key: KeyCode,
        mods: KeyModifiers,
    ) -> bool;

    /// Called when the user answers a [`InputMode::Prompt`].
    fn accept_prompt(&self, _ctx: &mut AcceptCtx, _idx: usize, _text: &str) {}

    /// Called when the user confirms an [`InputMode::Confirm`].
    fn confirm(&self, _ctx: &mut AcceptCtx, _idx: usize) {}

    /// One line of key hints, rendered at the bottom.
    fn footer(&self) -> String;

    /// The full key list, shown while the help overlay is toggled on.
    fn help_text(&self) -> &str {
        ""
    }

    /// When a draft is in progress (e.g. the user pressed Ctrl+N and is
    /// typing a title), the preview column shows this instead of the
    /// selected item.  `None` means "show the selected item".
    fn preview_override(
        &self,
        _width: usize,
        _wrap: bool,
        _draft_title: &str,
        _show_meta: bool,
    ) -> Option<Vec<PreviewLine>> {
        None
    }
}

struct MatchResults {
    selection: String,
    generation: u64,
    matches: Vec<usize>,
}

/// Where the panel ended up on screen, in terminal cells.
#[derive(Debug, Clone, Copy)]
struct PanelRect {
    x: usize,
    y: usize,
    cols: usize,
    rows: usize,
    /// Row (panel-local) of the first body row
    body_top: usize,
    body_rows: usize,
    list_cols: usize,
    preview_cols: usize,
}

/// A drag-selection inside the preview: (start row, start column) inclusive,
/// (end row, end column) exclusive, in preview-local coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PreviewSelection {
    anchor: (usize, usize),
    cursor: (usize, usize),
}

impl PreviewSelection {
    /// Normalised (first, last) ordering of the two ends.
    fn ordered(&self) -> ((usize, usize), (usize, usize)) {
        if (self.anchor.0, self.anchor.1) <= (self.cursor.0, self.cursor.1) {
            (self.anchor, self.cursor)
        } else {
            (self.cursor, self.anchor)
        }
    }
}

pub struct ListPreviewModal {
    source: Box<dyn ListPreviewSource>,
    selection: RefCell<String>,
    /// Caret position inside the search box, in characters
    selection_cursor: RefCell<usize>,
    matches: RefCell<Option<MatchResults>>,
    selected_row: RefCell<usize>,
    top_row: RefCell<usize>,
    preview_scroll: RefCell<usize>,
    max_rows_on_screen: RefCell<usize>,
    mode: RefCell<InputMode>,
    wrap: RefCell<bool>,
    element: RefCell<Option<Vec<ComputedElement>>>,
    rect: RefCell<Option<PanelRect>>,
    /// The preview text as rendered, for drag-selection copying
    preview_raw: RefCell<Vec<String>>,
    preview_selection: RefCell<Option<PreviewSelection>>,
    last_click: RefCell<Option<(MouseButton, ClickPosition, std::time::Instant, usize)>>,
    /// Whether the key-hint footer is showing: hidden, the one-line summary,
    /// or the full key list.  `Ctrl+H` cycles through the three.
    modal_footer: RefCell<FooterMode>,
    /// Whether the preview shows its metadata header (Ctrl+I toggles it).
    show_meta: RefCell<bool>,
    /// A short hint shown in place of the footer, with the instant after
    /// which it should be ignored.  A hint forces the footer back on even when
    /// the user hid it.
    transient: RefCell<Option<(String, std::time::Instant)>>,
    /// Whether the cached `element` was built with a footer row.  A hint
    /// appearing or expiring changes that, and the cached layout has to be
    /// thrown away so the panel grows/shrinks by one row.
    element_has_footer: RefCell<bool>,
    pane_id: Option<PaneId>,
}

/// How much of the key-hint footer is visible.
///
/// `Ctrl+H` cycles Hidden -> Short -> Full -> Hidden.  A transient message
/// (a hint from an action) temporarily forces the footer on, so a hidden
/// footer can still tell you what just happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FooterMode {
    Hidden,
    Short,
    Full,
}

impl FooterMode {
    fn next(self) -> Self {
        match self {
            FooterMode::Hidden => FooterMode::Short,
            FooterMode::Short => FooterMode::Full,
            FooterMode::Full => FooterMode::Hidden,
        }
    }
}

impl Default for FooterMode {
    fn default() -> Self {
        Self::Short
    }
}

impl From<config::SnippetFooter> for FooterMode {
    fn from(value: config::SnippetFooter) -> Self {
        match value {
            config::SnippetFooter::Hidden => FooterMode::Hidden,
            config::SnippetFooter::Short => FooterMode::Short,
            config::SnippetFooter::Full => FooterMode::Full,
        }
    }
}

impl ListPreviewModal {
    pub fn new(
        source: Box<dyn ListPreviewSource>,
        pane_id: Option<PaneId>,
        wrap: bool,
        preview_meta: bool,
        footer: FooterMode,
    ) -> Self {
        Self {
            source,
            selection: RefCell::new(String::new()),
            selection_cursor: RefCell::new(0),
            matches: RefCell::new(None),
            selected_row: RefCell::new(0),
            top_row: RefCell::new(0),
            preview_scroll: RefCell::new(0),
            max_rows_on_screen: RefCell::new(1),
            mode: RefCell::new(InputMode::Browse),
            wrap: RefCell::new(wrap),
            element: RefCell::new(None),
            rect: RefCell::new(None),
            preview_raw: RefCell::new(Vec::new()),
            preview_selection: RefCell::new(None),
            last_click: RefCell::new(None),
            modal_footer: RefCell::new(footer),
            show_meta: RefCell::new(preview_meta),
            transient: RefCell::new(None),
            element_has_footer: RefCell::new(false),
            pane_id,
        }
    }

    fn updated_input(&self) {
        *self.selected_row.borrow_mut() = 0;
        *self.top_row.borrow_mut() = 0;
        *self.preview_scroll.borrow_mut() = 0;
        *self.preview_selection.borrow_mut() = None;
        self.element.borrow_mut().take();
    }

    fn match_count(&self) -> usize {
        self.matches
            .borrow()
            .as_ref()
            .map(|m| m.matches.len())
            .unwrap_or(0)
    }

    fn selected_index(&self) -> Option<usize> {
        let row = *self.selected_row.borrow();
        self.matches
            .borrow()
            .as_ref()
            .and_then(|m| m.matches.get(row).copied())
    }

    /// Keep `top` such that the last page of matches fills the body: without
    /// this the list can scroll past the end and show a run of blank rows
    /// (which reads as "an empty entry appeared at the bottom").
    fn clamp_top(&self, top: usize) -> usize {
        let max_rows = (*self.max_rows_on_screen.borrow()).max(1);
        let len = self.match_count();
        top.min(len.saturating_sub(max_rows))
    }

    /// Move the selection, keeping `top_row` in sync.  Every RefCell borrow is
    /// scoped to a single statement: holding one across a call into another
    /// method that borrows the same cell used to panic with BorrowMutError.
    fn select(&self, row: usize) {
        let limit = self.match_count().saturating_sub(1);
        let row = row.min(limit);
        let max_rows = (*self.max_rows_on_screen.borrow()).max(1);
        let prior = *self.selected_row.borrow();
        if prior == row {
            return;
        }
        *self.selected_row.borrow_mut() = row;
        *self.preview_scroll.borrow_mut() = 0;
        *self.preview_selection.borrow_mut() = None;
        let mut top = *self.top_row.borrow();
        if row < top {
            top = row;
        } else if row >= top.saturating_add(max_rows) {
            top = row.saturating_add(1).saturating_sub(max_rows);
        }
        *self.top_row.borrow_mut() = self.clamp_top(top);
        self.element.borrow_mut().take();
    }

    fn move_up(&self) {
        let row = *self.selected_row.borrow();
        self.select(row.saturating_sub(1));
    }

    fn move_down(&self) {
        let row = *self.selected_row.borrow();
        self.select(row.saturating_add(1));
    }

    fn page(&self, up: bool) {
        let page = (*self.max_rows_on_screen.borrow()).max(1);
        for _ in 0..page {
            if up {
                self.move_up();
            } else {
                self.move_down();
            }
        }
    }

    fn scroll_preview(&self, delta: isize) {
        let prior = *self.preview_scroll.borrow() as isize;
        *self.preview_scroll.borrow_mut() = (prior + delta).max(0) as usize;
        // The clamp against the end of the preview happens in
        // `computed_element`, which knows how many lines there are.
        self.element.borrow_mut().take();
    }

    /// How many rows of the preview are currently rendered, and how many
    /// lines the preview has in total.
    ///
    /// Used to stop scrolling from pushing all of the text out of view.
    fn clamp_preview_scroll(&self, total_lines: usize, body_rows: usize, keep: usize) {
        let max_scroll = total_lines.saturating_sub(body_rows.saturating_sub(keep).max(1));
        let mut scroll = self.preview_scroll.borrow_mut();
        if *scroll > max_scroll {
            *scroll = max_scroll;
        }
    }

    /// Re-run the filter and put the cursor on item `idx`.
    fn refilter_and_select(&self, idx: usize) {
        let query = self.selection.borrow().clone();
        let list = self.source.filter(&query);
        let row = list.iter().position(|&i| i == idx);
        let generation = self.source.generation();
        *self.matches.borrow_mut() = Some(MatchResults {
            selection: query,
            generation,
            matches: list,
        });
        if let Some(row) = row {
            *self.selected_row.borrow_mut() = row;
            let max_rows = (*self.max_rows_on_screen.borrow()).max(1);
            let top = self.clamp_top(row.saturating_sub(max_rows / 2));
            *self.top_row.borrow_mut() = top;
        }
        self.element.borrow_mut().take();
    }

    /// Recompute the layout parameters (also used by the mouse handler).
    ///
    /// The cell count comes from the window's pixel size and the terminal's
    /// cell size, not from `terminal_size`: that field tracks the mux's idea
    /// of the size, which can lag behind the window.
    fn layout(
        term_window: &TermWindow,
        cell_w: f32,
        cell_h: f32,
        footer_visible: bool,
    ) -> PanelRect {
        let cfg = &term_window.config.snippet_panel;
        let padding_left = term_window.padding_left_top().0;
        let cols = (((term_window.dimensions.pixel_width as f32 - padding_left) / cell_w)
            .floor()
            .max(1.)) as usize;
        let rows = ((term_window.dimensions.pixel_height as f32 / cell_h)
            .floor()
            .max(1.)) as usize;

        // Leave room for the border and one cell of padding on each side.
        let avail_cols = cols.saturating_sub(4).max(1);
        let panel_cols = ((cols as f32 * cfg.width).round() as usize)
            .max(40.min(avail_cols))
            .min(avail_cols);
        let panel_rows = ((rows as f32 * cfg.height).round() as usize)
            .max(10.min(rows))
            .min(rows);

        // Inner width: the border and horizontal padding take 2 cells total.
        let inner_cols = panel_cols.saturating_sub(2);
        let list_cols = ((inner_cols as f32 * cfg.list_ratio).round() as usize)
            .max(16.min(inner_cols))
            .min(inner_cols.saturating_sub(1));
        // The vertical separator is one cell wide, and the preview starts
        // immediately after it, so both columns have the same left margin.
        let preview_cols = inner_cols.saturating_sub(list_cols + 1);

        // rows: input, rule, body..., rule, one footer row.  The help overlay
        // shares that row and is truncated rather than stealing a second one.
        // With the footer hidden both its row and the rule above it go away,
        // so the body gains two rows and the panel keeps its size (a panel
        // that changes height as you press Ctrl+H looks like a glitch).
        let chrome = if footer_visible { 5 } else { 3 };
        let body_rows = panel_rows.saturating_sub(chrome).max(1);
        PanelRect {
            x: 0,
            y: 0,
            cols: panel_cols,
            rows: panel_rows,
            body_top: 2,
            body_rows,
            list_cols,
            preview_cols,
        }
    }

    /// Apply one key to a single-line text field with a caret.
    ///
    /// Shared by the search box and every prompt so both behave the same
    /// (caret movement, word delete, IME input).
    fn edit_line(key: KeyCode, mods: KeyModifiers, text: &mut String, cursor: &mut usize) -> bool {
        let mut chars: Vec<char> = text.chars().collect();
        *cursor = (*cursor).min(chars.len());
        match (key, mods) {
            (KeyCode::LeftArrow, KeyModifiers::NONE) => *cursor = cursor.saturating_sub(1),
            (KeyCode::RightArrow, KeyModifiers::NONE) => *cursor = (*cursor + 1).min(chars.len()),
            (KeyCode::Home, KeyModifiers::NONE) | (KeyCode::Char('a'), KeyModifiers::CTRL) => {
                *cursor = 0
            }
            (KeyCode::End, KeyModifiers::NONE) | (KeyCode::Char('e'), KeyModifiers::CTRL) => {
                *cursor = chars.len()
            }
            (KeyCode::Backspace, KeyModifiers::NONE) => {
                if *cursor > 0 {
                    chars.remove(*cursor - 1);
                    *cursor -= 1;
                }
            }
            (KeyCode::Delete, KeyModifiers::NONE) => {
                if *cursor < chars.len() {
                    chars.remove(*cursor);
                }
            }
            (KeyCode::Char('w'), KeyModifiers::CTRL) => {
                let tail: Vec<char> = chars[*cursor..].to_vec();
                let mut before: String = chars[..*cursor].iter().collect();
                delete_last_word(&mut before);
                let mut next: Vec<char> = before.chars().collect();
                *cursor = next.len();
                next.extend(tail);
                chars = next;
            }
            (KeyCode::Char('u'), KeyModifiers::CTRL) => {
                chars.drain(..*cursor);
                *cursor = 0;
            }
            (KeyCode::Char(c), KeyModifiers::NONE) | (KeyCode::Char(c), KeyModifiers::SHIFT) => {
                chars.insert(*cursor, c);
                *cursor += 1;
            }
            _ => return false,
        }
        *text = chars.into_iter().collect();
        true
    }

    /// What the footer should show right now, or `None` when it is hidden.
    fn footer_text(&self, source: &dyn ListPreviewSource) -> Option<String> {
        if let Some((message, until)) = &*self.transient.borrow() {
            if std::time::Instant::now() < *until {
                // A hint is worth a row even when the user hid the hints.
                return Some(message.clone());
            }
        }
        match *self.modal_footer.borrow() {
            FooterMode::Hidden => None,
            FooterMode::Short => Some(source.footer()),
            FooterMode::Full => Some(source.help_text().to_string()),
        }
    }

    fn color_or(over: Option<config::RgbaColor>, fallback: SrgbaTuple) -> LinearRgba {
        match over {
            Some(c) => c.to_linear(),
            None => fallback.to_linear(),
        }
    }

    /// The free-text tokens of a query (drops `#tag`, `@dir`, `kind:`, `!`).
    fn free_text(query: &str) -> String {
        query
            .split_whitespace()
            .filter(|t| {
                !t.starts_with('#')
                    && !t.starts_with('@')
                    && !t.starts_with('/')
                    && !t.starts_with("kind:")
                    && *t != "!"
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Re-split an already-built preview line so matched characters are
    /// highlighted.  Used for the body, where the search term may appear
    /// anywhere.
    fn highlight_line(line: &PreviewLine, query: &str) -> PreviewLine {
        if query.is_empty() {
            return line.clone();
        }
        let text: String = line.iter().map(|s| s.text.clone()).collect();
        let pattern = matcher_pattern(query);
        let Some((_, indices)) = matcher_indices(&pattern, &text) else {
            return line.clone();
        };
        let hit: Vec<bool> = {
            let mut v = vec![false; text.chars().count()];
            for i in indices {
                if i < v.len() {
                    v[i] = true;
                }
            }
            v
        };

        // Walk the original spans so their dim/highlight styling survives, and
        // split at hit boundaries.
        let mut out = vec![];
        let mut idx = 0usize;
        for span in line {
            let mut cur = String::new();
            let mut cur_hit = hit.get(idx).copied().unwrap_or(false);
            for c in span.text.chars() {
                let is_hit = hit.get(idx).copied().unwrap_or(false);
                if is_hit != cur_hit && !cur.is_empty() {
                    out.push(if cur_hit {
                        Span::highlight(std::mem::take(&mut cur))
                    } else {
                        Span {
                            text: std::mem::take(&mut cur),
                            dim: span.dim,
                            highlight: false,
                        }
                    });
                }
                cur_hit = is_hit;
                cur.push(c);
                idx += 1;
            }
            if !cur.is_empty() {
                out.push(if cur_hit {
                    Span::highlight(cur)
                } else {
                    Span {
                        text: cur,
                        dim: span.dim,
                        highlight: false,
                    }
                });
            }
        }
        out
    }

    /// Split `title` into spans so that characters matched by the free-text
    /// part of `query` are highlighted.
    fn highlight_spans(title: &str, query: &str) -> Vec<Span> {
        if query.is_empty() {
            return vec![Span::normal(title)];
        }
        let free: Vec<&str> = query
            .split_whitespace()
            .filter(|t| {
                !t.starts_with('#')
                    && !t.starts_with('@')
                    && !t.starts_with('/')
                    && !t.starts_with("kind:")
                    && *t != "!"
            })
            .collect();
        if free.is_empty() {
            return vec![Span::normal(title)];
        }
        let pattern = matcher_pattern(&free.join(" "));
        let mut hit = vec![false; title.chars().count()];
        if let Some((_, indices)) = matcher_indices(&pattern, title) {
            for idx in indices {
                if idx < hit.len() {
                    hit[idx] = true;
                }
            }
        }

        let mut spans: Vec<Span> = vec![];
        let mut cur = String::new();
        let mut cur_hit = false;
        for (i, c) in title.chars().enumerate() {
            if hit[i] != cur_hit && !cur.is_empty() {
                spans.push(if cur_hit {
                    Span::highlight(std::mem::take(&mut cur))
                } else {
                    Span::normal(std::mem::take(&mut cur))
                });
            }
            cur_hit = hit[i];
            cur.push(c);
        }
        if !cur.is_empty() {
            spans.push(if cur_hit {
                Span::highlight(cur)
            } else {
                Span::normal(cur)
            });
        }
        if spans.is_empty() {
            spans.push(Span::normal(title));
        }
        spans
    }

    /// Text of the drag-selection, ready for the clipboard.
    fn selected_text(&self) -> Option<String> {
        let selection = (*self.preview_selection.borrow())?;
        let raw = self.preview_raw.borrow();
        let ((sr, sc), (er, ec)) = selection.ordered();
        let mut lines = vec![];
        for row in sr..=er.min(raw.len().saturating_sub(1)) {
            let line = &raw[row];
            let (from, to) = if sr == er {
                (sc, ec)
            } else if row == sr {
                (sc, usize::MAX)
            } else if row == er {
                (0, ec)
            } else {
                (0, usize::MAX)
            };
            let piece = slice_columns(line.trim_end(), from, to);
            lines.push(piece.trim_end().to_string());
        }
        let text = lines.join("\n");
        if text.is_empty() {
            None
        } else {
            Some(text)
        }
    }

    fn copy_selection(&self, term_window: &mut TermWindow, mouse_released: bool) {
        let text_opt = self.selected_text();
        if let Some(text) = text_opt {
            let dest = if mouse_released {
                config::keyassignment::ClipboardCopyDestination::ClipboardAndPrimarySelection
            } else {
                config::keyassignment::ClipboardCopyDestination::PrimarySelection
            };
            term_window.copy_to_clipboard(dest, text);
        }
    }
}

impl Modal for ListPreviewModal {
    fn composed_text(&self, text_input: &str, term_window: &mut TermWindow) -> bool {
        let mode = self.mode.borrow().clone();
        if let InputMode::Prompt {
            prompt,
            text: current_text,
            mut cursor,
            target,
        } = mode
        {
            let mut chars: Vec<char> = current_text.chars().collect();
            cursor = cursor.min(chars.len());
            for c in text_input.chars() {
                chars.insert(cursor, c);
                cursor += 1;
            }
            let current: String = chars.into_iter().collect();
            *self.mode.borrow_mut() = InputMode::Prompt {
                prompt,
                text: current,
                cursor,
                target,
            };
            self.element.borrow_mut().take();
            term_window.invalidate_modal();
            return true;
        }
        // Browse mode: the search box is a text field too.
        let mut text = self.selection.borrow().clone();
        let mut cursor = *self.selection_cursor.borrow();
        let mut chars: Vec<char> = text.chars().collect();
        cursor = cursor.min(chars.len());
        for c in text_input.chars() {
            chars.insert(cursor, c);
            cursor += 1;
        }
        text = chars.into_iter().collect();
        *self.selection.borrow_mut() = text;
        *self.selection_cursor.borrow_mut() = cursor;
        self.updated_input();
        term_window.invalidate_modal();
        // The panel owns input: never leak composed text to the pane behind it.
        true
    }

    fn perform_assignment(
        &self,
        _assignment: &config::keyassignment::KeyAssignment,
        _term_window: &mut TermWindow,
    ) -> bool {
        false
    }

    fn mouse_event(&self, event: MouseEvent, term_window: &mut TermWindow) -> anyhow::Result<()> {
        let rect = match *self.rect.borrow() {
            Some(rect) => rect,
            None => return Ok(()),
        };

        let inside = event.x >= rect.x
            && event.x < rect.x + rect.cols
            && event.y >= rect.y as i64
            && event.y < (rect.y + rect.rows) as i64;

        if !inside {
            // A click outside the panel only dismisses a preview selection; it
            // does not close the panel.  Closing is Esc / Ctrl+Alt+O / Enter,
            // so a stray click cannot lose the current search.
            if matches!(event.kind, MouseEventKind::Press) && event.button == MouseButton::Left {
                if self.preview_selection.borrow_mut().take().is_some() {
                    self.element.borrow_mut().take();
                    term_window.invalidate_modal();
                }
            }
            return Ok(());
        }

        // Panel-local coordinates.  One cell of border/padding around the
        // content, so shift by 1 before comparing against body rows.
        let local_y = (event.y - rect.y as i64).max(0) as usize;
        let local_x = (event.x - rect.x).max(1) - 1;
        let body_index = local_y.checked_sub(rect.body_top);
        let in_list = local_x < rect.list_cols;
        // list column, one separator cell, then preview
        let preview_col = local_x.saturating_sub(rect.list_cols + 1);

        match event.kind {
            MouseEventKind::Press => match event.button {
                MouseButton::WheelUp(_) => {
                    if in_list {
                        self.move_up();
                    } else {
                        self.scroll_preview(-1);
                    }
                }
                MouseButton::WheelDown(_) => {
                    if in_list {
                        self.move_down();
                    } else {
                        self.scroll_preview(1);
                    }
                }
                MouseButton::Right => {
                    if self.preview_selection.borrow().is_some() {
                        self.copy_selection(term_window, true);
                        *self.preview_selection.borrow_mut() = None;
                        *self.transient.borrow_mut() = Some((
                            "copied to clipboard".to_string(),
                            std::time::Instant::now() + TRANSIENT_DURATION,
                        ));
                        self.element.borrow_mut().take();
                        term_window.invalidate_modal();
                    }
                }
                MouseButton::Left => {
                    let row = match body_index {
                        Some(row) if row < rect.body_rows => row,
                        _ => return Ok(()),
                    };
                    if in_list {
                        // Track double clicks on the list to insert.
                        let position = ClickPosition {
                            column: event.x,
                            row: event.y,
                            x_pixel_offset: event.x_pixel_offset,
                            y_pixel_offset: event.y_pixel_offset,
                        };
                        let streak = {
                            let now = std::time::Instant::now();
                            let prior = self.last_click.borrow().clone();
                            let streak = match prior {
                                Some((button, pos, at, streak))
                                    if button == MouseButton::Left
                                        && pos.column == position.column
                                        && pos.row == position.row
                                        && now.duration_since(at)
                                            <= std::time::Duration::from_millis(500) =>
                                {
                                    streak + 1
                                }
                                _ => 1,
                            };
                            *self.last_click.borrow_mut() =
                                Some((MouseButton::Left, position, now, streak));
                            streak
                        };

                        let top = *self.top_row.borrow();
                        self.select(top.saturating_add(row));

                        if streak >= 2 {
                            if let Some(idx) = self.selected_index() {
                                let mut ctx = self.accept_ctx(term_window);
                                self.source.accept(&mut ctx, idx, false);
                                flush_messages(&mut ctx);
                            }
                            term_window.cancel_modal();
                            return Ok(());
                        }
                    } else {
                        // Only start a selection where there is actually text:
                        // dragging over blank space would copy nothing.
                        let has_text = self
                            .preview_raw
                            .borrow()
                            .get(row)
                            .map(|line| {
                                let trimmed = line.trim_end();
                                preview_col < display_width(trimmed)
                            })
                            .unwrap_or(false);
                        if has_text {
                            *self.preview_selection.borrow_mut() = Some(PreviewSelection {
                                anchor: (row, preview_col),
                                cursor: (row, preview_col),
                            });
                            self.element.borrow_mut().take();
                        } else {
                            *self.preview_selection.borrow_mut() = None;
                            self.element.borrow_mut().take();
                        }
                    }
                }
                _ => {}
            },
            MouseEventKind::Move => {
                // A drag is "a selection was started and the button has not
                // been released yet".  X11 move events do not always carry the
                // held button, so do not require it here.
                let existing = *self.preview_selection.borrow();
                if let Some(mut selection) = existing {
                    let row = body_index
                        .unwrap_or(0)
                        .min(rect.body_rows.saturating_sub(1));
                    selection.cursor = (row, preview_col);
                    *self.preview_selection.borrow_mut() = Some(selection);
                    self.element.borrow_mut().take();
                }
            }
            MouseEventKind::Release => {
                self.last_click.borrow_mut().take();
                // Dragging leaves the selection in place; a right click copies
                // it and clears it.  A click without dragging clears it.
                let selection = *self.preview_selection.borrow();
                if let Some(selection) = selection {
                    let ((sr, sc), (er, ec)) = selection.ordered();
                    if (sr, sc) == (er, ec) {
                        *self.preview_selection.borrow_mut() = None;
                    }
                    self.element.borrow_mut().take();
                }
            }
        }

        term_window.invalidate_modal();
        Ok(())
    }

    fn key_down(
        &self,
        key: KeyCode,
        mods: KeyModifiers,
        term_window: &mut TermWindow,
    ) -> anyhow::Result<bool> {
        // Any key dismisses the hint from the previous action.
        self.transient.borrow_mut().take();

        // The same key that opens the panel closes it again.
        const CTRL_ALT: KeyModifiers = KeyModifiers::CTRL.union(KeyModifiers::ALT);
        if mods == CTRL_ALT && key == KeyCode::Char('o') {
            term_window.cancel_modal();
            return Ok(true);
        }

        // Sub-modes consume most keys first.
        let mode = self.mode.borrow().clone();
        match mode {
            InputMode::Prompt {
                prompt,
                text,
                mut cursor,
                target,
            } => {
                let mut chars: Vec<char> = text.chars().collect();
                cursor = cursor.min(chars.len());
                match (key, mods) {
                    (KeyCode::Escape, KeyModifiers::NONE) => {
                        *self.mode.borrow_mut() = InputMode::Browse;
                        self.element.borrow_mut().take();
                        term_window.invalidate_modal();
                        return Ok(true);
                    }
                    (KeyCode::Enter, KeyModifiers::NONE) => {
                        *self.mode.borrow_mut() = InputMode::Browse;
                        // Prompts carry their own target so a click on another
                        // row cannot redirect the edit.
                        let idx = target.unwrap_or(usize::MAX);
                        let mut ctx = self.accept_ctx(term_window);
                        self.source.accept_prompt(&mut ctx, idx, &text);
                        if let Some(mode) = ctx.mode_request.take() {
                            *self.mode.borrow_mut() = mode;
                        }
                        let close = ctx.close_requested;
                        let select = ctx.select_request.take();
                        let inline = ctx.inline_message.take();
                        flush_messages(&mut ctx);
                        if let Some(message) = inline {
                            *self.transient.borrow_mut() =
                                Some((message, std::time::Instant::now() + TRANSIENT_DURATION));
                        } else {
                            self.transient.borrow_mut().take();
                        }
                        self.element.borrow_mut().take();
                        if let Some(idx) = select {
                            self.refilter_and_select(idx);
                        }
                        if close {
                            term_window.cancel_modal();
                            return Ok(true);
                        }
                        term_window.invalidate_modal();
                        return Ok(true);
                    }
                    (KeyCode::LeftArrow, KeyModifiers::NONE) => {
                        cursor = cursor.saturating_sub(1);
                    }
                    (KeyCode::RightArrow, KeyModifiers::NONE) => {
                        cursor = (cursor + 1).min(chars.len());
                    }
                    (KeyCode::Home, KeyModifiers::NONE) => cursor = 0,
                    (KeyCode::End, KeyModifiers::NONE) => cursor = chars.len(),
                    (KeyCode::Backspace, KeyModifiers::NONE) => {
                        if cursor > 0 {
                            chars.remove(cursor - 1);
                            cursor -= 1;
                        }
                    }
                    (KeyCode::Delete, KeyModifiers::NONE) => {
                        if cursor < chars.len() {
                            chars.remove(cursor);
                        }
                    }
                    (KeyCode::Char('a'), KeyModifiers::CTRL) => cursor = 0,
                    (KeyCode::Char('e'), KeyModifiers::CTRL) => cursor = chars.len(),
                    (KeyCode::Char('w'), KeyModifiers::CTRL) => {
                        let mut before: String = chars[..cursor].iter().collect();
                        delete_last_word(&mut before);
                        let before_len = before.chars().count();
                        let mut next: Vec<char> = before.chars().collect();
                        next.extend(chars[cursor..].iter().copied());
                        cursor = before_len;
                        chars = next;
                    }
                    (KeyCode::Char('u'), KeyModifiers::CTRL) => {
                        chars.drain(..cursor);
                        cursor = 0;
                    }
                    (KeyCode::Char(c), KeyModifiers::NONE)
                    | (KeyCode::Char(c), KeyModifiers::SHIFT) => {
                        chars.insert(cursor, c);
                        cursor += 1;
                    }
                    _ => return Ok(true),
                }
                let text: String = chars.into_iter().collect();
                *self.mode.borrow_mut() = InputMode::Prompt {
                    prompt,
                    text,
                    cursor,
                    target,
                };
                self.element.borrow_mut().take();
                term_window.invalidate_modal();
                return Ok(true);
            }
            InputMode::Confirm { prompt } => {
                let confirmed = matches!(
                    (key, mods),
                    (KeyCode::Char('y'), KeyModifiers::NONE)
                        | (KeyCode::Char('Y'), KeyModifiers::SHIFT)
                        | (KeyCode::Enter, KeyModifiers::NONE)
                );
                *self.mode.borrow_mut() = InputMode::Browse;
                let _ = prompt;
                if confirmed {
                    if let Some(idx) = self.selected_index() {
                        let mut ctx = self.accept_ctx(term_window);
                        self.source.confirm(&mut ctx, idx);
                        flush_messages(&mut ctx);
                    }
                }
                self.element.borrow_mut().take();
                term_window.invalidate_modal();
                return Ok(true);
            }
            InputMode::Browse => {}
        }

        match (key, mods) {
            (KeyCode::Escape, KeyModifiers::NONE) | (KeyCode::Char('g'), KeyModifiers::CTRL) => {
                // First Esc clears a preview selection, second closes.
                if self.preview_selection.borrow().is_some() {
                    *self.preview_selection.borrow_mut() = None;
                    self.element.borrow_mut().take();
                    term_window.invalidate_modal();
                    return Ok(true);
                }
                term_window.cancel_modal();
                return Ok(true);
            }
            (KeyCode::UpArrow, KeyModifiers::NONE) => {
                *self.preview_selection.borrow_mut() = None;
                self.move_up()
            }
            (KeyCode::DownArrow, KeyModifiers::NONE) => {
                *self.preview_selection.borrow_mut() = None;
                self.move_down()
            }
            (KeyCode::PageUp, KeyModifiers::NONE) => self.page(true),
            (KeyCode::PageDown, KeyModifiers::NONE) => self.page(false),
            // Home/End belong to the search field here (fzf behaves the same
            // way), as does Ctrl+A.  Ctrl+E is taken by "open in $EDITOR"
            // below, so use End to jump to the end of the query.
            // Soft-wrap toggle moved off Ctrl+W, which now deletes a word.
            (KeyCode::Char('w'), KeyModifiers::ALT) => {
                let wrap = *self.wrap.borrow();
                *self.wrap.borrow_mut() = !wrap;
                self.element.borrow_mut().take();
            }
            (KeyCode::Char('k'), KeyModifiers::CTRL) => self.scroll_preview(-1),
            (KeyCode::Char('j'), KeyModifiers::CTRL) => self.scroll_preview(1),
            (KeyCode::Char('r'), KeyModifiers::CTRL) => {
                self.source.rescan();
                self.updated_input();
            }
            // Ctrl+H: cycle the key-hint footer Hidden -> short -> full.
            // Function keys are deliberately avoided: F1/F3/F4 are grabbed by
            // desktop environments and other applications.
            (KeyCode::Char('h'), KeyModifiers::CTRL) => {
                let mode = *self.modal_footer.borrow();
                *self.modal_footer.borrow_mut() = mode.next();
                self.element.borrow_mut().take();
            }
            // Ctrl+I: hide/show the preview's metadata header.
            (KeyCode::Char('i'), KeyModifiers::CTRL) => {
                let meta = *self.show_meta.borrow();
                *self.show_meta.borrow_mut() = !meta;
                self.element.borrow_mut().take();
            }
            (KeyCode::Char('c'), KeyModifiers::CTRL) if self.selected_text().is_some() => {
                self.copy_selection(term_window, true);
            }
            (KeyCode::Enter, KeyModifiers::NONE) | (KeyCode::Enter, KeyModifiers::CTRL) => {
                let exec = mods.contains(KeyModifiers::CTRL);
                if let Some(idx) = self.selected_index() {
                    let mut ctx = self.accept_ctx(term_window);
                    self.source.accept(&mut ctx, idx, exec);
                    flush_messages(&mut ctx);
                }
                term_window.cancel_modal();
                return Ok(true);
            }
            _ => {
                // Source-specific keys first (Ctrl+N/D/E/T ...), then the
                // shared single-line editor: printable characters, arrows,
                // Home/End, Backspace/Delete, Ctrl+W and Ctrl+U go to the
                // search field.  This fallback must stay *last*, otherwise it
                // swallows every key bound above it.
                let idx = self.selected_index();
                let mut ctx = self.accept_ctx(term_window);
                let handled = self.source.custom_key(&mut ctx, idx, key, mods);
                if let Some(mode) = ctx.mode_request.take() {
                    *self.mode.borrow_mut() = mode;
                }
                let close = ctx.close_requested;
                let select = ctx.select_request.take();
                let inline = ctx.inline_message.take();
                flush_messages(&mut ctx);
                if let Some(message) = inline {
                    *self.transient.borrow_mut() =
                        Some((message, std::time::Instant::now() + TRANSIENT_DURATION));
                } else {
                    self.transient.borrow_mut().take();
                }
                if !handled {
                    // Not a source command: type it into the query instead.
                    let mut text = self.selection.borrow().clone();
                    let mut cursor = *self.selection_cursor.borrow();
                    if !Self::edit_line(key, mods, &mut text, &mut cursor) {
                        return Ok(false);
                    }
                    let changed = text != *self.selection.borrow();
                    *self.selection.borrow_mut() = text;
                    *self.selection_cursor.borrow_mut() = cursor;
                    if changed {
                        self.updated_input();
                    }
                    self.element.borrow_mut().take();
                    term_window.invalidate_modal();
                    return Ok(true);
                }
                if let Some(idx) = select {
                    self.refilter_and_select(idx);
                }
                self.element.borrow_mut().take();
                if close {
                    term_window.cancel_modal();
                    return Ok(true);
                }
            }
        }

        term_window.invalidate_modal();
        Ok(true)
    }

    fn computed_element(
        &self,
        term_window: &mut TermWindow,
    ) -> anyhow::Result<Ref<'_, [ComputedElement]>> {
        let selection = self.selection.borrow().clone();
        let generation = self.source.generation();

        {
            let mut results = self.matches.borrow_mut();
            let stale = results
                .as_ref()
                .map(|m| m.selection != selection || m.generation != generation)
                .unwrap_or(true);
            if stale {
                let matches = self.source.filter(&selection);
                let len = matches.len();
                *results = Some(MatchResults {
                    selection: selection.clone(),
                    generation,
                    matches,
                });
                let selected = *self.selected_row.borrow();
                let clamped = if len == 0 { 0 } else { selected.min(len - 1) };
                *self.selected_row.borrow_mut() = clamped;
            }
        }

        // Footer visibility affects how many body rows fit, so resolve it
        // before laying out.  A hint can appear or expire while the panel sits
        // idle, so the cached element is discarded whenever that changes.
        let footer_text = self.footer_text(&*self.source);
        if footer_text.is_some() != *self.element_has_footer.borrow() {
            *self.element_has_footer.borrow_mut() = footer_text.is_some();
            self.element.borrow_mut().take();
        }
        if let Some((_, until)) = &*self.transient.borrow() {
            // Repaint when the hint expires, otherwise the footer would stay
            // on screen until the next keypress.
            if let Some(window) = term_window.window.clone() {
                let until = *until;
                promise::spawn::spawn(async move {
                    smol::Timer::at(until).await;
                    window.invalidate();
                })
                .detach();
            }
        }

        let metrics = term_window.render_metrics;
        let rect = Self::layout(
            term_window,
            metrics.cell_size.width as f32,
            metrics.cell_size.height as f32,
            footer_text.is_some(),
        );
        *self.max_rows_on_screen.borrow_mut() = rect.body_rows;
        // Keep the window from scrolling past the last match: a run of blank
        // rows below the list looks like a phantom empty entry.
        {
            let clamped = self.clamp_top(*self.top_row.borrow());
            if clamped != *self.top_row.borrow() {
                *self.top_row.borrow_mut() = clamped;
                self.element.borrow_mut().take();
            }
        }
        let top_row = *self.top_row.borrow();
        let selected_row = *self.selected_row.borrow();
        let wrap = *self.wrap.borrow();
        let show_meta = *self.show_meta.borrow();
        let matched = self.match_count();
        let mode = self.mode.borrow().clone();

        // ---- list rows
        let mut list_rows: Vec<Vec<Span>> = Vec::with_capacity(rect.body_rows);
        for display_idx in 0..rect.body_rows {
            let row_idx = top_row.saturating_add(display_idx);
            let idx = self
                .matches
                .borrow()
                .as_ref()
                .and_then(|m| m.matches.get(row_idx).copied());
            let mut spans = vec![];
            match idx {
                Some(idx) => {
                    let row = self.source.row(idx);
                    let gutter = if row.gutter.is_empty() {
                        " ".to_string()
                    } else {
                        row.gutter.chars().next().unwrap().to_string()
                    };
                    // Gutter is two cells: the marker (pin/danger) and the
                    // selection bar.  They are separate columns so the bar
                    // never hides the marker.
                    spans.push(Span::normal(pad_to_width(&gutter, 1)));
                    if display_idx == selected_row {
                        spans.push(Span::normal("\u{258c}"));
                    } else {
                        spans.push(Span::normal(" "));
                    }

                    let badge_width_limit = (rect.list_cols * 2 / 5).max(3);
                    let badges = truncate_to_width(&row.badges, badge_width_limit);
                    let badge_w = display_width(&badges);
                    let title_width = rect.list_cols.saturating_sub(3 + badge_w + 1);
                    let title = truncate_to_width(&row.title, title_width);
                    let title_w = display_width(&title);

                    let mut title_spans = Self::highlight_spans(&title, &selection);
                    let pad = title_width.saturating_sub(title_w);
                    if pad > 0 {
                        title_spans.push(Span::normal(" ".repeat(pad)));
                    }
                    spans.extend(title_spans);

                    if !badges.is_empty() {
                        spans.push(Span::normal(" "));
                        spans.push(Span::dim(badges));
                    }
                }
                None => spans.push(Span::normal(" ".repeat(rect.list_cols))),
            }

            let used: usize = spans.iter().map(|s| s.width()).sum();
            if used < rect.list_cols {
                spans.push(Span::normal(" ".repeat(rect.list_cols - used)));
            } else if used > rect.list_cols {
                // Never let a wide title push the separator: trim the tail.
                let mut budget = rect.list_cols;
                let mut trimmed: Vec<Span> = vec![];
                for span in spans.drain(..) {
                    let w = span.width();
                    if budget == 0 {
                        break;
                    }
                    if w <= budget {
                        budget -= w;
                        trimmed.push(span);
                    } else {
                        trimmed.push(Span {
                            text: truncate_to_width(&span.text, budget),
                            ..span
                        });
                        budget = 0;
                    }
                }
                spans = trimmed;
            }
            list_rows.push(spans);
        }

        // ---- preview rows
        let body_query = Self::free_text(&selection);
        let selected_idx = self
            .matches
            .borrow()
            .as_ref()
            .and_then(|m| m.matches.get(selected_row).copied());
        let draft_title = match &mode {
            InputMode::Prompt { text, .. } => text.as_str(),
            _ => "",
        };
        // Keep at least `keep_preview_rows` lines on screen: scrolling to the
        // very end must not blank the column out.
        let keep = term_window.config.snippet_panel.keep_preview_rows;
        let all_preview: Vec<PreviewLine> =
            match self
                .source
                .preview_override(rect.preview_cols, wrap, draft_title, show_meta)
            {
                Some(draft) => draft,
                None => match selected_idx {
                    Some(idx) => self
                        .source
                        .preview_lines(idx, rect.preview_cols, wrap, show_meta),
                    None => vec![vec![Span::dim("no matches")]],
                },
            };
        self.clamp_preview_scroll(all_preview.len(), rect.body_rows, keep);
        let preview_scroll = *self.preview_scroll.borrow();
        let preview: Vec<PreviewLine> = all_preview
            .into_iter()
            .skip(preview_scroll)
            .take(rect.body_rows)
            .map(|line| Self::highlight_line(&line, &body_query))
            .collect();

        let mut preview_rows: Vec<Vec<Span>> = Vec::with_capacity(rect.body_rows);
        let mut preview_raw: Vec<String> = Vec::with_capacity(rect.body_rows);
        for display_idx in 0..rect.body_rows {
            let mut spans = preview.get(display_idx).cloned().unwrap_or_default();
            preview_raw.push(spans.iter().map(|s| s.text.clone()).collect());
            let used: usize = spans.iter().map(|s| s.width()).sum();
            if used < rect.preview_cols {
                spans.push(Span::normal(" ".repeat(rect.preview_cols - used)));
            }
            preview_rows.push(spans);
        }
        *self.preview_raw.borrow_mut() = preview_raw;

        if self.element.borrow().is_none() {
            let (elements, computed_rect) = Self::compute(
                term_window,
                &*self.source,
                &selection,
                &mode,
                matched,
                &list_rows,
                &preview_rows,
                selected_row,
                rect,
                self.preview_selection.borrow().as_ref(),
                footer_text.as_deref(),
                *self.selection_cursor.borrow(),
            )?;
            *self.rect.borrow_mut() = Some(computed_rect);
            self.element.borrow_mut().replace(elements);
        }

        Ok(Ref::map(self.element.borrow(), |v| {
            v.as_ref().unwrap().as_slice()
        }))
    }

    fn reconfigure(&self, _term_window: &mut TermWindow) {
        self.element.borrow_mut().take();
    }
}

fn cfg_footer_fg(term_window: &TermWindow) -> LinearRgba {
    term_window.config.snippet_panel.footer_fg.to_linear()
}

/// How long a footer hint stays on screen.
const TRANSIENT_DURATION: std::time::Duration = std::time::Duration::from_secs(6);

/// Remove the trailing word (and any trailing spaces) from `text`.
fn delete_last_word(text: &mut String) {
    while text.ends_with(char::is_whitespace) {
        text.pop();
    }
    while let Some(c) = text.chars().last() {
        if c.is_whitespace() {
            break;
        }
        text.pop();
    }
}

/// Colors used to draw one frame of the panel.
struct PanelColors {
    fg: LinearRgba,
    dim_fg: LinearRgba,
    match_fg: LinearRgba,
    sel_fg: LinearRgba,
    sel_bg: LinearRgba,
    bg: LinearRgba,
    border: LinearRgba,
    sep_fg: LinearRgba,
}

impl ListPreviewModal {
    fn accept_ctx<'a>(&'a self, term_window: &'a mut TermWindow) -> AcceptCtx<'a> {
        let pane = self.pane_id.and_then(|id| mux::Mux::get().get_pane(id));
        let placeholder = term_window.config.snippet_panel.placeholder;
        AcceptCtx {
            term_window,
            pane,
            placeholder,
            messages: vec![],
            urgent_messages: vec![],
            mode_request: None,
            close_requested: false,
            select_request: None,
            inline_message: None,
        }
    }
}

impl ListPreviewModal {
    #[allow(clippy::too_many_arguments)]
    fn compute(
        term_window: &mut TermWindow,
        source: &dyn ListPreviewSource,
        query: &str,
        mode: &InputMode,
        matched: usize,
        list_rows: &[Vec<Span>],
        preview_rows: &[Vec<Span>],
        selected_row: usize,
        rect: PanelRect,
        preview_selection: Option<&PreviewSelection>,
        footer_text: Option<&str>,
        search_cursor: usize,
    ) -> anyhow::Result<(Vec<ComputedElement>, PanelRect)> {
        let footer_fg = cfg_footer_fg(term_window);
        let font = term_window
            .fonts
            .default_font()
            .expect("to resolve the default font");
        let metrics = term_window.render_metrics;
        let cell_w = metrics.cell_size.width as f32;
        let cell_h = metrics.cell_size.height as f32;

        let palette_colors = term_window.palette().clone();
        let cfg = &term_window.config.snippet_panel;

        let bg = cfg.bg.to_linear();
        // When the user hasn't chosen a foreground, use the terminal's, but
        // flip it to a light gray if it would be unreadable on the panel
        // background (light theme + default black panel).
        let mut fg = Self::color_or(cfg.fg, palette_colors.foreground);
        if cfg.fg.is_none() {
            let bg_luma = 0.2126 * bg.0 + 0.7152 * bg.1 + 0.0722 * bg.2;
            let fg_luma = 0.2126 * fg.0 + 0.7152 * fg.1 + 0.0722 * fg.2;
            if (bg_luma - fg_luma).abs() < 0.35 {
                fg = if bg_luma < 0.5 {
                    LinearRgba(0.80, 0.80, 0.80, 1.0)
                } else {
                    LinearRgba(0.15, 0.15, 0.15, 1.0)
                };
            }
        }
        let dim_fg = fg.mul_alpha(0.55);
        let colors = PanelColors {
            fg,
            dim_fg,
            match_fg: cfg.match_fg.to_linear(),
            sel_fg: cfg.selection_fg.to_linear(),
            sel_bg: cfg.selection_bg.to_linear(),
            bg,
            border: fg.mul_alpha(0.45),
            sep_fg: fg.mul_alpha(0.30),
        };

        let panel_cols = rect.cols;
        let inner_cols = panel_cols.saturating_sub(2);

        let mut elements = vec![];

        // ---- input row
        let counter = format!(" {matched}/{} ", source.total());
        let input_line = match mode {
            InputMode::Browse => format!("{} > {query}", source.source_name()),
            InputMode::Prompt { prompt, text, .. } => format!("{prompt}: {text}"),
            InputMode::Confirm { prompt } => prompt.clone(),
        };
        let input_left =
            truncate_to_width(&input_line, inner_cols.saturating_sub(counter.len() + 1));
        let (before_caret, caret_char, after_caret) = match mode {
            InputMode::Prompt {
                prompt,
                text,
                cursor,
                ..
            } => {
                let head: String = format!("{prompt}: ");
                let chars: Vec<char> = text.chars().collect();
                let cursor = (*cursor).min(chars.len());
                let before: String = chars[..cursor].iter().collect();
                let caret = chars.get(cursor).copied();
                let after: String = chars[cursor + caret.map(|_| 1).unwrap_or(0)..]
                    .iter()
                    .collect();
                (
                    truncate_to_width(&format!("{head}{before}"), inner_cols.saturating_sub(1)),
                    caret
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| " ".to_string()),
                    after,
                )
            }
            InputMode::Browse => {
                let head = format!("{} > ", source.source_name());
                let chars: Vec<char> = query.chars().collect();
                let cursor = search_cursor.min(chars.len());
                let before: String = chars[..cursor].iter().collect();
                let caret = chars.get(cursor).copied();
                let after: String = chars[cursor + caret.map(|_| 1).unwrap_or(0)..]
                    .iter()
                    .collect();
                (
                    truncate_to_width(&format!("{head}{before}"), inner_cols.saturating_sub(1)),
                    caret
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| " ".to_string()),
                    after,
                )
            }
            InputMode::Confirm { .. } => {
                let caret = input_left.chars().last();
                let before: String = {
                    let mut v: Vec<char> = input_left.chars().collect();
                    if !v.is_empty() {
                        v.pop();
                    }
                    v.into_iter().collect()
                };
                (
                    before,
                    caret
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| " ".to_string()),
                    String::new(),
                )
            }
        };
        elements.push(
            Element::new(
                &font,
                ElementContent::Children(vec![
                    Element::new(
                        &font,
                        ElementContent::Children(vec![
                            Element::new(&font, ElementContent::Text(before_caret)),
                            Element::new(&font, ElementContent::Text(caret_char)).colors(
                                ElementColors {
                                    border: BorderColor::default(),
                                    bg: colors.sel_bg.into(),
                                    text: colors.sel_fg.into(),
                                },
                            ),
                            Element::new(&font, ElementContent::Text(after_caret)),
                        ]),
                    )
                    .colors(ElementColors {
                        border: BorderColor::default(),
                        bg: LinearRgba::TRANSPARENT.into(),
                        text: colors.fg.into(),
                    }),
                    Element::new(&font, ElementContent::Text(counter))
                        .float(Float::Right)
                        .zindex(10)
                        .colors(ElementColors {
                            border: BorderColor::default(),
                            bg: LinearRgba::TRANSPARENT.into(),
                            text: colors.dim_fg.into(),
                        }),
                ]),
            )
            .display(DisplayType::Block)
            .min_width(Some(Dimension::Cells(inner_cols as f32))),
        );

        // ---- rule under the input line
        let rule = "\u{2500}".repeat(inner_cols);
        elements.push(
            Element::new(&font, ElementContent::Text(rule.clone()))
                .colors(ElementColors {
                    border: BorderColor::default(),
                    bg: LinearRgba::TRANSPARENT.into(),
                    text: colors.dim_fg.into(),
                })
                .display(DisplayType::Block),
        );

        // ---- body rows
        for display_idx in 0..rect.body_rows {
            let is_selected = display_idx == selected_row;
            let list_spans = list_rows.get(display_idx).cloned().unwrap_or_default();
            let preview_spans = preview_rows.get(display_idx).cloned().unwrap_or_default();

            let list_fg = if is_selected {
                colors.sel_fg
            } else {
                colors.fg
            };
            let list_bg = if is_selected {
                colors.sel_bg
            } else {
                colors.bg
            };

            // ---- assemble the row: [list column][separator][preview]
            // The selection bar must cover the list column only, so the list
            // text is its own element with its own background.
            let mut list_children = vec![];
            for span in &list_spans {
                let text = if span.dim { colors.dim_fg } else { list_fg };
                list_children.push(
                    Element::new(&font, ElementContent::Text(span.text.clone())).colors(
                        ElementColors {
                            border: BorderColor::default(),
                            bg: LinearRgba::TRANSPARENT.into(),
                            text: text.into(),
                        },
                    ),
                );
            }
            let list_element = Element::new(&font, ElementContent::Children(list_children))
                .colors(ElementColors {
                    border: BorderColor::default(),
                    bg: list_bg.into(),
                    text: list_fg.into(),
                })
                .min_width(Some(Dimension::Cells(rect.list_cols as f32)));

            let sep_element = Element::new(&font, ElementContent::Text("\u{2502}".to_string()))
                .colors(ElementColors {
                    border: BorderColor::default(),
                    bg: colors.bg.into(),
                    text: colors.sep_fg.into(),
                });

            // Split each preview span at the selection boundaries so the
            // highlight covers exactly the selected columns, not whole runs.
            let mut preview_children = vec![];
            let mut col = 0usize;
            for span in &preview_spans {
                let span_width = span.width();
                let selected_range = preview_selection.and_then(|selection| {
                    let ((sr, sc), (er, ec)) = selection.ordered();
                    let row = display_idx;
                    if row < sr || row > er {
                        return None;
                    }
                    let from = if row == sr { sc } else { 0 };
                    let to = if row == er { ec } else { usize::MAX };
                    let start = from.max(col);
                    let end = to.min(col + span_width);
                    if start < end {
                        Some((start - col, end - col))
                    } else {
                        None
                    }
                });
                match selected_range {
                    Some((a, b)) => {
                        let head = slice_columns(&span.text, 0, a);
                        let mid = slice_columns(&span.text, a, b);
                        let tail = slice_columns(&span.text, b, span_width);
                        let base_fg = if span.highlight {
                            colors.match_fg
                        } else if span.dim {
                            colors.dim_fg
                        } else {
                            colors.fg
                        };
                        for (text, bg, fg) in [
                            (head, None, base_fg),
                            (mid, Some(colors.sel_bg), colors.sel_fg),
                            (tail, None, base_fg),
                        ] {
                            if text.is_empty() {
                                continue;
                            }
                            preview_children.push(
                                Element::new(&font, ElementContent::Text(text)).colors(
                                    ElementColors {
                                        border: BorderColor::default(),
                                        bg: bg.unwrap_or(LinearRgba::TRANSPARENT).into(),
                                        text: fg.into(),
                                    },
                                ),
                            );
                        }
                    }
                    None => {
                        let this_fg = if span.highlight {
                            colors.match_fg
                        } else if span.dim {
                            colors.dim_fg
                        } else {
                            colors.fg
                        };
                        preview_children.push(
                            Element::new(&font, ElementContent::Text(span.text.clone())).colors(
                                ElementColors {
                                    border: BorderColor::default(),
                                    bg: LinearRgba::TRANSPARENT.into(),
                                    text: this_fg.into(),
                                },
                            ),
                        );
                    }
                }
                col += span_width;
            }
            let preview_element = Element::new(&font, ElementContent::Children(preview_children))
                .colors(ElementColors {
                    border: BorderColor::default(),
                    bg: colors.bg.into(),
                    text: colors.fg.into(),
                });

            elements.push(
                Element::new(
                    &font,
                    ElementContent::Children(vec![list_element, sep_element, preview_element]),
                )
                .colors(ElementColors {
                    border: BorderColor::default(),
                    bg: colors.bg.into(),
                    text: colors.fg.into(),
                })
                .display(DisplayType::Block)
                .min_width(Some(Dimension::Cells(inner_cols as f32))),
            );
        }

        // ---- rule above the footer (only when the footer itself is shown)
        if footer_text.is_some() {
            elements.push(
                Element::new(&font, ElementContent::Text(rule))
                    .colors(ElementColors {
                        border: BorderColor::default(),
                        bg: LinearRgba::TRANSPARENT.into(),
                        text: colors.dim_fg.into(),
                    })
                    .display(DisplayType::Block),
            );
        }

        // Exactly one footer row, or none when the hints are hidden: the long
        // help line is truncated rather than stealing a second row from the
        // list.  Without a footer the window shrinks by one row instead of
        // leaving a blank line.
        let footer_line = footer_text.map(|text| truncate_to_width(text, inner_cols));
        for line in footer_line.iter() {
            elements.push(
                Element::new(&font, ElementContent::Text(line.clone()))
                    .colors(ElementColors {
                        border: BorderColor::default(),
                        bg: LinearRgba::TRANSPARENT.into(),
                        text: footer_fg.into(),
                    })
                    .display(DisplayType::Block),
            );
        }

        let desired_pixel_width = panel_cols as f32 * cell_w;
        let desired_pixel_height = rect.rows as f32 * cell_h;

        let top_bar_height = if term_window.show_tab_bar && !term_window.config.tab_bar_at_bottom {
            term_window.tab_bar_pixel_height().unwrap_or(0.)
        } else {
            0.
        };
        let (padding_left, padding_top) = term_window.padding_left_top();
        let border = term_window.get_os_border();
        let top_pixel_y = top_bar_height + padding_top + border.top.get() as f32;

        let cols_px = term_window.dimensions.pixel_width as f32;
        let rows_px = term_window.dimensions.pixel_height as f32;
        let x_adjust = ((cols_px - padding_left - desired_pixel_width) / 2.).max(0.);
        let y_adjust = ((rows_px - top_pixel_y - desired_pixel_height) / 2.).max(0.);

        let element = Element::new(&font, ElementContent::Children(elements))
            .colors(ElementColors {
                border: BorderColor::new(colors.border),
                bg: colors.bg.into(),
                text: colors.fg.into(),
            })
            .border(BoxDimension::new(Dimension::Pixels(1.)))
            .border_corners(Some(Corners {
                top_left: SizedPoly {
                    width: Dimension::Cells(0.25),
                    height: Dimension::Cells(0.25),
                    poly: TOP_LEFT_ROUNDED_CORNER,
                },
                top_right: SizedPoly {
                    width: Dimension::Cells(0.25),
                    height: Dimension::Cells(0.25),
                    poly: TOP_RIGHT_ROUNDED_CORNER,
                },
                bottom_left: SizedPoly {
                    width: Dimension::Cells(0.25),
                    height: Dimension::Cells(0.25),
                    poly: BOTTOM_LEFT_ROUNDED_CORNER,
                },
                bottom_right: SizedPoly {
                    width: Dimension::Cells(0.25),
                    height: Dimension::Cells(0.25),
                    poly: BOTTOM_RIGHT_ROUNDED_CORNER,
                },
            }))
            .padding(BoxDimension {
                // Exactly one cell on each side: `inner_cols = cols - 2`
                // then matches the available width, so the left and right
                // insets are identical instead of the right one being wider.
                left: Dimension::Cells(1.),
                right: Dimension::Cells(1.),
                top: Dimension::Cells(0.25),
                bottom: Dimension::Cells(0.25),
            })
            .min_width(Some(Dimension::Pixels(desired_pixel_width)));

        let computed = term_window.compute_element(
            &LayoutContext {
                height: DimensionContext {
                    dpi: term_window.dimensions.dpi as f32,
                    pixel_max: term_window.dimensions.pixel_height as f32,
                    pixel_cell: cell_h,
                },
                width: DimensionContext {
                    dpi: term_window.dimensions.dpi as f32,
                    pixel_max: term_window.dimensions.pixel_width as f32,
                    pixel_cell: cell_w,
                },
                bounds: euclid::rect(
                    padding_left + x_adjust,
                    top_pixel_y + y_adjust,
                    desired_pixel_width,
                    desired_pixel_height,
                ),
                metrics: &metrics,
                gl_state: term_window.render_state.as_ref().unwrap(),
                zindex: 100,
            },
            &element,
        )?;

        // Convert the pixel origin back into cell coordinates for the mouse
        // handler.  Cells are measured from the padding/border origin, the
        // same way `mouse_event_impl` does it.
        let origin_x = ((padding_left + x_adjust) / cell_w).round() as usize;
        let origin_y = (((top_pixel_y + y_adjust) - top_bar_height - padding_top) / cell_h)
            .round()
            .max(0.) as usize;

        let computed_rect = PanelRect {
            x: origin_x,
            y: origin_y,
            ..rect
        };

        Ok((vec![computed], computed_rect))
    }
}

fn flush_messages(ctx: &mut AcceptCtx) {
    let cfg = &ctx.term_window.config.snippet_panel;
    let timeout = cfg.notify_timeout_ms;
    let enabled = cfg.desktop_notifications;
    let mut messages: Vec<String> = Vec::new();
    if enabled {
        messages.extend(ctx.messages.drain(..));
    } else {
        ctx.messages.clear();
    }
    // `urgent_messages` bypasses the switch: the panel is closed by then.
    messages.extend(ctx.urgent_messages.drain(..));
    for message in messages {
        wezterm_toast_notification::show(wezterm_toast_notification::ToastNotification {
            title: "wezterm snippet".to_string(),
            message,
            url: None,
            timeout: Some(std::time::Duration::from_millis(timeout.max(1000))),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed {
        n: usize,
        generation: RefCell<u64>,
    }

    impl ListPreviewSource for Fixed {
        fn source_name(&self) -> &str {
            "test"
        }
        fn rescan(&self) {}
        fn generation(&self) -> u64 {
            *self.generation.borrow()
        }
        fn filter(&self, _query: &str) -> Vec<usize> {
            (0..self.n).collect()
        }
        fn total(&self) -> usize {
            self.n
        }
        fn row(&self, idx: usize) -> ListRowText {
            ListRowText {
                gutter: " ".to_string(),
                title: format!("item {idx}"),
                badges: String::new(),
            }
        }
        fn preview_lines(
            &self,
            _idx: usize,
            _width: usize,
            _wrap: bool,
            _show_meta: bool,
        ) -> Vec<PreviewLine> {
            vec![]
        }
        fn accept(&self, _ctx: &mut AcceptCtx, _idx: usize, _exec: bool) {}
        fn custom_key(
            &self,
            _ctx: &mut AcceptCtx,
            _idx: Option<usize>,
            _key: KeyCode,
            _mods: KeyModifiers,
        ) -> bool {
            false
        }
        fn footer(&self) -> String {
            String::new()
        }
    }

    fn modal_with(n: usize) -> ListPreviewModal {
        let source = Fixed {
            n,
            generation: RefCell::new(0),
        };
        let modal = ListPreviewModal::new(Box::new(source), None, false, true, FooterMode::Short);
        let matches = (0..n).collect();
        *modal.matches.borrow_mut() = Some(MatchResults {
            selection: String::new(),
            generation: 0,
            matches,
        });
        *modal.max_rows_on_screen.borrow_mut() = 5;
        modal
    }

    #[test]
    fn navigation_does_not_panic_or_escape_bounds() {
        for n in [0usize, 1, 2, 7, 40] {
            let modal = modal_with(n);
            for _ in 0..100 {
                modal.move_down();
                assert!(*modal.selected_row.borrow() < n.max(1));
            }
            for _ in 0..100 {
                modal.move_up();
                assert!(*modal.selected_row.borrow() < n.max(1));
            }
            for _ in 0..100 {
                modal.page(true);
                assert!(*modal.selected_row.borrow() < n.max(1));
                modal.page(false);
                assert!(*modal.selected_row.borrow() < n.max(1));
            }
            let selected = *modal.selected_row.borrow();
            let top = *modal.top_row.borrow();
            assert!(top <= selected);
        }
    }

    #[test]
    fn top_row_never_scrolls_past_the_last_match() {
        // Every visible body row below `top + len` renders blank, which reads
        // as a phantom empty entry at the bottom of the list.
        let modal = modal_with(12);
        *modal.max_rows_on_screen.borrow_mut() = 5;
        for _ in 0..50 {
            modal.move_down();
            let top = *modal.top_row.borrow();
            let len = modal.match_count();
            let max_rows = *modal.max_rows_on_screen.borrow();
            assert!(
                top + max_rows <= len.max(1),
                "top={top} max_rows={max_rows} len={len}"
            );
        }
        // A filtered list that shrinks below the old top must pull it back.
        *modal.matches.borrow_mut() = Some(MatchResults {
            selection: String::new(),
            generation: 0,
            matches: vec![1, 3],
        });
        *modal.top_row.borrow_mut() = 9;
        let clamped = modal.clamp_top(9);
        assert_eq!(clamped, 0);
    }

    #[test]
    fn footer_mode_cycles_hidden_short_full() {
        assert_eq!(FooterMode::Hidden.next(), FooterMode::Short);
        assert_eq!(FooterMode::Short.next(), FooterMode::Full);
        assert_eq!(FooterMode::Full.next(), FooterMode::Hidden);
        assert_eq!(FooterMode::default(), FooterMode::Short);
    }

    #[test]
    fn selecting_the_same_row_is_a_no_op() {
        let modal = modal_with(10);
        modal.select(3);
        assert_eq!(*modal.selected_row.borrow(), 3);
        modal.select(3);
        assert_eq!(*modal.selected_row.borrow(), 3);
        modal.select(999);
        assert_eq!(*modal.selected_row.borrow(), 9);
    }

    #[test]
    fn width_helpers_handle_cjk() {
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("中文"), 4);
        assert_eq!(display_width(&truncate_to_width("中文标题很长", 5)), 5);
        assert_eq!(display_width(&pad_to_width("中", 4)), 4);
    }

    #[test]
    fn slice_columns_respects_wide_chars() {
        assert_eq!(slice_columns("abcdef", 2, 4), "cd");
        assert_eq!(slice_columns("中文字", 2, 4), "文");
        assert_eq!(slice_columns("abc", 1, 99), "bc");
        assert_eq!(slice_columns("abc", 3, 1), "");
    }

    #[test]
    fn highlight_spans_marks_matched_chars() {
        let spans = ListPreviewModal::highlight_spans("查看 pod 详情", "pod");
        let text: String = spans.iter().map(|s| s.text.clone()).collect();
        assert_eq!(text, "查看 pod 详情");
        let highlighted: String = spans
            .iter()
            .filter(|s| s.highlight)
            .map(|s| s.text.clone())
            .collect();
        assert_eq!(highlighted.to_lowercase(), "pod");
    }

    #[test]
    fn highlight_ignores_filter_only_tokens() {
        let spans = ListPreviewModal::highlight_spans("iptables 列表", "#net !");
        assert!(spans.iter().all(|s| !s.highlight));
    }

    #[test]
    fn delete_last_word_matches_shell_behaviour() {
        let mut text = String::from("kubectl get pod ");
        delete_last_word(&mut text);
        assert_eq!(text, "kubectl get ");
        delete_last_word(&mut text);
        assert_eq!(text, "kubectl ");
        let mut text = String::from("single");
        delete_last_word(&mut text);
        assert_eq!(text, "");
        let mut text = String::from("   ");
        delete_last_word(&mut text);
        assert_eq!(text, "");
    }

    #[test]
    fn edit_line_rejects_command_keys() {
        // `edit_line` is the fallback for keys no explicit arm claimed.  If it
        // ever starts accepting a command key, that key silently turns into a
        // text edit instead of running its command - which is exactly how
        // Enter/Ctrl+H/Ctrl+I once stopped working.
        for (key, mods) in [
            (KeyCode::Enter, KeyModifiers::NONE),
            (KeyCode::Enter, KeyModifiers::CTRL),
            (KeyCode::Escape, KeyModifiers::NONE),
            (KeyCode::Char('h'), KeyModifiers::CTRL),
            (KeyCode::Char('i'), KeyModifiers::CTRL),
            (KeyCode::Char('j'), KeyModifiers::CTRL),
            (KeyCode::Char('k'), KeyModifiers::CTRL),
            (KeyCode::Char('r'), KeyModifiers::CTRL),
            (KeyCode::Char('n'), KeyModifiers::CTRL),
            (KeyCode::Char('d'), KeyModifiers::CTRL),
            (KeyCode::Char('t'), KeyModifiers::CTRL),
            (KeyCode::Char('w'), KeyModifiers::ALT),
        ] {
            let mut text = String::from("abc");
            let mut cursor = 3;
            assert!(
                !ListPreviewModal::edit_line(key, mods, &mut text, &mut cursor),
                "{key:?} {mods:?} must not be typed into the search box"
            );
            assert_eq!(text, "abc");
        }
    }

    #[test]
    fn preview_selection_orders_ends() {
        let forward = PreviewSelection {
            anchor: (1, 2),
            cursor: (3, 4),
        };
        let backward = PreviewSelection {
            anchor: (3, 4),
            cursor: (1, 2),
        };
        assert_eq!(forward.ordered(), ((1, 2), (3, 4)));
        assert_eq!(backward.ordered(), ((1, 2), (3, 4)));
    }
}
