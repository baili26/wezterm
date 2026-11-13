//! Runtime application of keyword highlighting rules.
//!
//! This is a personal-fork feature.  Rules live in the config and are applied
//! at paint time to a *clone* of the line that is about to be rendered, so the
//! terminal model (scrollback, copy/paste, search) is never modified.
use config::KeywordRule;
use termwiz::cell::{Intensity, Underline};
use termwiz::color::ColorAttribute;
use termwiz::surface::Line;

/// Recolor all cells in `line` that match one of `rules`.
///
/// Rules are applied in order, so a later rule wins over an earlier one for
/// cells that both match.  Fields that a rule does not set (`fg`/`bg` left as
/// `None`, or `bold`/`italic`/`underline` false) are left untouched, which
/// means style-only rules can be layered on top of color rules.
///
/// This only mutates cell attributes, never the cell text, so line content and
/// the byte offsets used for matching stay in sync.
pub fn apply_keyword_rules(rules: &[KeywordRule], line: &mut Line) {
    if rules.is_empty() || line.len() == 0 {
        return;
    }

    // `as_str()` borrows from the line for compressed lines, so take an owned
    // copy before we need a mutable borrow below.
    let text = line.as_str().into_owned();

    // Map byte offsets in `text` to cell indices.  `as_str()` concatenates
    // `visible_cells()` in order and skips the placeholder cells that follow
    // double-width graphemes, so these two walks line up exactly.
    struct Span {
        byte_start: usize,
        byte_end: usize,
        cell_index: usize,
        width: usize,
    }
    let mut spans = Vec::new();
    let mut offset = 0usize;
    for cell in line.visible_cells() {
        let len = cell.str().len();
        spans.push(Span {
            byte_start: offset,
            byte_end: offset + len,
            cell_index: cell.cell_index(),
            width: cell.width(),
        });
        offset += len;
    }
    if spans.is_empty() {
        return;
    }

    // (cell_index, rule_index) pairs to paint.  Collected first, then applied
    // in one pass, so the mutable borrow of the line happens once.
    let mut hits: Vec<(usize, usize)> = Vec::new();

    for (rule_idx, rule) in rules.iter().enumerate() {
        for found in rule.regex.find_iter(&text) {
            let m = match found {
                Ok(m) => m,
                // fancy_regex can fail at runtime (e.g. backtrack limit);
                // skip that match rather than aborting the whole paint.
                Err(_) => continue,
            };
            let (start, end) = (m.start(), m.end());
            if start >= end {
                // Zero-width match: nothing to paint.
                continue;
            }
            for span in &spans {
                if span.byte_end <= start || span.byte_start >= end {
                    continue;
                }
                for idx in span.cell_index..span.cell_index + span.width {
                    hits.push((idx, rule_idx));
                }
            }
        }
    }

    if hits.is_empty() {
        return;
    }

    // Later rules overwrite earlier ones for the same cell.
    hits.sort_by_key(|&(cell_idx, rule_idx)| (cell_idx, rule_idx));

    let cells = line.cells_mut_for_attr_changes_only();
    for (cell_idx, rule_idx) in hits {
        let Some(cell) = cells.get_mut(cell_idx) else {
            continue;
        };
        let rule = &rules[rule_idx];
        let attrs = cell.attrs_mut();
        if let Some(fg) = &rule.fg {
            attrs.set_foreground(ColorAttribute::TrueColorWithDefaultFallback((*fg).into()));
        }
        if let Some(bg) = &rule.bg {
            attrs.set_background(ColorAttribute::TrueColorWithDefaultFallback((*bg).into()));
        }
        if rule.bold {
            attrs.set_intensity(Intensity::Bold);
        }
        if rule.italic {
            attrs.set_italic(true);
        }
        if rule.underline {
            attrs.set_underline(Underline::Single);
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use termwiz::cell::CellAttributes;

    fn rule(regex: &str, fg: Option<&str>, bold: bool) -> KeywordRule {
        let mut r = KeywordRule::new(regex, fg).unwrap();
        if bold {
            r = r.with_bold();
        }
        r
    }

    fn line_of(text: &str) -> Line {
        Line::from_text(text, &CellAttributes::default(), 0, None)
    }

    fn painted_cells(line: &Line) -> Vec<usize> {
        line.visible_cells()
            .filter(|c| c.attrs().foreground() != ColorAttribute::Default)
            .map(|c| c.cell_index())
            .collect()
    }

    fn fg(s: &str) -> ColorAttribute {
        ColorAttribute::TrueColorWithDefaultFallback(s.parse().unwrap())
    }

    #[test]
    fn matches_only_the_keyword() {
        let mut line = line_of("an error occurred");
        apply_keyword_rules(&[rule(r"(?i)\berror\b", Some("#ff5f5f"), false)], &mut line);
        assert_eq!(painted_cells(&line), vec![3, 4, 5, 6, 7]);
        assert_eq!(line.as_str(), "an error occurred");
    }

    #[test]
    fn later_rule_wins() {
        let mut line = line_of("foo");
        apply_keyword_rules(
            &[
                rule("foo", Some("#ff0000"), false),
                rule("o", Some("#00ff00"), false),
            ],
            &mut line,
        );
        let colors: Vec<_> = line.visible_cells().map(|c| c.attrs().foreground()).collect();
        assert_eq!(colors[0], fg("#ff0000"));
        assert_eq!(colors[1], fg("#00ff00"));
        assert_eq!(colors[2], fg("#00ff00"));
    }

    #[test]
    fn wide_characters_and_emoji() {
        let text = "❤ 错误 😍";
        let mut line = line_of(text);
        apply_keyword_rules(&[rule("错误", Some("#ff5f5f"), false)], &mut line);

        // Only the two CJK graphemes should be painted -- not the surrounding
        // double-width emoji, and not the leading space.  (Indices depend on
        // how wide the emoji are assigned, so compare by grapheme text.)
        let painted_text: Vec<_> = line
            .visible_cells()
            .filter(|c| c.attrs().foreground() != ColorAttribute::Default)
            .map(|c| c.str().to_string())
            .collect();
        assert_eq!(painted_text, vec!["错".to_string(), "误".to_string()]);

        // The placeholder cell that follows each double-width grapheme must
        // also be painted, otherwise the second half of the glyph keeps the
        // old background/foreground.
        let cells = line.cells_mut_for_attr_changes_only();
        for cell in cells.iter() {
            if cell.str() == "错" || cell.str() == "误" {
                assert_eq!(cell.width(), 2);
            }
        }
        let painted_indices: Vec<usize> = line
            .visible_cells()
            .filter(|c| c.attrs().foreground() != ColorAttribute::Default)
            .map(|c| c.cell_index())
            .collect();
        for &idx in &painted_indices {
            // The cell right after a painted double-width grapheme is its
            // placeholder and should carry the same attributes.
            assert_eq!(
                line.cells_mut_for_attr_changes_only()[idx + 1]
                    .attrs()
                    .foreground(),
                fg("#ff5f5f"),
                "placeholder cell after index {idx} was not painted"
            );
        }

        // Text must be unchanged.
        assert_eq!(line.as_str(), text);
    }

    #[test]
    fn style_only_rule_keeps_colors() {
        let mut line = line_of("warning here");
        apply_keyword_rules(&[rule("warning", None, true)], &mut line);
        let first = line.visible_cells().next().unwrap();
        assert_eq!(first.attrs().foreground(), ColorAttribute::Default);
        assert_eq!(first.attrs().intensity(), Intensity::Bold);
    }

    #[test]
    fn no_match_and_empty_rules() {
        let mut line = line_of("all good");
        apply_keyword_rules(&[], &mut line);
        apply_keyword_rules(&[rule(r"\bdanger\b", Some("#ff0000"), false)], &mut line);
        assert!(painted_cells(&line).is_empty());
    }

    #[test]
    fn zero_width_matches_are_ignored() {
        let mut line = line_of("abc");
        apply_keyword_rules(&[rule(r"\b", Some("#ff0000"), false)], &mut line);
        assert!(painted_cells(&line).is_empty());
    }

    #[test]
    fn url_gets_underlined_only() {
        let mut line = line_of("see https://example.com/x for details");
        let rule = KeywordRule::new(r"https?://[^\s]+", None)
            .unwrap()
            .with_underline();
        apply_keyword_rules(&[rule], &mut line);
        let underlined: Vec<_> = line
            .visible_cells()
            .filter(|c| c.attrs().underline() != Underline::None)
            .map(|c| c.cell_index())
            .collect();
        let expected = "https://example.com/x";
        assert_eq!(underlined.len(), expected.len());
        assert_eq!(
            underlined.first().copied(),
            Some("see ".len()),
            "underline should start right after 'see '"
        );
    }
}
