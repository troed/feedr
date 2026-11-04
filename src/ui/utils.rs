use ratatui::layout::Rect;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

// Helper function to create a centered rect with minimum dimensions
pub(crate) fn centered_rect_with_min(
    percent_x: u16,
    percent_y: u16,
    min_w: u16,
    min_h: u16,
    r: Rect,
) -> Rect {
    let pct_w = r.width * percent_x / 100;
    let pct_h = r.height * percent_y / 100;
    let w = pct_w.max(min_w).min(r.width);
    let h = pct_h.max(min_h).min(r.height);
    let x = r.x + (r.width.saturating_sub(w)) / 2;
    let y = r.y + (r.height.saturating_sub(h)) / 2;
    Rect::new(x, y, w, h)
}

// Helper function to truncate a URL for display
pub(crate) fn truncate_url(url: &str, max_length: usize) -> String {
    // Remove common prefixes for cleaner display
    let clean_url = url
        .replace("https://", "")
        .replace("http://", "")
        .replace("www.", "");

    truncate_str(&clean_url, max_length)
}

// Helper function to truncate a string with unicode awareness
pub(crate) fn truncate_str(s: &str, max_chars: usize) -> String {
    if s.width() <= max_chars {
        return s.to_string();
    }
    // No room for anything but the ellipsis (three columns of it).
    if max_chars <= 3 {
        return ".".repeat(max_chars);
    }
    // Walk characters, never bytes: `cut` only ever lands on a char
    // boundary, so a wide first character can't produce a mid-slice panic.
    let budget = max_chars - 3;
    let mut total_width = 0;
    let mut cut = 0;
    for (idx, c) in s.char_indices() {
        let char_width = c.width_cjk().unwrap_or(1);
        if total_width + char_width > budget {
            break;
        }
        total_width += char_width;
        cut = idx + c.len_utf8();
    }
    format!("{}...", &s[..cut])
}

// Helper function to format content for better reading experience
pub(crate) fn format_content_for_reading(text: &str) -> String {
    let mut formatted_lines = Vec::new();
    let mut current_paragraph = Vec::new();
    let mut in_list = false;

    for line in text.lines() {
        let trimmed = line.trim();

        // Detect list items (lines starting with -, *, •, numbers, etc.)
        let is_list_item = trimmed.starts_with('-')
            || trimmed.starts_with('*')
            || trimmed.starts_with('•')
            || trimmed.starts_with("  - ")
            || trimmed.starts_with("  * ")
            || (trimmed.len() > 2
                && trimmed.chars().next().unwrap_or(' ').is_ascii_digit()
                && trimmed.chars().nth(1) == Some('.'));

        if trimmed.is_empty() {
            // Empty line - end current paragraph
            if !current_paragraph.is_empty() {
                formatted_lines.push(current_paragraph.join(" "));
                current_paragraph.clear();
                formatted_lines.push(String::new()); // Add spacing between paragraphs
                in_list = false;
            }
        } else if is_list_item {
            // List item - preserve as its own line
            if !current_paragraph.is_empty() {
                formatted_lines.push(current_paragraph.join(" "));
                current_paragraph.clear();
            }
            formatted_lines.push(format!("  {}", trimmed));
            in_list = true;
        } else if in_list && trimmed.starts_with("  ") {
            // Continuation of list item
            formatted_lines.push(format!("    {}", trimmed.trim()));
        } else {
            // Regular text - accumulate into current paragraph
            if in_list && !current_paragraph.is_empty() {
                // Starting new paragraph after list
                formatted_lines.push(String::new());
                in_list = false;
            }
            current_paragraph.push(trimmed.to_string());
        }
    }

    // Add any remaining paragraph
    if !current_paragraph.is_empty() {
        formatted_lines.push(current_paragraph.join(" "));
    }

    // Clean up excessive empty lines (max 2 in a row becomes 1)
    let mut result = Vec::new();
    let mut empty_count = 0;

    for line in formatted_lines {
        if line.is_empty() {
            empty_count += 1;
            if empty_count <= 1 {
                result.push(line);
            }
        } else {
            empty_count = 0;
            result.push(line);
        }
    }

    result.join("\n")
}

// Helper function to count the number of lines when text is wrapped
pub(crate) fn count_wrapped_lines(text: &str, width: usize) -> u16 {
    if width == 0 {
        return 0;
    }

    let mut line_count = 0u16;

    for line in text.lines() {
        if line.is_empty() {
            // Empty lines still count as one line
            line_count = line_count.saturating_add(1);
        } else {
            // Calculate how many wrapped lines this line will take
            let line_width = line.width();
            if line_width == 0 {
                line_count = line_count.saturating_add(1);
            } else {
                let wrapped_lines = line_width.div_ceil(width).max(1);
                line_count = line_count.saturating_add(wrapped_lines as u16);
            }
        }
    }

    // If text doesn't end with newline, we still have the lines we counted
    // If text is empty, return at least 1 line
    line_count.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_str_wide_first_char_at_tiny_width_stays_on_char_boundaries() {
        // '○' is three bytes wide and two columns wide, so a 4-column
        // budget leaves room only for the ellipsis.
        assert_eq!(truncate_str("○ Unread", 4), "...");
        assert_eq!(truncate_str("記事ABC", 4), "...");
    }

    #[test]
    fn truncate_str_never_exceeds_the_requested_display_width() {
        for max_chars in 0..=40 {
            let out = truncate_str("記事 evening ○ wide", max_chars);
            assert!(
                out.width() <= max_chars,
                "max {max_chars} produced {:?} ({} columns)",
                out,
                out.width()
            );
        }
    }
}
