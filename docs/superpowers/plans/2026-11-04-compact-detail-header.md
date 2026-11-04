# Compact Article Header Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Cut the article-view header from a fixed 9 rows to 3–4 rows in compact mode by dropping spacer lines and vertical padding and merging metadata with the link.

**Architecture:** `src/ui/detail.rs` builds the header `Paragraph` inline. Extract that into a pure `build_header_lines(...)` helper so the compact shape is unit-testable, add a pure `compact_header_height(...)` for the `Constraint::Length`, and branch the block padding on `app.compact`. Non-compact output stays byte-identical.

**Tech Stack:** Rust (MSRV 1.75), ratatui 0.23, `unicode-width` (already a dependency).

**Spec:** `docs/superpowers/specs/2026-11-04-compact-detail-header-design.md`

## Global Constraints

- MSRV 1.75.0 — nothing newer than 1.75 and no unstable APIs; prefer explicit `if` over bool-to-int conversion helpers.
- `cargo clippy --all-targets --all-features -- -D warnings` must pass; `cargo fmt --all -- --check` must pass.
- All cargo commands run through the sandbox wrapper: `.scratch/cargo.sh` (adds `--offline` where needed).
- Conventional commits (`feat(ui): ...`, `test(ui): ...`).
- Non-compact rendering must not change: 9-row header, blank spacers, separate link row.

## Review Focus

- Item with no author, no date and no link — header collapses to 3 rows, no empty inner row.
- Narrow terminal (e.g. 24×40, 24×20) — all width arithmetic saturates; no underflow panic.
- Very long title — truncated to one row, metadata row survives.
- Author/date long enough to fill the row — link must be dropped or truncated instead of wrapping the block and squeezing the body.
- CJK / wide-character title and author — widths use `unicode-width`, not char or byte counts.
- Non-compact path unchanged.

---

### Task 1: Slim the compact article header

**Files:**
- Modify: `src/ui/detail.rs` (header construction around lines 76–215, tests module at line ~564)
- Test: `src/ui/detail.rs` (inline `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `crate::ui::utils::truncate_str(&str, usize) -> String`, `crate::ui::ColorScheme` fields (`text`, `text_secondary`, `muted`, `secondary`, `primary`, `success`, `highlight`).
- Produces:
  - `fn compact_header_height(has_meta: bool, has_link: bool) -> u16` — returns `3` when `has_meta || has_link` is false, else `4`.
  - `fn build_header_lines(title: &str, read_status: Option<bool>, author: Option<&str>, date: Option<&str>, link: Option<&str>, compact: bool, inner_width: u16, colors: &ColorScheme) -> Vec<Line<'static>>` — `inner_width` is the text columns available inside the block (already excluding borders and horizontal padding).

- [ ] **Step 1: Write the failing tests**

Add to the existing `mod tests` in `src/ui/detail.rs` (reuse the `line_text` helper already there; import `UnicodeWidthStr` at the top of the test module for width assertions):

```rust
#[test]
fn compact_header_height_is_four_with_meta_or_link_and_three_without() {
    assert_eq!(compact_header_height(false, false), 3);
    assert_eq!(compact_header_height(true, false), 4);
    assert_eq!(compact_header_height(false, true), 4);
}

#[test]
fn compact_header_builds_title_then_meta_and_link_on_one_row() {
    let colors = ColorScheme::dark();
    let lines = build_header_lines(
        "Hello", Some(false), Some("Ada"), Some("Nov 4"),
        Some("https://example.com/a"), true, 60, &colors,
    );
    assert_eq!(lines.len(), 2);
    let second = line_text(&lines[1]);
    assert!(second.contains('Ada'), "got {second}");
    assert!(second.contains("Nov 4"), "got {second}");
    assert!(second.contains("example.com/a"), "got {second}");
    assert!(!second.contains('\n'));
}

#[test]
fn compact_header_collapses_to_one_line_without_meta_or_link() {
    let colors = ColorScheme::dark();
    let lines = build_header_lines("Hello", None, None, None, None, true, 60, &colors);
    assert_eq!(lines.len(), 1);
    assert_eq!(line_text(&lines[0]), "Hello");
}

#[test]
fn compact_header_truncates_long_title_to_inner_width() {
    let colors = ColorScheme::dark();
    let title = "A".repeat(200);
    let lines = build_header_lines(&title, None, None, None, None, true, 40, &colors);
    assert!(UnicodeWidthStr::width(line_text(&lines[0]).as_str()) <= 40);
}

#[test]
fn compact_header_never_exceeds_inner_width_when_meta_fills_the_row() {
    let colors = ColorScheme::dark();
    let author = "A".repeat(60);
    let lines = build_header_lines(
        "T", Some(false), Some(&author), Some("Nov 4"),
        Some("https://example.com/a/very/long/path/that/should/not/fit"), true, 40, &colors,
    );
    for line in &lines {
        assert!(
            UnicodeWidthStr::width(line_text(line).as_str()) <= 40,
            "line overflows: {}",
            line_text(line)
        );
    }
}

#[test]
fn compact_header_counts_wide_characters_by_display_width() {
    let colors = ColorScheme::dark();
    let title = "記事".repeat(40); // 80 display columns
    let lines = build_header_lines(&title, None, None, None, None, true, 40, &colors);
    assert!(UnicodeWidthStr::width(line_text(&lines[0]).as_str()) <= 40);
}

#[test]
fn non_compact_header_keeps_spacers_and_separate_link_row() {
    let colors = ColorScheme::dark();
    let lines = build_header_lines(
        "Hello", Some(true), Some("Ada"), Some("Nov 4"),
        Some("https://example.com/a"), false, 60, &colors,
    );
    assert_eq!(lines.len(), 5);
    assert_eq!(line_text(&lines[1]), "");
    assert!(line_text(&lines[3]).starts_with('\u{1F517}'));
    assert!(!line_text(&lines[2]).contains("example.com"));
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `.scratch/cargo.sh test --lib ui::detail --offline`
Expected: FAIL — `cannot find function compact_header_height`, `cannot find function build_header_lines`.

- [ ] **Step 3: Implement the two helpers**

In `src/ui/detail.rs`:

- `compact_header_height(has_meta: bool, has_link: bool) -> u16`: `if has_meta || has_link { 4 } else { 3 }`.
- `build_header_lines(...)`:
  - `compact == false`: reproduce today's five lines exactly — bold title from the raw string, `""`, separator-joined metadata (status, author, date) or `""` when the parts list is empty, `""`, then `🔗 ` + `truncate_url(link, 70)` when a link exists. Today the code pushes a blank spacer instead of the metadata line when there are no parts, and skips the link line entirely when there is no link — preserve both behaviours.
  - `compact == true`: line 1 is the bold title, `truncate_str(title, inner_width as usize)`. Build the second line from the metadata spans, tracking their display width; when a link exists and `inner_width - used - 3 (" · ") - 3 ("🔗 ")` is at least 8, append ` · `, `🔗 ` and `truncate_url(url, remaining)`. Append nothing else. Return one line if the metadata span list is empty and the link was not appended.
  - Separator (` · `) styling and per-part styling match today's code (`muted` separators, `success`/`highlight` status, italic `secondary` author, `text_secondary` date, underlined `primary` link).

- [ ] **Step 4: Run the tests to verify they pass**

Run: `.scratch/cargo.sh test --lib ui::detail --offline`
Expected: PASS (existing `build_styled_body` tests plus the seven new ones).

- [ ] **Step 5: Wire the helpers into the render path**

In `render_item_detail`:
- Before the layout split, take a short immutable borrow to derive the compact height:
  `has_meta = author.is_some() || formatted_date.is_some() || (selected_feed.is_some() && selected_item.is_some())`, `has_link = link.is_some()`; use `compact_header_height(has_meta, has_link)` when `app.compact`, else `9`. Comment that the borrow is short-lived so the later `&mut app` writes stay legal.
- Replace `Constraint::Length(9)` with the computed value.
- After the split (where the header is built), compute `inner_width = chunks[0].width.saturating_sub(2).saturating_sub(6)` (borders + 3 columns of horizontal padding each side) and call `build_header_lines(...)` instead of the inline `header_lines` construction.
- Block padding: `Padding::new(3, 3, if app.compact { 0 } else { 1 }, if app.compact { 0 } else { 1 })`.

- [ ] **Step 6: Add the layout regression test**

In `mod tests`, add a `TestBackend`-based test in `src/ui/detail.rs` (mirror the setup patterns in `src/tui.rs:944-1030`; use `ratatui::backend::TestBackend`, `Terminal`, and `crate::ui::render`):

```rust
#[test]
fn compact_detail_header_occupies_four_rows_and_leaves_more_body_rows() {
    // Build an App with one feed + one item having author and link, select it,
    // set app.view = View::FeedItemDetail, app.compact = true, and render into
    // TestBackend::new(80, 24).
    // Assert: the buffer's row 0 contains "Article" in the header band and the
    // header's bottom border row is <= 4, i.e. the first body row appears at
    // or above row 5 of the detail area (detail area starts at row 1 because
    // the compact title bar takes row 0).
}
```

Search for the top border row of the *second* block (the body, titled `Summary`/`Full-text`) and assert it is at most `1 + 4` — i.e. the header cost the detail view at most 4 rows. Do the same render with `app.compact = false` and assert the body's top border sits at `1 + 1 + 9` (outer margin 1 + header 9).

- [ ] **Step 7: Run the full check suite**

Run: `.scratch/cargo.sh fmt --all && .scratch/cargo.sh clippy --all-targets --all-features --offline -- -D warnings && .scratch/cargo.sh test --lib --offline`
Expected: fmt clean, clippy silent, all lib tests pass.

- [ ] **Step 8: Commit**

```bash
git add src/ui/detail.rs docs/superpowers/specs/2026-11-04-compact-detail-header-design.md docs/superpowers/plans/2026-11-04-compact-detail-header.md
git commit -m "feat(ui): slim the article header in compact mode"
```
