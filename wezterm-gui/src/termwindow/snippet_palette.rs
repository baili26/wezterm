//! The snippet palette: scan a directory of snippet files, search them, and
//! insert the selected one into a pane.
//!
//! Personal-fork feature.  Snippets are plain files; the first `---` block is
//! optional front matter:
//!
//! ```text
//! ---
//! title: 查看某命名空间的 pod
//! desc: 宽格式列出
//! tags: [k8s, pod]
//! kind: command
//! danger: true
//! pin: true
//! ---
//! kubectl -n ? get pod -o wide
//! ```
//!
//! A single `?` in a *single-line* snippet marks where the cursor should
//! land after insertion; `??` is a literal `?`.  Multi-line snippets never
//! move the cursor: getting back to a line in the middle needs up-arrow
//! presses, which are unreliable across soft-wrap and wide characters.
use crate::overlay::selector::{matcher_pattern, matcher_score};
use crate::termwindow::list_preview::{
    display_width, truncate_to_width, AcceptCtx, ListPreviewSource, ListRowText, PreviewLine, Span,
};
use frecency::Frecency;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use wezterm_term::{KeyCode, KeyModifiers};

/// The marker that names the cursor landing spot in a single-line snippet.
///
/// `$|$` rather than a bare `?`: prompt strings and shell globs legitimately
/// contain `?`, which made `?` unusable as a marker.
pub const PLACEHOLDER: &str = "$|$";

#[derive(Debug, Clone)]
pub struct Snippet {
    /// Absolute path
    pub abs_path: PathBuf,
    /// Path relative to the snippet directory; also the identity used for
    /// frecency so renaming the directory doesn't lose history.
    pub rel: String,
    pub title: String,
    pub desc: String,
    pub tags: Vec<String>,
    pub kind: String,
    pub danger: bool,
    pub pin: bool,
    /// Body with the placeholder removed, ready to insert.
    pub text: String,
    /// Body as written, with the placeholder still in place.  Used by the
    /// preview so the landing spot is visible.
    pub display_text: String,
    /// Number of characters between the placeholder and the end of the line.
    /// `None` when the snippet has no placeholder, is multi-line, or the
    /// feature is disabled.
    pub chars_after: Option<usize>,
    pub is_multiline: bool,
}

impl Snippet {
    /// Preview block: the metadata header, a short rule, then the body.
    ///
    /// `show_meta` controls the header; `Ctrl+I` toggles it at runtime, which
    /// is why the header can afford to be complete: people who don't want the
    /// vertical cost turn it off once and keep it off.
    pub fn preview_lines(&self, width: usize, wrap: bool, show_meta: bool) -> Vec<PreviewLine> {
        let mut out: Vec<PreviewLine> = vec![];

        if show_meta {
            out.push(meta_line("title:", &self.title, width));
            out.push(meta_line("path:", &self.rel, width));
            if !self.tags.is_empty() {
                out.push(meta_line("tags:", &self.tags.join(", "), width));
            }
            if !self.desc.is_empty() {
                out.push(meta_line("desc:", &self.desc, width));
            }
            let mut flags = vec![self.kind.clone()];
            if self.pin {
                flags.push("pinned".to_string());
            }
            if self.danger {
                flags.push("danger".to_string());
            }
            if self.is_multiline {
                flags.push("multiline".to_string());
            }
            out.push(meta_line("kind:", &flags.join(" "), width));
            if let Some(after) = self.chars_after {
                // Where the cursor lands: 1-based column of the marker in the
                // body as written.  `self.text` is the body with the marker
                // already removed, so back up over the characters that
                // followed it.
                let column = self.text.chars().count() - after + 1;
                out.push(meta_line("cursor:", &format!("column {column}"), width));
            }
            let rule_w = (width / 2).max(4).min(width);
            out.push(vec![Span::dim("\u{2500}".repeat(rule_w))]);
        }

        for line in self.display_text.lines() {
            if wrap {
                for part in wrap_line(line, width) {
                    out.push(Self::placeholder_spans(&part, width));
                }
            } else {
                out.push(Self::placeholder_spans(
                    &truncate_to_width(line, width),
                    width,
                ));
            }
        }
        out
    }

    /// Split one preview line so the placeholder marker is highlighted.
    fn placeholder_spans(line: &str, _width: usize) -> PreviewLine {
        match line.find(PLACEHOLDER) {
            Some(pos) => {
                let mut spans = vec![];
                let before = &line[..pos];
                let after = &line[pos + PLACEHOLDER.len()..];
                if !before.is_empty() {
                    spans.push(Span::normal(before.to_string()));
                }
                spans.push(Span::highlight(PLACEHOLDER.to_string()));
                if !after.is_empty() {
                    spans.push(Span::normal(after.to_string()));
                }
                spans
            }
            None => vec![Span::normal(line.to_string())],
        }
    }
}

/// One `label: value` line of the preview header.
///
/// The label is dim and padded to a fixed width so the values line up in a
/// column; the value keeps the normal color so it reads as the content.
fn meta_line(label: &str, value: &str, width: usize) -> PreviewLine {
    // Pad to the longest label (`cursor:`) so every value starts in the
    // same column.
    let label = format!("{label:<8}");
    let label_w = display_width(&label);
    if label_w >= width {
        return vec![Span::dim(truncate_to_width(&label, width))];
    }
    let value = truncate_to_width(value, width - label_w);
    vec![Span::dim(label), Span::normal(value)]
}

fn wrap_line(line: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    let mut out = vec![];
    let mut cur = String::new();
    let mut w = 0;
    for c in line.chars() {
        let cw = display_width(&c.to_string());
        if w + cw > width && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
            w = 0;
        }
        cur.push(c);
        w += cw;
    }
    out.push(cur);
    out
}

// ---------------------------------------------------------------------
// Front matter
// ---------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
pub struct FrontMatter {
    pub title: Option<String>,
    pub desc: Option<String>,
    pub tags: Vec<String>,
    pub kind: Option<String>,
    pub danger: Option<bool>,
    pub pin: Option<bool>,
}

impl FrontMatter {
    fn has_known_key(&self) -> bool {
        self.title.is_some()
            || self.desc.is_some()
            || !self.tags.is_empty()
            || self.kind.is_some()
            || self.danger.is_some()
            || self.pin.is_some()
    }
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2 {
        let first = s.as_bytes()[0];
        let last = s.as_bytes()[s.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return s[1..s.len() - 1].to_string();
        }
    }
    s.to_string()
}

/// Interpret a y/n (or true/false) answer for a boolean metadata field.
fn parse_bool_text(text: &str) -> &'static str {
    match text.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" | "true" | "1" | "on" => "true",
        _ => "false",
    }
}

fn parse_bool(s: &str) -> Option<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" => Some(true),
        "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn parse_list(s: &str) -> Vec<String> {
    let s = s.trim();
    let inner = if s.starts_with('[') && s.ends_with(']') {
        &s[1..s.len() - 1]
    } else {
        s
    };
    inner
        .split(',')
        .map(|t| unquote(t.trim()))
        .filter(|t| !t.is_empty())
        .collect()
}

/// Split an optional front-matter block off the front of `body`.
///
/// Only treats the leading `---` block as front matter when it actually
/// contains one of the keys we understand, so a k8s yaml file that legitimately
/// begins with `---` is not mistaken for metadata.
pub fn split_front_matter(body: &str) -> (FrontMatter, String) {
    let normalized = body.replace("\r\n", "\n");
    let rest = match normalized.strip_prefix("---\n") {
        Some(rest) => rest,
        None => return (FrontMatter::default(), normalized),
    };
    let end = match rest.find("\n---\n") {
        Some(idx) => idx,
        None => match rest.find("\n---") {
            // a file that is only front matter, with no trailing newline
            Some(idx) if rest[idx + 4..].trim().is_empty() => idx,
            _ => return (FrontMatter::default(), normalized),
        },
    };

    let meta_text = &rest[..end];
    let after = {
        let tail = &rest[end + 4..];
        tail.strip_prefix('\n').unwrap_or(tail).to_string()
    };

    let mut meta = FrontMatter::default();
    for line in meta_text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = match line.split_once(':') {
            Some(kv) => kv,
            None => continue,
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();
        match key.as_str() {
            "title" => meta.title = Some(unquote(value)),
            "desc" | "description" => meta.desc = Some(unquote(value)),
            "tags" | "tag" => meta.tags = parse_list(value),
            "kind" => meta.kind = Some(unquote(value)),
            "danger" => meta.danger = parse_bool(value),
            "pin" | "pinned" | "favorite" => meta.pin = parse_bool(value),
            _ => {}
        }
    }

    if !meta.has_known_key() {
        return (FrontMatter::default(), normalized);
    }

    (meta, after)
}

/// Replace (or insert) one front-matter key, leaving every other line alone.
pub fn set_front_matter_key(body: &str, key: &str, value: &str) -> String {
    let normalized = body.replace("\r\n", "\n");
    let (meta, after) = split_front_matter(&normalized);
    if !meta.has_known_key() {
        // No front matter yet: prepend a new block.
        return format!("---\n{key}: {value}\n---\n{normalized}");
    }

    // Re-derive the metadata block boundaries so we can rewrite only the
    // matching line.
    let header_len = 4; // "---\n"
    let rest = &normalized[header_len..];
    let end = rest.find("\n---\n").or_else(|| rest.find("\n---")).unwrap();
    let meta_text = &rest[..end];
    let tail = &rest[end + 4..];
    let tail = tail.strip_prefix('\n').unwrap_or(tail);

    let mut rewritten = String::new();
    let mut found = false;
    for line in meta_text.lines() {
        let line_key = line
            .split_once(':')
            .map(|(k, _)| k.trim().to_ascii_lowercase())
            .unwrap_or_default();
        if line_key == key.to_ascii_lowercase() {
            rewritten.push_str(&format!("{key}: {value}\n"));
            found = true;
        } else {
            rewritten.push_str(line);
            rewritten.push('\n');
        }
    }
    if !found {
        rewritten.push_str(&format!("{key}: {value}\n"));
    }

    let _ = after;
    format!("---\n{rewritten}---\n{tail}")
}

// ---------------------------------------------------------------------
// Scanning
// ---------------------------------------------------------------------

fn guess_kind(path: &str) -> String {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".yaml")
        || lower.ends_with(".yml")
        || lower.ends_with(".json")
        || lower.ends_with(".toml")
        || lower.ends_with(".conf")
    {
        "config".to_string()
    } else {
        "command".to_string()
    }
}

/// Extract the placeholder from a body.
///
/// Returns `(text_without_marker, display_text, chars_after)`.  `chars_after`
/// is `None` when there is no marker.  Only a single-line body is eligible:
/// getting back to a line in the middle needs up-arrow presses, which are
/// unreliable across soft-wrap and wide characters.
fn extract_placeholder(text: &str) -> (String, String, Option<usize>) {
    if text.contains('\n') {
        return (text.to_string(), text.to_string(), None);
    }
    let Some(pos) = text.find(PLACEHOLDER) else {
        return (text.to_string(), text.to_string(), None);
    };
    let before = &text[..pos];
    let after = &text[pos + PLACEHOLDER.len()..];
    let removed = format!("{before}{after}");
    (removed, text.to_string(), Some(after.chars().count()))
}

fn parse_file(dir: &Path, rel: &str) -> Option<Snippet> {
    let abs_path = dir.join(rel);
    let body = std::fs::read_to_string(&abs_path).ok()?;
    let (meta, raw_text) = split_front_matter(&body);
    // Keep blank lines inside the body, but drop a trailing newline.
    let raw_text = raw_text.trim_end_matches('\n').to_string();
    if raw_text.trim().is_empty() {
        // A snippet with no body is not usable: skip it entirely, otherwise it
        // shows up as an untitled empty row.
        return None;
    }

    let is_multiline = raw_text.contains('\n');
    let (text, display_text, chars_after) = extract_placeholder(&raw_text);

    Some(Snippet {
        abs_path,
        rel: rel.to_string(),
        title: meta
            .title
            .clone()
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| rel.to_string()),
        desc: meta.desc.clone().unwrap_or_default(),
        tags: meta.tags.clone(),
        kind: meta.kind.clone().unwrap_or_else(|| guess_kind(rel)),
        danger: meta.danger.unwrap_or(false),
        pin: meta.pin.unwrap_or(false),
        text,
        display_text,
        chars_after,
        is_multiline,
    })
}

/// Read every snippet under `dir`.  Directories such as `.trash/` are skipped.
pub fn scan(dir: &Path) -> Vec<Snippet> {
    let mut out = vec![];
    let walker = walkdir::WalkDir::new(dir)
        .max_depth(4)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            // Skip hidden files/directories such as `.trash/`; `inbox/` is
            // deliberately included so newly created snippets show up.
            e.depth() == 0 || !e.file_name().to_string_lossy().starts_with('.')
        });

    for entry in walker.flatten() {
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = match entry.path().strip_prefix(dir) {
            Ok(rel) => rel.to_string_lossy().to_string(),
            Err(_) => continue,
        };
        if let Some(snippet) = parse_file(dir, &rel) {
            out.push(snippet);
        }
    }

    out.sort_by(|a, b| a.rel.cmp(&b.rel));
    out
}

// ---------------------------------------------------------------------
// Frecency
// ---------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone)]
struct Recent {
    rel: String,
    frecency: Frecency,
}

fn recent_file_name() -> PathBuf {
    config::DATA_DIR.join("snippet-frecency.json")
}

fn load_recents() -> Vec<Recent> {
    std::fs::File::open(recent_file_name())
        .ok()
        .and_then(|f| serde_json::from_reader::<_, Vec<Recent>>(f).ok())
        .unwrap_or_default()
}

fn save_recent(rel: &str) {
    let mut recents = load_recents();
    if let Some(recent) = recents.iter_mut().find(|r| r.rel == rel) {
        recent.frecency.register_access();
    } else {
        let mut frecency = Frecency::new();
        frecency.register_access();
        recents.push(Recent {
            rel: rel.to_string(),
            frecency,
        });
    }
    if let Ok(json) = serde_json::to_string(&recents) {
        if let Err(err) = std::fs::write(recent_file_name(), json) {
            log::error!("snippet: failed to save frecency: {err:#}");
        }
    }
}

// ---------------------------------------------------------------------
// Filtering
// ---------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Term {
    /// `#tag`
    Tag(String),
    /// `@dir`
    Dir(String),
    /// `kind:command`
    Kind(String),
    /// `!`
    Danger,
    /// `/text` — also search the body
    Body(String),
    /// Free text, fuzzy matched
    Fuzzy(String),
}

fn parse_query(query: &str) -> Vec<Term> {
    let mut terms = vec![];
    for raw in query.split_whitespace() {
        if raw == "!" {
            terms.push(Term::Danger);
        } else if let Some(rest) = raw.strip_prefix('#') {
            if !rest.is_empty() {
                terms.push(Term::Tag(rest.to_ascii_lowercase()));
            }
        } else if let Some(rest) = raw.strip_prefix('@') {
            if !rest.is_empty() {
                terms.push(Term::Dir(rest.to_ascii_lowercase()));
            }
        } else if let Some(rest) = raw.strip_prefix('/') {
            if !rest.is_empty() {
                terms.push(Term::Body(rest.to_ascii_lowercase()));
            }
        } else if let Some(rest) = raw.strip_prefix("kind:") {
            if !rest.is_empty() {
                terms.push(Term::Kind(rest.to_ascii_lowercase()));
            }
        } else {
            terms.push(Term::Fuzzy(raw.to_string()));
        }
    }
    terms
}

fn dir_of(rel: &str) -> &str {
    match rel.rsplit_once('/') {
        Some((dir, _)) => dir,
        None => "",
    }
}

fn score_snippet(snippet: &Snippet, terms: &[Term]) -> Option<u32> {
    let mut score = 0u32;
    for term in terms {
        match term {
            Term::Danger => {
                if !snippet.danger {
                    return None;
                }
            }
            Term::Tag(tag) => {
                let hit = snippet
                    .tags
                    .iter()
                    .any(|t| t.to_ascii_lowercase().contains(tag.as_str()))
                    || dir_of(&snippet.rel)
                        .to_ascii_lowercase()
                        .contains(tag.as_str());
                if !hit {
                    return None;
                }
                score += 50;
            }
            Term::Dir(dir) => {
                if !snippet.rel.to_ascii_lowercase().starts_with(dir.as_str()) {
                    return None;
                }
                score += 50;
            }
            Term::Kind(kind) => {
                if !snippet.kind.to_ascii_lowercase().contains(kind.as_str()) {
                    return None;
                }
                score += 50;
            }
            Term::Body(text) => {
                if !snippet.text.to_ascii_lowercase().contains(text.as_str()) {
                    return None;
                }
                score += 50;
            }
            Term::Fuzzy(text) => {
                let haystack = format!(
                    "{} {} {} {}",
                    snippet.title,
                    snippet.desc,
                    snippet.tags.join(" "),
                    snippet.rel
                );
                let pattern = matcher_pattern(text);
                let s = matcher_score(&pattern, &haystack)?;
                score = score.saturating_add(s);
            }
        }
    }
    Some(score)
}

// ---------------------------------------------------------------------
// The source
// ---------------------------------------------------------------------

pub struct SnippetSource {
    dir: PathBuf,
    snippets: RefCell<Vec<Snippet>>,
    recents: RefCell<HashMap<String, f64>>,
    trash_enabled: bool,
    /// Bumped whenever `snippets` changes, so the modal re-filters even when
    /// the query string is unchanged.
    generation: RefCell<u64>,
    /// Body captured for a pending `Ctrl+N`, waiting for the title prompt.
    pending_body: RefCell<Option<String>>,
    /// Snippet picked for a pending `Ctrl+D` confirmation.
    pending_delete: RefCell<Option<String>>,
    /// Which metadata field the pending prompt is editing.
    pending_meta: RefCell<Option<MetaField>>,
    /// The item the metadata edit applies to.
    pending_meta_target: RefCell<Option<usize>>,
}

/// The fields `Ctrl+T` walks through, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
enum MetaField {
    Title,
    Tags,
    Desc,
    Pin,
    Danger,
}

impl MetaField {
    fn key(&self) -> &'static str {
        match self {
            MetaField::Title => "title",
            MetaField::Tags => "tags",
            MetaField::Desc => "desc",
            MetaField::Pin => "pin",
            MetaField::Danger => "danger",
        }
    }

    fn next(&self) -> Option<MetaField> {
        match self {
            MetaField::Title => Some(MetaField::Tags),
            MetaField::Tags => Some(MetaField::Desc),
            MetaField::Desc => Some(MetaField::Pin),
            MetaField::Pin => Some(MetaField::Danger),
            MetaField::Danger => None,
        }
    }

    fn prompt(&self) -> &'static str {
        match self {
            MetaField::Title => "title",
            MetaField::Tags => "tags (comma separated)",
            MetaField::Desc => "description",
            MetaField::Pin => "pinned? [y/N]",
            MetaField::Danger => "danger? [y/N]",
        }
    }
}

impl SnippetSource {
    pub fn new(dir: PathBuf, placeholder_enabled: bool, trash_enabled: bool) -> Self {
        let _ = placeholder_enabled;
        let recents = load_recents()
            .into_iter()
            .map(|r| (r.rel, r.frecency.score()))
            .collect();
        let snippets = scan(&dir);
        Self {
            dir,
            snippets: RefCell::new(snippets),
            recents: RefCell::new(recents),
            trash_enabled,
            generation: RefCell::new(0),
            pending_body: RefCell::new(None),
            pending_delete: RefCell::new(None),
            pending_meta: RefCell::new(None),
            pending_meta_target: RefCell::new(None),
        }
    }

    fn bump_generation(&self) {
        *self.generation.borrow_mut() += 1;
    }

    fn start_metadata_prompt(&self, ctx: &mut AcceptCtx, idx: usize) {
        let snippet = match self.snippet(idx) {
            Some(s) => s,
            None => return,
        };
        let field = MetaField::Title;
        let current = snippet.title.clone();
        *self.pending_meta.borrow_mut() = Some(field.clone());
        *self.pending_meta_target.borrow_mut() = Some(idx);
        ctx.prompt_for(
            format!("{} (esc to cancel)", field.prompt()),
            current,
            Some(idx),
        );
    }

    fn advance_metadata(&self, ctx: &mut AcceptCtx, idx: usize, field: &MetaField) {
        let next = match field.next() {
            Some(next) => next,
            None => {
                *self.pending_meta.borrow_mut() = None;
                *self.pending_meta_target.borrow_mut() = None;
                self.rescan();
                return;
            }
        };
        let snippet = match self.snippet(idx) {
            Some(s) => s,
            None => {
                *self.pending_meta.borrow_mut() = None;
                return;
            }
        };
        let current = match next {
            MetaField::Title => snippet.title.clone(),
            MetaField::Tags => snippet.tags.join(", "),
            MetaField::Desc => snippet.desc.clone(),
            MetaField::Pin => if snippet.pin { "y" } else { "n" }.to_string(),
            MetaField::Danger => if snippet.danger { "y" } else { "n" }.to_string(),
        };
        *self.pending_meta.borrow_mut() = Some(next.clone());
        ctx.prompt_for(
            format!("{} (esc to cancel)", next.prompt()),
            current,
            Some(idx),
        );
    }

    fn update_metadata(&self, snippet: &Snippet, key: &str, value: &str) -> anyhow::Result<()> {
        let body = std::fs::read_to_string(&snippet.abs_path)?;
        let updated = set_front_matter_key(&body, key, value);
        std::fs::write(&snippet.abs_path, updated)?;
        // Re-read so the next metadata prompt shows the new values and the
        // list reflects the change.
        self.rescan();
        Ok(())
    }

    fn snippet(&self, idx: usize) -> Option<Snippet> {
        self.snippets.borrow().get(idx).cloned()
    }

    fn write_text(&self, ctx: &mut AcceptCtx, snippet: &Snippet, exec: bool) {
        if !ctx.paste(&snippet.text) {
            ctx.notify_inline("could not insert: no usable pane");
            return;
        }

        let mut can_exec = exec;
        if snippet.danger {
            // The panel is closed by now, so the footer cannot carry this:
            // always surface it as a desktop notification.
            ctx.notify_always(format!(
                "danger snippet \"{}\": inserted but not run. Press enter to run it.",
                snippet.title
            ));
            can_exec = false;
        }
        if let Some(after) = snippet.chars_after {
            ctx.move_cursor_left(after);
            if exec {
                ctx.notify_inline("snippet has a cursor placeholder; inserted but not run");
            }
            can_exec = false;
        }

        if can_exec {
            ctx.submit_line();
        }

        save_recent(&snippet.rel);
        let mut frecency = Frecency::new();
        frecency.register_access();
        self.recents
            .borrow_mut()
            .insert(snippet.rel.clone(), frecency.score());
    }

    fn create(&self, title: &str, body: &str) -> Option<String> {
        let inbox = self.dir.join("inbox");
        if let Err(err) = std::fs::create_dir_all(&inbox) {
            log::error!("snippet: create inbox: {err:#}");
            return None;
        }
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let name = format!("{stamp}.snip");
        let path = inbox.join(&name);
        let quoted = title.replace('"', "'");
        let body = body.trim_end();
        if body.trim().is_empty() {
            log::error!("snippet: refusing to create an empty snippet");
            return None;
        }
        let content = format!("---\ntitle: \"{quoted}\"\n---\n{body}\n");
        if let Err(err) = std::fs::write(&path, content) {
            log::error!("snippet: write {path:?}: {err:#}");
            return None;
        }
        self.rescan();
        Some(format!("inbox/{name}"))
    }

    pub fn trash(&self, rel: &str) -> anyhow::Result<PathBuf> {
        let from = self.dir.join(rel);
        let mut to = self.dir.join(".trash").join(rel);
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if to.exists() {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let file_name = to
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "snippet".to_string());
            to.set_file_name(format!("{file_name}.{stamp}"));
        }
        std::fs::rename(&from, &to)?;
        self.rescan();
        Ok(to)
    }
}

impl ListPreviewSource for SnippetSource {
    fn source_name(&self) -> &str {
        "snippets"
    }

    fn rescan(&self) {
        *self.snippets.borrow_mut() = scan(&self.dir);
        self.bump_generation();
    }

    fn generation(&self) -> u64 {
        *self.generation.borrow()
    }

    fn filter(&self, query: &str) -> Vec<usize> {
        let terms = parse_query(query);
        let snippets = self.snippets.borrow();
        let recents = self.recents.borrow();

        let mut scored: Vec<(usize, u32)> = snippets
            .iter()
            .enumerate()
            .filter_map(|(idx, snippet)| score_snippet(snippet, &terms).map(|s| (idx, s)))
            .collect();

        if terms.is_empty() {
            scored.sort_by(|(a_idx, _), (b_idx, _)| {
                let a = &snippets[*a_idx];
                let b = &snippets[*b_idx];
                b.pin.cmp(&a.pin).then_with(|| {
                    let a_score = recents.get(&a.rel).copied().unwrap_or(0.);
                    let b_score = recents.get(&b.rel).copied().unwrap_or(0.);
                    b_score
                        .partial_cmp(&a_score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| a.title.cmp(&b.title))
                })
            });
        } else {
            scored.sort_by(|(a_idx, a_score), (b_idx, b_score)| {
                b_score.cmp(a_score).then_with(|| {
                    let a = &snippets[*a_idx];
                    let b = &snippets[*b_idx];
                    let a_rec = recents.get(&a.rel).copied().unwrap_or(0.);
                    let b_rec = recents.get(&b.rel).copied().unwrap_or(0.);
                    b_rec
                        .partial_cmp(&a_rec)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
            });
        }

        scored.into_iter().map(|(idx, _)| idx).collect()
    }

    fn total(&self) -> usize {
        self.snippets.borrow().len()
    }

    fn row(&self, idx: usize) -> ListRowText {
        let snippets = self.snippets.borrow();
        match snippets.get(idx) {
            Some(snippet) => ListRowText {
                gutter: if snippet.danger {
                    "!".to_string()
                } else if snippet.pin {
                    "*".to_string()
                } else {
                    " ".to_string()
                },
                title: snippet.title.clone(),
                badges: format!(
                    "{}{}",
                    dir_of(&snippet.rel),
                    if snippet.tags.is_empty() {
                        String::new()
                    } else {
                        format!(" #{}", snippet.tags.join(" #"))
                    }
                ),
            },
            None => ListRowText::default(),
        }
    }

    fn preview_lines(
        &self,
        idx: usize,
        width: usize,
        wrap: bool,
        show_meta: bool,
    ) -> Vec<PreviewLine> {
        match self.snippets.borrow().get(idx) {
            Some(snippet) => snippet.preview_lines(width, wrap, show_meta),
            None => vec![],
        }
    }

    fn accept(&self, ctx: &mut AcceptCtx, idx: usize, exec: bool) {
        if let Some(snippet) = self.snippet(idx) {
            let placeholder_enabled = ctx.placeholder;
            let snippet = if !placeholder_enabled {
                // Insert the body exactly as written, marker included.
                let mut s = snippet;
                if s.chars_after.is_some() {
                    s.text = s.display_text.clone();
                    s.chars_after = None;
                }
                s
            } else {
                snippet
            };
            self.write_text(ctx, &snippet, exec);
        }
    }
    fn custom_key(
        &self,
        ctx: &mut AcceptCtx,
        idx: Option<usize>,
        key: KeyCode,
        mods: KeyModifiers,
    ) -> bool {
        match (key, mods) {
            (KeyCode::Char('n'), KeyModifiers::CTRL) => {
                let body = match ctx.term_window.get_active_pane_or_overlay() {
                    Some(pane) => ctx.term_window.selection_text(&pane),
                    None => String::new(),
                };
                // Creating a snippet with nothing selected would produce an
                // empty file, which is never what the user wants.
                if body.trim().is_empty() {
                    ctx.notify_inline(
                        "nothing selected: select text first, Ctrl+N uses it as the body",
                    );
                    return true;
                }
                *self.pending_body.borrow_mut() = Some(body);
                // Start with an empty field: the default title is only a
                // fallback for an empty answer.
                ctx.prompt_for(
                    "new snippet title (enter = first line of the body)".to_string(),
                    String::new(),
                    None,
                );
                true
            }
            (KeyCode::Char('d'), KeyModifiers::CTRL) => {
                if !self.trash_enabled {
                    ctx.notify_inline("delete is disabled (snippet_panel.enable_trash = false)");
                    return true;
                }
                let idx = match idx {
                    Some(idx) => idx,
                    None => return true,
                };
                let snippet = match self.snippet(idx) {
                    Some(s) => s,
                    None => return true,
                };
                *self.pending_delete.borrow_mut() = Some(snippet.rel.clone());
                ctx.request_mode(crate::termwindow::list_preview::InputMode::Confirm {
                    prompt: format!("move to .trash/ ? (y to confirm)  {}", snippet.title),
                });
                true
            }
            (KeyCode::Char('e'), KeyModifiers::CTRL) => {
                let idx = match idx {
                    Some(idx) => idx,
                    None => return true,
                };
                let snippet = match self.snippet(idx) {
                    Some(s) => s,
                    None => return true,
                };
                let editor_cmd = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
                let mut args: Vec<String> = shlex::Shlex::new(&editor_cmd).collect();
                if args.is_empty() {
                    args.push("vi".to_string());
                }
                let path = snippet.abs_path.display().to_string();
                // Back to the plain editor invocation: wrapping it in `sh -c`
                // made the pane inherit a mangled argv and print garbage.
                // `exit_behavior = "Hold"` handling is left to the user.
                args.push(path);
                ctx.term_window.spawn_command(
                    &config::keyassignment::SpawnCommand {
                        args: Some(args),
                        cwd: Some(self.dir.clone()),
                        ..Default::default()
                    },
                    crate::spawn::SpawnWhere::SplitPane(mux::tab::SplitRequest {
                        direction: mux::tab::SplitDirection::Horizontal,
                        target_is_second: true,
                        size: mux::tab::SplitSize::Percent(40),
                        top_level: false,
                    }),
                );
                // The editor now owns the screen; get out of its way.
                ctx.request_close();
                true
            }
            (KeyCode::Char('t'), KeyModifiers::CTRL) => {
                let idx = match idx {
                    Some(idx) => idx,
                    None => return true,
                };
                self.start_metadata_prompt(ctx, idx);
                true
            }
            _ => false,
        }
    }

    fn accept_prompt(&self, ctx: &mut AcceptCtx, idx: usize, text: &str) {
        // A pending body means this prompt was started by Ctrl+N.
        if let Some(body) = self.pending_body.borrow_mut().take() {
            // A body made only of blank lines would create an empty snippet
            // that shows up as a nameless row: refuse it here as well.
            if body.trim().is_empty() {
                ctx.notify_inline("nothing selected: the snippet body would be empty");
                return;
            }
            let default_title = body
                .lines()
                .find(|l| !l.trim().is_empty())
                .map(|l| l.trim().chars().take(40).collect::<String>())
                .unwrap_or_else(|| "untitled".to_string());
            let title = if text.trim().is_empty() {
                default_title
            } else {
                text.trim().to_string()
            };
            match self.create(&title, &body) {
                Some(rel) => {
                    ctx.notify_inline(format!("created {rel}"));
                    // Select the new snippet so it's visible immediately.
                    if let Some(idx) = self.snippets.borrow().iter().position(|s| s.rel == rel) {
                        ctx.select_request = Some(idx);
                    }
                }
                None => ctx.notify_inline("could not create the snippet"),
            }
            return;
        }

        // Otherwise it was a metadata field edit.
        let field = match self.pending_meta.borrow().clone() {
            Some(field) => field,
            None => return,
        };
        let snippet = match self.snippet(idx) {
            Some(s) => s,
            None => return,
        };
        let value = match &field {
            MetaField::Title => format!("\"{}\"", text.trim().replace('"', "'")),
            MetaField::Tags => format!(
                "[{}]",
                text.split(',')
                    .map(|t| t.trim())
                    .filter(|t| !t.is_empty())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            MetaField::Desc => format!("\"{}\"", text.trim().replace('"', "'")),
            MetaField::Pin => parse_bool_text(text).to_string(),
            MetaField::Danger => parse_bool_text(text).to_string(),
        };
        match self.update_metadata(&snippet, field.key(), &value) {
            Ok(()) => {
                self.advance_metadata(ctx, idx, &field);
                if self.pending_meta.borrow().is_none() {
                    ctx.notify_inline("metadata updated");
                }
            }
            Err(err) => ctx.notify_inline(format!("write failed: {err:#}")),
        }
    }

    fn confirm(&self, ctx: &mut AcceptCtx, _idx: usize) {
        let rel = match self.pending_delete.borrow_mut().take() {
            Some(rel) => rel,
            None => return,
        };
        match self.trash(&rel) {
            Ok(path) => ctx.notify_inline(format!("moved to {}", path.display())),
            Err(err) => ctx.notify_inline(format!("delete failed: {err:#}")),
        }
    }

    fn footer(&self) -> String {
        // One line by default; press Ctrl+H for the rest.
        "enter insert  ^enter run  ^e edit  ^w del-word  ^u clear  ^h=keys  ^i=meta  esc close"
            .to_string()
    }

    fn help_text(&self) -> &str {
        "arrows select  ^n new  ^d delete  ^t edit-meta  ^r rescan  ^j/^k scroll  alt+w wrap  pgup/pgdn page"
    }

    fn preview_override(
        &self,
        width: usize,
        wrap: bool,
        draft_title: &str,
        show_meta: bool,
    ) -> Option<Vec<PreviewLine>> {
        // While Ctrl+T is editing metadata, keep showing the item being edited
        // even if the user clicks another row.
        if let Some(idx) = *self.pending_meta_target.borrow() {
            if let Some(snippet) = self.snippet(idx) {
                if self.pending_body.borrow().is_none() {
                    return Some(snippet.preview_lines(width, wrap, show_meta));
                }
            }
        }
        // While Ctrl+N is collecting a title, show what is about to be
        // created: the body comes from the current selection.
        let body = self.pending_body.borrow().clone()?;
        let title = if draft_title.trim().is_empty() {
            "(new snippet)".to_string()
        } else {
            draft_title.trim().to_string()
        };
        let body = body.trim_end().to_string();
        let draft = Snippet {
            abs_path: PathBuf::new(),
            rel: "inbox/<new>.snip".to_string(),
            title,
            desc: String::new(),
            tags: vec![],
            kind: "command".to_string(),
            danger: false,
            pin: false,
            display_text: body.clone(),
            text: body.clone(),
            chars_after: None,
            is_multiline: body.contains('\n'),
        };
        let mut lines = draft.preview_lines(width, wrap, true);
        if body.trim().is_empty() {
            lines.push(vec![Span::dim(
                "(no selection - the snippet body will be empty)",
            )]);
        }
        Some(lines)
    }
}

/// Exposed for tests / other consumers that want to reuse the parser.
#[cfg(test)]
pub fn front_matter_keys_are_known(body: &str) -> bool {
    split_front_matter(body).0.has_known_key()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_front_matter() {
        let body = "---\ntitle: 查看 pod\ndesc: 宽格式\ntags: [k8s, pod]\nkind: command\ndanger: true\npin: yes\n---\nkubectl get pod\n";
        let (meta, text) = split_front_matter(body);
        assert_eq!(meta.title.as_deref(), Some("查看 pod"));
        assert_eq!(meta.desc.as_deref(), Some("宽格式"));
        assert_eq!(meta.tags, vec!["k8s", "pod"]);
        assert_eq!(meta.kind.as_deref(), Some("command"));
        assert_eq!(meta.danger, Some(true));
        assert_eq!(meta.pin, Some(true));
        assert_eq!(text.trim(), "kubectl get pod");
    }

    #[test]
    fn no_front_matter_keeps_whole_body() {
        let body = "ss -lntp | grep :80\n";
        let (meta, text) = split_front_matter(body);
        assert!(!meta.has_known_key());
        assert_eq!(text, body);
    }

    #[test]
    fn k8s_yaml_starting_with_dashes_is_not_front_matter() {
        let body = "---\napiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: x\n";
        let (meta, text) = split_front_matter(body);
        // `kind:` is one of our keys, but it needs a closing `---` too; there
        // is none here, so the whole file stays as body.
        assert!(!meta.has_known_key());
        assert_eq!(text, body);
    }

    #[test]
    fn update_keeps_unknown_keys_and_comments() {
        let body = "---\n# keep me\ntitle: old\ncustom: value\n---\nkubectl get pod\n";
        let updated = set_front_matter_key(body, "pin", "true");
        assert!(updated.contains("# keep me"));
        assert!(updated.contains("custom: value"));
        assert!(updated.contains("pin: true"));
        assert!(updated.contains("title: old"));
        assert!(updated.ends_with("kubectl get pod\n"));
    }

    #[test]
    fn adds_front_matter_when_absent() {
        let updated = set_front_matter_key("echo hi\n", "title", "\"hi\"");
        assert_eq!(updated, "---\ntitle: \"hi\"\n---\necho hi\n");
    }

    #[test]
    fn placeholder_positions() {
        let (text, display, after) = extract_placeholder("kubectl -n $|$ get pod -o wide");
        assert_eq!(text, "kubectl -n  get pod -o wide");
        assert_eq!(display, "kubectl -n $|$ get pod -o wide");
        assert_eq!(after, Some(" get pod -o wide".chars().count()));

        // A bare `?` is ordinary text now, so prompt strings survive.
        let (text, _, after) = extract_placeholder("echo $?");
        assert_eq!(text, "echo $?");
        assert_eq!(after, None);

        let (text, _, after) = extract_placeholder("no placeholder here");
        assert_eq!(text, "no placeholder here");
        assert_eq!(after, None);

        let (text, _, after) = extract_placeholder("$|$tail");
        assert_eq!(text, "tail");
        assert_eq!(after, Some(4));
    }

    #[test]
    fn multi_line_has_no_placeholder() {
        let (text, _, after) = extract_placeholder("line1\n$|$line2");
        assert_eq!(text, "line1\n$|$line2");
        assert_eq!(after, None);
    }

    #[test]
    fn filters_by_tag_dir_kind_danger_and_body() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("k8s")).unwrap();
        std::fs::write(
            dir.path().join("k8s/pod.snip"),
            "---\ntitle: pod\ntags: [k8s, debug]\n---\nkubectl -n ? get pod\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("k8s/deploy.yaml"),
            "---\ntitle: deploy\nkind: config\n---\napiVersion: v1\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("danger.snip"),
            "---\ntitle: boom\ndanger: true\n---\ndrop table users;\n",
        )
        .unwrap();

        let source = SnippetSource::new(dir.path().to_path_buf(), true, true);
        let titles = |q: &str| {
            let snippets = source.snippets.borrow();
            source
                .filter(q)
                .into_iter()
                .map(|i| snippets[i].title.clone())
                .collect::<Vec<_>>()
        };

        assert_eq!(source.total(), 3);
        assert_eq!(titles("#debug"), vec!["pod"]);
        assert_eq!(titles("@k8s"), vec!["deploy", "pod"]);
        assert_eq!(titles("kind:config"), vec!["deploy"]);
        assert_eq!(titles("!"), vec!["boom"]);
        // Body searches are single tokens: whitespace splits the query.
        assert_eq!(titles("/drop"), vec!["boom"]);
        assert_eq!(titles("#debug !").len(), 0);
    }

    #[test]
    fn trash_moves_file_and_rescans() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.snip"), "echo hi\n").unwrap();
        let source = SnippetSource::new(dir.path().to_path_buf(), true, true);
        assert_eq!(source.total(), 1);

        let to = source.trash("a.snip").unwrap();
        assert!(to.starts_with(dir.path().join(".trash")));
        assert!(to.exists());
        assert!(!dir.path().join("a.snip").exists());
        // hidden directory is not scanned
        assert_eq!(source.total(), 0);
    }
}
