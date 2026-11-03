# Inline article images via Kitty Unicode placeholders

Date: 2026-11-03

## Intent

Feedr today renders at most one image per article: the first `<media:thumbnail>` or the first inline `<img>`, in a fixed 10-row strip above the body. Users reading image-heavy feeds (blogs, comics, screenshot-heavy posts) want the images where the author put them, in the reading flow, with the text continuing underneath.

This design replaces the strip with **every block-level image in the article body rendered inline at its position**, scrolling with the text, sized to the content column.

Approach selected: **Kitty Unicode placeholders (`U+10EEEE` virtual placements)**. The alternatives (reserved rows + per-frame placements; full block-layout rewrite) were considered and rejected (see Approaches).

## Goals

- Every `<img>` in the item body that sits on its own (figure / image-only paragraph / gallery cell) renders inline at its position in feed order.
- Images resize with the pane, scroll with the text, and behave correctly when partially visible.
- Modals, search highlights, and the scrollbar draw over/around images without special-casing image z-order.
- Background fetching with bounded memory, bandwidth, and concurrency; existing SSRF, byte-size and decode guards preserved.
- Existing reading, scrolling and in-article search behaviour is unchanged.
- Works on Kitty (>= 0.28) and Ghostty. Unsupported terminals (WezTerm, tmux, anything else) show the image's alt text in place of the image.

## Non-goals (v1)

- Images inline **inside a sentence** (e.g. an icon between words). Those keep the current `[alt]` text.
- More than one placement of the same URL (duplicates are deduped; first occurrence wins).
- Images in the Readability full-text view. `ExtractedArticle` stores plain text today; retaining and rendering `article.content` is a follow-up.
- Feed-level media that is not in the body: `<media:content>`, `<enclosure>`, `media:thumbnail` are not appended. (They remain available to `%m` / `%M` / `%i` macros.)
- tmux passthrough, iTerm2, Sixel, half-block fallbacks.
- Image zoom/lightbox or an "open image" action. Opening the article in the browser remains.
- Animated image playback (first frame only, as today).
- A config toggle. Inline images are always on where the terminal supports them.

## Constraints

- ratatui 0.23 + crossterm 0.27. `Paragraph`'s internal wrapper exposes no line/row positions, and the detail body is currently a single `Paragraph` with `.wrap()` + `.scroll()`.
- `html2text` flattens every `<img>` to decorative text (`CleanDecorator::decorate_image`), so image positions do not survive the existing body pipeline.
- Kitty Unicode placeholders need: a virtual placement (`a=p,U=1,i=<id>,c=<cols>,r=<rows>`) plus buffer cells containing `U+10EEEE` followed by the row/column diacritics, with the image id encoded in the foreground colour.
- WezTerm does **not** implement `U=1` placeholders (upstream PR open as of 2026-10). Ghostty has since 2024. Kitty since 0.28.
- Existing image security posture must be kept: `feed::is_safe_auto_url`, `Feed::build_safe_redirect_client`, `MAX_IMAGE_BYTES = 5 MB`, `image::Limits` decode caps.
- `FeedItem` is serialized to disk. Any derived layout data must be either recomputable on load (`#[serde(skip)]`) or cheap to serialize.
- `%i` / `%m` / `%M` semantics and the `thumbnail` field stay unchanged.

## Approaches considered

### A. Reserved rows + per-frame Kitty placements

Keep the text pane, pre-wrap the body, replace each block image with N blank rows, and emit real Kitty placements after every draw at `body_origin + (image_row - scroll)`. Partially visible tall images require source-rectangle cropping, which needs cell pixel size.

- Pros: one engine; works on Kitty/Ghostty/WezTerm as today; smallest structural change.
- Cons: per-frame placement bookkeeping, scroll clipping/cropping, stale placement deletion, modal z-order special case, cell-pixel-size query, and Ghostty's historical "c/r placements stick while scrolling" bug class.

### B. Kitty Unicode placeholders — **selected**

Image rows are ordinary terminal cells containing `U+10EEEE` + diacritics. The terminal owns scroll and clipping.

- Pros: no per-frame placement math, no source-rect crop, no cell pixel size, no modal special case; partial visibility and resize are free.
- Cons: Kitty/Ghostty only; per-cell diacritic/colour encoding; virtual-placement lifecycle; WezTerm falls back to alt text.

### C. Full block layout engine

Parse the article into blocks and render each block as its own widget.

- Pros: exact control; natural home for inline images later.
- Cons: rewrites the detail view, search mapping, scrolling and full-text rendering; 2–3x the work and high regression risk.

Decision: **B**, accepting that WezTerm users get alt text until WezTerm implements placeholders.

## Design

### 1. Image discovery and block classification

New function in `src/feed.rs`, e.g.:

```rust
pub(crate) struct BodyImage {
    pub url: String,
    pub alt: String,
    pub declared_w: Option<u32>,
    pub declared_h: Option<u32>,
}

pub(crate) struct PreparedBody {
    /// HTML with block images replaced by sentinels.
    pub html: String,
    pub images: Vec<BodyImage>,
}

pub(crate) fn prepare_body_html(html: &str, base_url: Option<&str>) -> PreparedBody;
```

It is called on the **flattened** description (output of `flatten_description_html`), per the pipeline order in §2.

- Parse the fragment with `scraper` (already a dependency). Select `img[src]` in document order.
- Resolve the URL: `src`; if missing or `data:`/`javascript:`, try `data-src` or `data-lazy-src`. Resolve relative URLs against the item link. Keep only `http(s)`.
- Skip: declared `width`/`height` <= 2 (tracking pixels), empty URL, URLs whose path ends in `.svg` (the `image` crate cannot decode SVG; Kitty does not decode it either), duplicates after resolution.
- **Block predicate**: let `P` be the nearest ancestor of the `<img>` in `{p, figure, div, li, td, th, blockquote}`. The image is block-level iff every non-whitespace text node and non-`<img>` media descendant of `P` is inside `<figcaption>`. In other words: the paragraph/figure contains only images, links around those images, and an optional caption. Multiple images in one `P` are all block-level (gallery).
- Block images are recorded in `images` in document order. Their `<img ...>` tags are replaced in the HTML string by a sentinel token on its own paragraph: `<p>\u{E000}<index>\u{E001}</p>`. Classification comes from the DOM parse; replacement is a single left-to-right tag-walk over the raw string using the same document order (both traversals yield the same ordinal). Inline images are left untouched, so `CleanDecorator` continues to emit `[alt]`.
- Cap: at most 40 block images per article; beyond that, leave them as alt text.

Note: extraction runs once per item, lazily, and is cached (see §5). It is not per-frame work.

### 2. Body text pipeline

The detail body keeps a **single** model for summary and full-text. The only difference is the source: summary bodies run image discovery; full-text bodies are plain text (`BodySource::Extracted`) and therefore contain no image rows. `BodySource::Extracting` / `Failed` banners are prepended to the summary body *before* layout.

Pipeline order (pinned, because classification and sentinel ordinals must agree):

1. **Flatten tables first**: `flatten_description_html(desc)` (unchanged). It drops `table/tbody/thead/tfoot` wrappers, turns `td/th` into spaces and `tr` into `<br><br>`, and leaves `<img>` tags intact. This preserves today's Reddit behaviour.
2. **Classify and replace on the flattened string**: `prepare_body_html(&flattened, base)` parses that same string with `scraper`, classifies images, and replaces block-image tags in that same string. Both traversals see one string, so ordinals align by construction.
3. Render with `html2text` (`CleanDecorator`) at a very wide width (e.g. 10_000) so html2text does not hard-wrap paragraphs. It still produces paragraph breaks, list markers, headings, code blocks and `<br>`.
4. A sentinel-aware normaliser collapses blank lines as `format_content_for_reading` does today, preserving sentinel tokens. If a line contains a sentinel plus text (e.g. a `<figcaption>`), split it into: text before, image, text after.
5. Pre-wrap text lines at the real content width with a Unicode-width-aware word wrapper. Each resulting row is one screen row. The wrapper must break long words and preserve explicit blank lines.

```rust
enum RowKind {
    Text { line: usize, start: usize, end: usize }, // byte range within logical line
    Image(usize),                                   // index into PreparedBody::images
}

struct BodyLayout {
    rows: Vec<RowKind>,
    /// Unwrapped logical lines, exactly as today's search sees them.
    logical_lines: Vec<String>,
}

impl BodyLayout {
    /// Exact display row of each logical line's first screen row, given the
    /// display height of every image block. Recomputed when an image loads
    /// or the terminal resizes.
    fn row_of_line(&self, image_rows: &[u16]) -> Vec<u16>;
}
```

- `Paragraph` is rendered **without** `.wrap()`; each `Line` is one row. Scroll is `detail_vertical_scroll` rows, unchanged.
- Image rows expand to N display rows at render time (§3) and have no logical line.
- Search still runs on `logical_lines` (joined with `\n`), so matches keep today's semantics, including matches that straddle a soft-wrap boundary. `row_of_line` (built from the pre-wrap, so exact) replaces the `div_ceil` estimate in `wrapped_row_of_line`.
- When building display rows, a text row styles the intersection of any match range with `[start, end)`, so one logical-line match can highlight across several rows.

The `plain_text` cache (`FeedItem::plain_text`, used by pipes and content filters) is unchanged and keeps the `[alt]` render.

### 3. Sizing and placeholder cells

- Content width = current detail content area width (post-border, post-padding), as today.
- Rows for an image:
  `rows = round(cell_aspect * content_cols * img_h / img_w)`, `cell_aspect = 0.5` (the existing `CELL_ASPECT` heuristic), clamped to `1..=60`.
- Before an image is decoded: if the HTML declared both width and height, use them for the same calculation; otherwise reserve `DEFAULT_ROWS = 12`. When the real dimensions arrive, the row count updates and the body reflows (scroll is clamped). This is the only intentional layout jump.
- After decode: create/update the virtual placement with `c = content_cols, r = rows`. With both `c` and `r` specified, Kitty letterboxes rather than distorts, so a bad aspect estimate only adds bars.
- Placeholder row for image `i` at image row `y`:

  for each cell `x in 0..content_cols`:
  `symbol = "\u{10EEEE}" + diacritic(y) + diacritic(x)`
  style foreground = `Color::Rgb(id >> 16, id >> 8 & 0xFF, id & 0xFF)`.

  The 256-entry diacritic table is Kitty's `gen/rowcolumn-diacritics.txt` (Unicode 6.0 combining class 230). Vendor it into the source as a `[char; 256]` with a comment naming the source and version. One virtual placement per image, `p=1`, so a resize updates it instead of stacking.
- No underline colour is needed because there is exactly one placement per image id.
- 24-bit ids: `next_kitty_id` already wraps in `1..=0x00FF_FFFF`; keep that.

### 4. Image cache and fetching

Rework `src/image.rs` around entries keyed by URL:

```rust
enum EntryState {
    InFlight,
    Ready {
        id: u32,
        png: Option<Arc<Vec<u8>>>, // taken on transmit
        transmitted: bool,
        src_w: u32,
        src_h: u32,
    },
    Failed,
}
```

- The worker decodes and re-encodes to PNG (downscaled to `MAX_SOURCE_DIM`), then sends `(url, Option<Encoded>)`. The `DynamicImage` is dropped in the worker; the main thread never holds decoded pixels.
- Transmission happens on the first frame that needs the image: existing chunked `a=t,f=100,t=d,i=ID,q=2,m=...` sequence, then `a=p,U=1,i=ID,p=1,c=<cols>,r=<rows>,q=2`.
- **Transmit budget**: at most **one image per frame** and at most **4 MB of base64** per frame, whichever is hit first. An image that is decoded but not yet transmitted renders its loading placeholder (dim `[loading image: <alt>]` / blank rows) and is sent on the next tick. This bounds the worst case when a viewport prefetch makes several images ready at once, instead of writing tens of megabytes inside one draw.
- On resize / row change: re-emit the `a=p,U=1,...` placement with the new `c,r`; do not retransmit.
- Fetch policy: each frame, request images whose display block intersects the viewport expanded by one viewport height. Global concurrency cap stays 4. Known failures are cached for the session (no retry), as today.
- Cache cap: 32 URLs, LRU, **but never evict a URL referenced by the currently displayed article**. If the current article references more than 32, allow it to exceed the cap for the session (bounded by the 40-image article cap). On eviction, emit `a=d,d=I,i=ID,q=2` (uppercase `I`: free terminal pixel data as well).
- Security: unchanged `is_safe_auto_url` + safe-redirect client, `MAX_IMAGE_BYTES`, decode `Limits`.

### 5. Caching the prepared body

- Add to `FeedItem` (serde-skipped, recomputed on demand):
  `prepared_body: Option<PreparedBody>`.
- `prepare_body_html` runs on first detail render for an item after load and is stored in memory.
- `BodyLayout` (wrapped rows) depends on content width, so cache it on `App` keyed by `(feed, item, width, body generation)`. Recompute on width change, item change, or full-text toggle.
- Full-text (`BodySource::Extracted`) uses the same layout path with `logical_lines` from `format_content_for_reading(text)` and no image rows. There is no second renderer or second search/scroll path.

### 6. Detail rendering and search

- `ui/detail.rs` builds `Vec<Line>` from `BodyLayout`:
  - text lines get the existing match-highlight spans;
  - image lines get placeholder spans (loaded), dim `[loading image: <alt>]` on the first reserved row plus blank rows (loading), or a single `[alt]` line (unsupported terminal / failed fetch).
- `detail_max_scroll = display_rows - viewport_height`.
- Search:
  - `find_article_matches` scans `logical_lines.join("\n")`, so match semantics are unchanged (including matches across soft-wrap boundaries).
  - `scroll_to_current_article_match` uses `row_of_line(image_rows)[m.line]`; delete `wrapped_row_of_line` and `article_body_width_cache` if no other callers remain.
  - Match highlighting intersects each match's byte range with each display row's `[start, end)` range, so a match split across rows highlights on every row.
- Modals: no image-specific suppression needed. They are drawn after the body and cover the placeholder cells.
- Remove: `PendingImage`, `App::pending_image_render`, `App::last_frame_had_image`, `tui::emit_inline_image`, the `IMAGE_STRIP_ROWS` / `MIN_AREA_HEIGHT_FOR_IMAGE` layout branch, and their tests. `ImageCache` stays, restructured.
- Compact mode: no special-case. Images are in-flow; a short viewport simply scrolls.

### 7. Terminal detection and fallback

- `detect_protocol` returns `UnicodePlaceholders` for Kitty (`KITTY_WINDOW_ID`, `TERM=*kitty*`) and Ghostty (`TERM=*ghostty*`, `TERM_PROGRAM=ghostty`); `None` for everything else, including WezTerm and `$TMUX`.
- On `None`, image lines render as `[alt]` text; no fetching is attempted (same as today's no-op path).
- Do not implement runtime feature queries in v1 (Kitty's `a=q,e=` is not yet answered by Ghostty).
- Update README: inline images require Kitty >= 0.28 or Ghostty; WezTerm does not support Unicode placeholders yet and falls back to alt text. Remove the current WezTerm claim.

### 8. Testing

Unit tests:

- Block predicate: sole `<figure><img>`, `<p><a><img></a></p>`, `<p><img><figcaption>`, gallery of multiple images, `<p>text <img></p>` (inline), image with `data-src`, tiny/1x1, SVG, duplicate URLs.
- Sentinel replacement preserves document order and does not touch inline images.
- Sentinel-aware normaliser and wrapper: paragraph joining, explicit blank lines, long words, CJK width, exact line count at a given width; caption split.
- Diacritic table: values 0, 1, 255 map to the expected code points; placeholder symbol for (row, col) is correct.
- Row estimate: known dimensions, missing dimensions default, 60-row cap.
- Search alignment: a match before/after an image block maps to the correct display row.
- A match that straddles a soft-wrap boundary still matches and highlights on both rows.
- Flatten-then-classify: a Reddit-style `<table>` body keeps its flattened text and the image ordinal aligns with the replaced tag.
- Full-text bodies go through the same layout/search path with no image rows.
- Transmit budget: with several decoded images, only one is transmitted in a frame; the rest show the loading placeholder and go out on later frames.
- Unsupported terminal: image lines render `[alt]` and no fetch starts.

Integration tests (ratatui `TestBackend`):

- A loaded image produces the expected number of placeholder rows with `U+10EEEE` symbols and the image-id foreground colour.
- Scrolling changes which placeholder rows are in the buffer; the body text below still lines up.
- Opening a modal covers the placeholders (no placeholder cells in the modal area).
- Resize recomputes placeholder rows and re-emits a placement.
- WezTerm/`None` renders alt text only.
- Persisted-feed reload recomputes `prepared_body` (serde-skipped field).

Manual checks: Kitty and Ghostty; tall image (partial scroll), comic-width image, resize, search jump into/after an image, article switch, eviction of a non-current article.

### 9. Effort

Roughly **12–15 focused dev-days**, including tests:

| Piece | Days |
|---|---|
| Spike: sentinel round-trip + block predicate on the four feeds | 0.5 |
| Discovery/classification + prepared body cache | 2 |
| Sentinel-aware normaliser + pre-wrap + row/logical-line model | 3 |
| Image cache rewrite (PNG-only, virtual placements, transmit budget) | 2 |
| Detail rendering, search/scroll integration, full-text unification, strip removal | 2.5 |
| Tests, docs, polish | 2 |
| Contingency | 1 |

Main risks: exact wrapping/row parity; placeholder quirks in Ghostty; keeping search/scroll correct after removing ratatui's wrapper.

## Decisions made here (override on review)

- Body images only; feed-level media/thumbnails are not appended.
- Block-only; inline images remain `[alt]`.
- One body model for summary and full-text. Full-text bodies go through the same normaliser/wrapper/search path but have no image rows.
- Tables are flattened **before** image classification and sentinel replacement; both run on the flattened string.
- In-article search keeps logical-line semantics. A match may span soft-wrapped rows and highlights on each row; `row_of_line` is exact because we pre-wrap.
- At most one image and 4 MB of base64 transmitted per frame; decoded-but-unsent images show the loading placeholder until their turn.
- Full-text (Readability) images are a follow-up.
- Max image height 60 rows; unknown dimensions reserve 12 rows.
- Content-column width, letterboxed by the terminal when the aspect estimate is off.
- 40 images per article maximum; first occurrence of a duplicate URL wins.
- 32-image LRU, pinning the current article; terminal data freed on eviction.
- No config toggle, no zoom/lightbox, no tmux passthrough.
- Kitty >= 0.28 + Ghostty only; WezTerm/tmux fall back to alt text.

## Left to the builder

- Sentinel code points and the exact tag-walk implementation.
- Word-wrapper implementation (`textwrap` crate vs a small local width-aware wrapper).
- Whether `article_body_cache` is kept as a newline-joined string or replaced by `Vec<String>`.
- Exact LRU bookkeeping and how "current article" is pinned across view changes.
- Whether to keep the `thumbnail`-based strip code behind a flag for a transition period (the design removes it).
- Vendor the diacritic table as a source file or a generated const.
