//! Kitty Unicode placeholder cells for inline image rows.
//!
//! A placeholder cell is `U+10EEEE` followed by a diacritic for the row and a
//! diacritic for the column; the image id is carried by the foreground colour.
//! The terminal owns scroll and clipping, so these are ordinary buffer cells.

use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};
use unicode_width::UnicodeWidthChar;

use super::rowcolumn_diacritics::ROWCOL_DIACRITICS;
use crate::ui::ColorScheme;

/// Rows reserved for an image whose dimensions are not known yet.
pub(crate) const DEFAULT_IMAGE_ROWS: u16 = 12;

/// Upper bound on the rows a single image may occupy.
pub(crate) const MAX_IMAGE_ROWS: u16 = 60;

/// Upper bound on the columns a placeholder image may occupy. The terminal
/// addresses a placeholder column with one diacritic byte (`min(255)` in the
/// table lookup), so wider images would alias and smear.
pub(crate) const MAX_PLACEHOLDER_COLS: u16 = 256;

/// Height of a terminal cell relative to its width (terminal cells are tall).
const CELL_ASPECT: f32 = 0.5;

/// Number of rows an image should occupy at `cols` content columns.
///
/// `dims` is the source pixel size; `None` reserves [`DEFAULT_IMAGE_ROWS`].
pub(crate) fn display_rows(dims: Option<(u32, u32)>, cols: u16) -> u16 {
    let Some((width, height)) = dims else {
        return DEFAULT_IMAGE_ROWS;
    };
    let cols = cols.min(MAX_PLACEHOLDER_COLS);
    if width == 0 || cols == 0 {
        return DEFAULT_IMAGE_ROWS;
    }
    let rows = CELL_ASPECT * f32::from(cols) * height as f32 / width as f32;
    (rows.round() as u16).clamp(1, MAX_IMAGE_ROWS)
}

/// Placeholder cells for an image covering `cols` x `rows` cells.
pub(crate) fn placeholder_lines(id: u32, cols: u16, rows: u16) -> Vec<Line<'static>> {
    let cols = cols.min(MAX_PLACEHOLDER_COLS);
    let fg = Style::default().fg(Color::Rgb(
        ((id >> 16) & 0xff) as u8,
        ((id >> 8) & 0xff) as u8,
        (id & 0xff) as u8,
    ));

    (0..rows)
        .map(|row| {
            let mut content = String::with_capacity(usize::from(cols) * 3);
            for col in 0..cols {
                content.push('\u{10EEEE}');
                content.push(ROWCOL_DIACRITICS[usize::from(row).min(255)]);
                content.push(ROWCOL_DIACRITICS[usize::from(col).min(255)]);
            }
            Line::from(vec![Span::styled(content, fg)])
        })
        .collect()
}

/// Loading placeholder: a dim caption on the first row, blank rows below.
pub(crate) fn loading_lines(
    alt: &str,
    cols: u16,
    rows: u16,
    colors: &ColorScheme,
) -> Vec<Line<'static>> {
    let cols = cols.min(MAX_PLACEHOLDER_COLS);
    let style = Style::default().fg(colors.muted);
    let caption = truncate_to_cols(&format!("[loading image: {alt}]"), cols);

    (0..rows)
        .map(|row| {
            let content = if row == 0 {
                caption.clone()
            } else {
                String::new()
            };
            Line::from(vec![Span::styled(content, style)])
        })
        .collect()
}

/// Alt-text fallback for terminals without placeholder support.
pub(crate) fn alt_lines(alt: &str, colors: &ColorScheme) -> Vec<Line<'static>> {
    let content = if alt.is_empty() {
        "[image]".to_string()
    } else {
        format!("[{alt}]")
    };
    vec![Line::from(vec![Span::styled(
        content,
        Style::default().fg(colors.muted),
    )])]
}

/// Truncate `text` so it occupies at most `cols` terminal columns.
fn truncate_to_cols(text: &str, cols: u16) -> String {
    let max = usize::from(cols);
    let mut out = String::new();
    let mut width = 0;
    for ch in text.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(1);
        if width + ch_width > max {
            break;
        }
        width += ch_width;
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::ColorScheme;

    #[test]
    fn placeholder_cells_encode_image_id_and_row_col() {
        let lines = placeholder_lines(0x010203, 2, 1);
        assert_eq!(lines.len(), 1);
        let span = &lines[0].spans[0];
        assert_eq!(
            span.content.as_ref(),
            "\u{10EEEE}\u{0305}\u{0305}\u{10EEEE}\u{0305}\u{030D}"
        );
        assert_eq!(span.style.fg, Some(Color::Rgb(1, 2, 3)));
    }

    #[test]
    fn display_rows_uses_aspect_default_and_cap() {
        assert_eq!(display_rows(Some((1600, 900)), 80), 23);
        assert_eq!(display_rows(None, 80), DEFAULT_IMAGE_ROWS);
        assert_eq!(display_rows(Some((100, 5000)), 80), MAX_IMAGE_ROWS);
    }

    #[test]
    fn placeholder_columns_are_clamped_to_the_diacritic_table() {
        let lines = placeholder_lines(1, 300, 1);
        assert_eq!(lines.len(), 1);
        let cells = lines[0].spans[0].content.matches('\u{10EEEE}').count();
        assert_eq!(cells, usize::from(MAX_PLACEHOLDER_COLS));

        // The row estimate must use the same clamped width, not 300.
        assert_eq!(
            display_rows(Some((1600, 400)), 300),
            display_rows(Some((1600, 400)), MAX_PLACEHOLDER_COLS)
        );
    }

    #[test]
    fn loading_and_alt_lines_have_the_right_height() {
        let colors = ColorScheme::dark();
        assert_eq!(loading_lines("A", 40, 3, &colors).len(), 3);
        assert_eq!(alt_lines("A", &colors).len(), 1);
    }
}
