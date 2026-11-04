//! Body layout: turn sentinel-marked body text into display rows.
//!
//! The detail body is pre-wrapped here instead of by `ratatui`'s `Paragraph`
//! wrapper so the renderer knows the exact display row of every logical line
//! (search and scroll need that) and so image blocks can reserve rows in the
//! flow. This module deliberately knows nothing about terminals or images
//! themselves: a recorded block image is just an index into the article's
//! image list.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use unicode_width::UnicodeWidthChar;

use crate::feed::{IMG_SENTINEL_END, IMG_SENTINEL_START};

/// Fingerprint of a resolved body source: its display text plus the number of
/// prepared image blocks.
///
/// Cached layouts compare this alongside their [`BodyKey`](crate::app::BodyKey)
/// so a change the key cannot see \u2014 a summary text replaced by a refresh, a
/// full-text extraction landing, or a `Ready \u2192 Failed` transition \u2014 still
/// invalidates the layout. Callers pass the text that is actually laid out, so
/// two sources that render identically share a layout.
pub fn body_hash(text: &str, image_count: usize) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    image_count.hash(&mut hasher);
    hasher.finish()
}

/// One display row of the body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowKind {
    /// A slice `start..end` (byte offsets) of `logical_lines[line]`.
    Text {
        line: usize,
        start: usize,
        end: usize,
    },
    /// A reserved image block; the index is into the article's image list.
    Image(usize),
}

/// Pre-wrapped body: display rows plus the unwrapped logical lines that
/// in-article search scans.
#[derive(Clone, Debug, Default)]
pub struct BodyLayout {
    pub rows: Vec<RowKind>,
    pub logical_lines: Vec<String>,
}

impl BodyLayout {
    /// Display row of the first screen row of each logical line.
    ///
    /// `image_rows[i]` is the display height of image block `i`; an unknown
    /// height falls back to one row. Recomputed when an image loads or the
    /// terminal resizes, which is why it takes the heights as an argument
    /// rather than caching them.
    pub fn row_of_line(&self, image_rows: &[u16]) -> Vec<u16> {
        let mut first_rows = vec![0u16; self.logical_lines.len()];
        let mut seen = vec![false; self.logical_lines.len()];
        let mut display_row: u16 = 0;

        for row in &self.rows {
            match *row {
                RowKind::Text { line, .. } => {
                    if let (Some(first), Some(seen)) =
                        (first_rows.get_mut(line), seen.get_mut(line))
                    {
                        if !*seen {
                            *seen = true;
                            *first = display_row;
                        }
                    }
                    display_row = display_row.saturating_add(1);
                }
                RowKind::Image(index) => {
                    let rows = image_rows.get(index).copied().unwrap_or(1);
                    display_row = display_row.saturating_add(rows);
                }
            }
        }

        first_rows
    }
}

/// Split one logical line into rows as byte ranges.
///
/// Rows are at most `width` terminal cells wide, measured with
/// `unicode_width` (so CJK characters count as two cells). A break at
/// whitespace consumes the whole whitespace run, so the whitespace belongs to
/// neither row. Words longer than a row are broken at the width, and a single
/// character wider than the row gets a row of its own. A width of 0 keeps the
/// whole line in one row; blank input yields one empty row.
pub(crate) fn wrap_line(line: &str, width: usize) -> Vec<(usize, usize)> {
    if width == 0 {
        return vec![(0, line.len())];
    }
    if line.is_empty() {
        return vec![(0, 0)];
    }

    let mut rows = Vec::new();
    let mut row_start = 0usize;
    let mut cells = 0usize;
    // Byte offset of the start of the trailing whitespace run, if the byte
    // before the current one was whitespace. Because it is reset on every
    // non-whitespace char, "still pending" at byte `idx` means `idx` is the
    // first byte after the run.
    let mut pending_space: Option<usize> = None;

    for (idx, ch) in line.char_indices() {
        let cell_width = ch.width().unwrap_or(0);

        if ch.is_whitespace() {
            if pending_space.is_none() {
                pending_space = Some(idx);
            }
            cells = cells.saturating_add(cell_width);
            continue;
        }

        if cell_width > 0 && cells.saturating_add(cell_width) > width && idx > row_start {
            match pending_space.take() {
                // Break at the last whitespace that fits and consume it.
                Some(space_start) if space_start > row_start => {
                    rows.push((row_start, space_start));
                }
                // The row is only leading whitespace: keep the indentation
                // with the first row rather than emitting an empty one.
                _ => rows.push((row_start, idx)),
            }
            row_start = idx;
            cells = 0;
        }

        if cell_width > width {
            // Wider than a whole row on its own: it cannot share with
            // anything, so give it its own row.
            rows.push((idx, idx + ch.len_utf8()));
            row_start = idx + ch.len_utf8();
            cells = 0;
            pending_space = None;
            continue;
        }

        pending_space = None;
        cells = cells.saturating_add(cell_width);
    }

    // Trailing whitespace is not part of any row.
    match pending_space {
        Some(space_start) if space_start > row_start => rows.push((row_start, space_start)),
        Some(_) => rows.push((row_start, row_start)),
        None => rows.push((row_start, line.len())),
    }

    rows
}

/// Sentinel text -> display rows and logical lines.
///
/// Every `\u{E000}<index>\u{E001}` token becomes an [`RowKind::Image`] row and
/// the text around it becomes its own logical line(s). Empty text segments are
/// dropped; runs of blank lines collapse to a single blank logical line;
/// leading and trailing blank lines are dropped. Lines without a token keep
/// their leading indentation.
pub fn layout_body(sentinel_text: &str, width: usize) -> BodyLayout {
    let mut layout = BodyLayout::default();
    // Set when a blank line was seen. Flushed as one blank logical line only
    // if more content follows, which collapses runs and drops trailing blanks.
    let mut pending_blank = false;

    for line in sentinel_text.split('\n') {
        let pieces = split_pieces(line);
        let has_image = pieces.iter().any(|piece| matches!(piece, Piece::Image(_)));

        if !has_image {
            let text = line.trim_end();
            if text.is_empty() {
                // Leading blanks are dropped too: nothing has been emitted.
                if !layout.logical_lines.is_empty() {
                    pending_blank = true;
                }
                continue;
            }
            if pending_blank {
                pending_blank = false;
                push_blank_line(&mut layout);
            }
            push_text_line(&mut layout, text, width);
            continue;
        }

        for piece in pieces {
            match piece {
                Piece::Image(index) => {
                    if pending_blank {
                        pending_blank = false;
                        push_blank_line(&mut layout);
                    }
                    layout.rows.push(RowKind::Image(index));
                }
                Piece::Text(text) => {
                    let text = text.trim_end();
                    if text.is_empty() {
                        continue;
                    }
                    if pending_blank {
                        pending_blank = false;
                        push_blank_line(&mut layout);
                    }
                    push_text_line(&mut layout, text, width);
                }
            }
        }
    }

    layout
}

/// A run of text or an image token within one physical line.
enum Piece<'a> {
    Text(&'a str),
    Image(usize),
}

/// Split one line around `\u{E000}<digits>\u{E001}` tokens.
///
/// A token whose body is not a valid `usize` is malformed and stays text. The
/// returned pieces always cover the whole line, and there is always at least
/// one piece.
fn split_pieces(line: &str) -> Vec<Piece<'_>> {
    let start_len = IMG_SENTINEL_START.len_utf8();
    let end_len = IMG_SENTINEL_END.len_utf8();
    let mut pieces = Vec::new();
    let mut text_start = 0usize;
    let mut scan = 0usize;

    while scan < line.len() {
        let Some(offset) = line[scan..].find(IMG_SENTINEL_START) else {
            break;
        };
        let token_start = scan + offset;
        let digits_start = token_start + start_len;
        let Some(offset) = line[digits_start..].find(IMG_SENTINEL_END) else {
            break;
        };
        let token_end = digits_start + offset;

        match line[digits_start..token_end].parse::<usize>() {
            Ok(index) => {
                pieces.push(Piece::Text(&line[text_start..token_start]));
                pieces.push(Piece::Image(index));
                text_start = token_end + end_len;
                scan = text_start;
            }
            // Malformed token: leave its characters in the text run and keep
            // scanning after the opening sentinel so we cannot loop forever.
            Err(_) => scan = digits_start,
        }
    }

    pieces.push(Piece::Text(&line[text_start..]));
    pieces
}

/// Append one blank logical line and its single empty row.
fn push_blank_line(layout: &mut BodyLayout) {
    let line = layout.logical_lines.len();
    layout.logical_lines.push(String::new());
    layout.rows.push(RowKind::Text {
        line,
        start: 0,
        end: 0,
    });
}

/// Append one logical line and wrap it into its display rows.
fn push_text_line(layout: &mut BodyLayout, text: &str, width: usize) {
    let line = layout.logical_lines.len();
    layout.logical_lines.push(text.to_string());
    for (start, end) in wrap_line(text, width) {
        layout.rows.push(RowKind::Text { line, start, end });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_line_breaks_on_words() {
        // Greedy wrapping: "brown fox" (9 cells) fits in the 10-cell row, so
        // this yields two rows. The plan's draft expected a third
        // `(16, 19)` row, which no greedy width-aware wrapper produces.
        assert_eq!(wrap_line("the quick brown fox", 10), vec![(0, 9), (10, 19)]);
    }

    #[test]
    fn wrap_line_breaks_long_words_and_respects_cjk_width() {
        assert_eq!(wrap_line("abcdefghij", 4), vec![(0, 4), (4, 8), (8, 10)]);
        assert_eq!(wrap_line("日本語", 4), vec![(0, 6), (6, 9)]);
    }

    #[test]
    fn wrap_line_keeps_blank_lines() {
        assert_eq!(wrap_line("", 10), vec![(0, 0)]);
    }

    #[test]
    fn wrap_line_with_zero_width_keeps_the_whole_line() {
        assert_eq!(wrap_line("abc", 0), vec![(0, 3)]);
    }

    #[test]
    fn layout_splits_caption_around_sentinel() {
        let layout = layout_body("\u{E000}0\u{E001}Caption", 80);
        assert_eq!(
            layout.rows,
            vec![
                RowKind::Image(0),
                RowKind::Text {
                    line: 0,
                    start: 0,
                    end: 7
                }
            ]
        );
        assert_eq!(layout.logical_lines, vec!["Caption".to_string()]);
    }

    #[test]
    fn layout_splits_text_on_both_sides_of_a_sentinel() {
        let layout = layout_body("before \u{E000}2\u{E001} after", 80);
        assert_eq!(layout.logical_lines, vec!["before", " after"]);
        assert_eq!(
            layout.rows,
            vec![
                RowKind::Text {
                    line: 0,
                    start: 0,
                    end: 6
                },
                RowKind::Image(2),
                RowKind::Text {
                    line: 1,
                    start: 0,
                    end: 6
                },
            ]
        );
    }

    #[test]
    fn layout_treats_a_malformed_sentinel_as_text() {
        let layout = layout_body("\u{E000}x\u{E001}tail", 80);
        assert_eq!(
            layout.logical_lines,
            vec!["\u{E000}x\u{E001}tail".to_string()]
        );
        assert!(layout
            .rows
            .iter()
            .all(|row| matches!(row, RowKind::Text { .. })));
    }

    #[test]
    fn layout_keeps_leading_indentation() {
        let layout = layout_body("    indented", 80);
        assert_eq!(layout.logical_lines, vec!["    indented"]);
        assert_eq!(
            layout.rows,
            vec![RowKind::Text {
                line: 0,
                start: 0,
                end: 12
            }]
        );
    }

    #[test]
    fn layout_collapses_blank_lines_and_maps_image_rows() {
        let layout = layout_body("alpha\n\n\nbeta", 80);
        assert_eq!(layout.logical_lines, vec!["alpha", "", "beta"]);
        assert_eq!(layout.row_of_line(&[]), vec![0, 1, 2]);
        assert_eq!(layout.row_of_line(&[12]), vec![0, 1, 2]);
    }

    #[test]
    fn row_of_line_accounts_for_image_blocks() {
        let layout = layout_body("alpha\n\u{E000}0\u{E001}\nbeta", 80);
        assert_eq!(layout.row_of_line(&[12]), vec![0, 13]);
    }

    #[test]
    fn body_hash_tracks_text_and_image_count() {
        let base = body_hash("alpha", 0);

        assert_eq!(base, body_hash("alpha", 0), "identical sources must match");
        assert_ne!(
            base,
            body_hash("beta", 0),
            "a replaced summary text must change the hash"
        );
        assert_ne!(
            base,
            body_hash("alpha", 1),
            "a changed image count must change the hash"
        );
    }
}
