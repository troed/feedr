# Slimmer article header in compact mode

Date: 2026-11-04

## Intent

In compact mode the article (detail) view still reserves a fixed **9-row** header
box for the title, a blank line, the metadata line, another blank line, and the
link. On a 24-row terminal that is 9 of the 22 rows available to the detail view,
leaving ~7 rows of article text once the body block's own border and padding are
paid for. Compact mode already strips the outer margin, shrinks the title and
help bars to one row each, hides the preview pane and the image strip — the
header is the last oversized piece of chrome.

Approach selected: **keep the bordered box, drop the spacer lines and vertical
padding, and merge metadata and link onto one row** (9 → 3–4 rows). The
alternatives (borderless 2-row header; keeping three separate content rows) were
offered and the bordered 4-row version was chosen.

## Goals

- Compact article view: header occupies at most 4 rows (2 border + title +
  metadata/link), 3 when the item has neither metadata nor a link.
- Gained rows go to the article body, not to empty space.
- Non-compact layout is **unchanged** (still a 9-row header).
- The header keeps its centred ` Article ` block title, border and horizontal
  padding, so the reading pane still has a visible top boundary.

## Non-goals

- Re-styling the non-compact header.
- Changing the body block's border/padding (it still costs 6 rows of chrome).
- Changing the image strip, search footer, or scroll math.

## Constraints

- ratatui 0.23, MSRV 1.75. Header height must be known *before* the vertical
  `Layout::split`, but the `item` borrow must stay after the `&mut` writes to
  `pending_image_render` / `image_cache`. So the compact height is derived from
  cheap presence flags (metadata present, link present) taken in a short-lived
  borrow, not by building the lines early.
- A long title must not push the metadata row out of a 2-row content area, so in
  compact mode the title is truncated to the header's inner width (unicode-width
  aware, the same `truncate_str` used for URLs).
- `truncate_url`'s fixed 70-column cap is too wide for narrow terminals; the
  link is truncated to whatever width the metadata row has left after the
  status/author/date spans.

## Decisions

- Header rows: `2 + 1 + (meta || link)` in compact, unchanged `9` otherwise.
- Compact block padding: horizontal 3 (as now), **vertical 0**.
- Compact content rows: title (bold, truncated), then one row of
  `status · author · date · 🔗 url`.
- The status (`✓ Read` / `○ Unread`), author, date and link are kept — nothing
  informational is dropped, only whitespace and the separate-row arrangement.
- Rendering code is untouched in shape: a pure `build_header_lines(...)` helper
  returns the lines so the compact shape is unit-testable without a
  `TestBackend`; the existing block construction is parametrised by mode.

## Testing

- Unit tests for `build_header_lines`: compact returns exactly 2 lines with no
  blank spacers; metadata and link share line 2; missing metadata/link collapse
  to a single line; link truncation respects the available width; a long title
  is truncated rather than wrapped.
- Manual check at 24×80 in compact mode: header is 4 rows and 5 more text rows
  are visible than before.
