# Inline Article Images Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Render every block-level image in an article body inline, at its position in the reading flow, using Kitty Unicode placeholders (`U+10EEEE`), replacing the single 10-row thumbnail strip.

**Architecture:** The body becomes an ordered list of text rows and image blocks. Block images are discovered in the flattened HTML and replaced with sentinel tokens; `html2text` renders the text; a pre-wrapper turns logical lines into display rows; image rows are terminal cells carrying `U+10EEEE` plus row/column diacritics, backed by Kitty virtual placements (`U=1`). The terminal owns scroll and clipping. Kitty and Ghostty render placeholders; WezTerm, tmux and other terminals fall back to the image's alt text.

**Tech Stack:** Rust 2021, ratatui 0.23, crossterm 0.27, html2text 0.6, scraper 0.18, image 0.25 (pinned), unicode-width 0.1. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-11-03-inline-images-design.md`

## Global Constraints

- Target terminals: Kitty >= 0.28 and Ghostty. WezTerm and tmux render `[alt]` and must not fetch images.
- Body `<img>` elements only. Do not append `<media:content>`, `<enclosure>` or `<media:thumbnail>` to the flow. `%m`, `%M` and `%i` keep their current behaviour.
- Block-level images only. Images inside a sentence stay `[alt]`.
- At most 40 block images per article; duplicate URLs after resolution are dropped (first occurrence wins).
- Max image height 60 rows. Unknown dimensions reserve 12 rows until decoded.
- At most 1 image and 4 MB of base64 transmitted per frame.
- 32-entry image LRU, pinning every URL referenced by the current article; evicted transmitted images free terminal data with `a=d,d=I,i=<id>,q=2`.
- Keep `is_safe_auto_url`, `Feed::build_safe_redirect_client`, `MAX_IMAGE_BYTES = 5 MB`, and the existing `image::Limits` decode caps.
- `FeedItem::thumbnail`, `%i`, `%m`, `%M` semantics are unchanged.
- `PreparedBody` is cache-only: `#[serde(skip)]`, recomputed after loading persisted feeds.
- Word-wrap width is terminal cells, not bytes or chars. Rust edition 2021, existing toolchain.

## Review Focus

These are the input classes most likely to bite a reader. Each appears again as a test in the task that owns the code.

1. **An image inside a sentence** must stay as `[alt]` text and must not become a block, lose surrounding text, or gain blank rows. (Task 1, Task 2, Task 9)
2. **A figure with a caption** must produce an image block followed by the caption text, in that order, not a merged line. (Task 1, Task 3)
3. **A gallery of several images in one container** must render every image in document order, each as its own block. (Task 1)
4. **A search phrase spanning a soft-wrap boundary** in an image-heavy article must still match, highlight on every row it spans, and `n`/`N` must scroll to the first row. (Task 3, Task 9)
5. **A lazy-loaded image (`data-src`) preceded by a 1x1 beacon** must pick the real image and skip the beacon. (Task 1)
6. **WezTerm/tmux** must show `[alt]`, produce no placeholder cells, and start no image fetches. (Task 7, Task 9)

---

## File Structure

Create:

- `src/body.rs` — sentinel text normalisation, word wrapping with byte ranges, `BodyLayout`, display-row mapping. No terminal or image code.
- `src/ui/placeholder.rs` — Kitty placeholder cell lines, loading/alt lines, display-row height calculation.
- `src/ui/rowcolumn_diacritics.rs` — generated `[char; 256]` table from Kitty's `gen/rowcolumn-diacritics.txt`.

Modify:

- `src/feed.rs` — `BodyImage`, `PreparedBody`, `prepare_body_html`, `PreparedBody::render_text`, `FeedItem::prepared_body` cache.
- `src/image.rs` — PNG-only cache entries, lazy fetch, transmit budget, virtual placements, eviction deletes.
- `src/app.rs` — body-layout cache and display-row map; remove strip-only fields.
- `src/ui/detail.rs` — render `BodyLayout` rows and image placeholders; search highlighting across wrapped rows; remove strip layout.
- `src/ui/mod.rs` — register the placeholder module.
- `src/tui.rs` — pump images before each draw; delete `emit_inline_image`.
- `src/lib.rs` — `pub mod body;`.
- `README.md`, `CLAUDE.md` — terminal support and limitations.

---

### Task 1: Block-image discovery and sentinel replacement

**Files:**
- Modify: `src/feed.rs` (new types and `prepare_body_html`, tests in the existing `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `flatten_description_html` output (string), `flatten_description_html` itself, `url::Url`.
- Produces:
  ```rust
  pub const IMG_SENTINEL_START: char = '\u{E000}';
  pub const IMG_SENTINEL_END: char = '\u{E001}';

  #[derive(Clone, Debug, PartialEq, Eq)]
  pub struct BodyImage {
      pub url: String,
      pub alt: String,
      pub declared_w: Option<u32>,
      pub declared_h: Option<u32>,
  }

  #[derive(Clone, Debug, Default)]
  pub struct PreparedBody {
      pub html: String,
      pub images: Vec<BodyImage>,
  }

  pub(crate) fn prepare_body_html(flattened: &str, base_url: Option<&str>) -> PreparedBody;
  ```

- [ ] **Step 1: Write the failing tests**

Add to `src/feed.rs` tests:

```rust
#[test]
fn prepare_body_html_keeps_inline_image() {
    let html = r#"<p>before <img src="https://ex.com/a.png" alt="a"> after</p>"#;
    let prepared = prepare_body_html(html, Some("https://ex.com/post"));
    assert!(prepared.images.is_empty());
    assert_eq!(prepared.html, html);
}

#[test]
fn prepare_body_html_replaces_figure_image_and_keeps_caption() {
    let html = r#"<figure><img src="/pics/a.png" alt="A"><figcaption>Cap</figcaption></figure>"#;
    let prepared = prepare_body_html(html, Some("https://ex.com/post"));
    assert_eq!(prepared.images.len(), 1);
    assert_eq!(prepared.images[0].url, "https://ex.com/pics/a.png");
    assert_eq!(prepared.images[0].alt, "A");
    assert_eq!(
        prepared.html,
        "<figure>\u{E000}0\u{E001}<figcaption>Cap</figcaption></figure>"
    );
}

#[test]
fn prepare_body_html_keeps_gallery_order() {
    let html = r#"<div><img src="a.png"><img src="b.png"><img src="c.png"></div>"#;
    let prepared = prepare_body_html(html, Some("https://ex.com/post/"));
    let urls: Vec<&str> = prepared.images.iter().map(|i| i.url.as_str()).collect();
    assert_eq!(
        urls,
        vec![
            "https://ex.com/post/a.png",
            "https://ex.com/post/b.png",
            "https://ex.com/post/c.png"
        ]
    );
    assert!(prepared.html.contains("\u{E000}0\u{E001}"));
    assert!(prepared.html.contains("\u{E000}1\u{E001}"));
    assert!(prepared.html.contains("\u{E000}2\u{E001}"));
}

#[test]
fn prepare_body_html_skips_beacon_and_uses_data_src() {
    let html = r#"<div><img src="https://ex.com/px.gif" width="1" height="1"><img src="data:image/png;base64,xx"><img data-src="https://ex.com/real.jpg" alt="real"></div>"#;
    let prepared = prepare_body_html(html, Some("https://ex.com/post"));
    assert_eq!(prepared.images.len(), 1);
    assert_eq!(prepared.images[0].url, "https://ex.com/real.jpg");
    assert_eq!(prepared.images[0].alt, "real");
}
```

Also add tests named:
- `prepare_body_html_link_wrapped_image_is_block` — `<p><a href="/x"><img src="/a.png"></a></p>` yields 1 image and a sentinel.
- `prepare_body_html_skips_svg` — a `.svg` URL yields no images and leaves the tag untouched.
- `prepare_body_html_dedupes_urls` — the same resolved URL twice yields 1 image and one sentinel; the second tag stays untouched.
- `prepare_body_html_requires_http_base_for_relative_src` — `src="a.png"` with `base_url = None` yields no images.
- `prepare_body_html_uses_declared_dimensions` — `width="640" height="480"` yields `declared_w == Some(640)`, `declared_h == Some(480)`.
- `prepare_body_html_flattened_table_keeps_ordinals` — flattened Reddit-style HTML with one image yields ordinal 0 and a sentinel at that position.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib prepare_body_html_`
Expected: FAIL with "cannot find function `prepare_body_html`".

- [ ] **Step 3: Implement discovery, classification and replacement in `src/feed.rs`**

Rules to implement:

- Parse `flattened` with `scraper::Html::parse_fragment`. Select every `img` element in document order (not `img[src]`, so `data-src` is visible).
- Resolve the URL: `src`; if missing, empty, or starting with `data:`/`javascript:`, use `data-src`, then `data-lazy-src`. Resolve relative URLs against `base_url` with `Url::parse(base)?.join(src)`. Keep only `http`/`https`.
- Skip: resolved URL whose path ends in `.svg` (case-insensitive); declared `width` or `height` <= 2 (parse with a trailing `px` trimmed); duplicate resolved URLs after the first.
- **Block predicate**: find the nearest ancestor in `{p, figure, div, li, td, th, blockquote}`. The image is block-level iff every descendant text node of that ancestor, excluding subtrees under `figcaption`, is whitespace-only. Multiple `img` descendants are allowed (gallery). If there is no such ancestor, the image is inline.
  `ElementRef` derefs to `ego_tree::NodeRef`, so recurse over `.children()` and match `scraper::node::Node::Text` / `Node::Element`.
- `PreparedBody.images` records only block images, in document order. `declared_w`/`declared_h` come from the HTML attributes.
- Replace the original `<img ...>` tag with `\u{E000}<index>\u{E001}` only when that image was recorded. Leave inline, skipped and filtered images untouched so `CleanDecorator` keeps `[alt]`.
- Replacement is a left-to-right walk over the raw string. Count every `<img` tag in the same document order as the selector, record an `Option<image_index>` per tag, then substitute by ordinal. Attributes are read from the raw tag for the src fallback and dimensions; classification comes from the DOM. The tag-end scan is quote-aware: a `>` inside an attribute value does not end the tag. An unterminated tag leaves the rest of the string verbatim.
- Cap `images` at 40; further block images are left as tags (alt text).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib prepare_body_html_`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/feed.rs
git commit -m "feat(feed): discover block images and mark them in body HTML"
```

---

### Task 2: Render prepared body text and cache it on FeedItem

**Files:**
- Modify: `src/feed.rs` (impl, `FeedItem` field, literals)
- Modify: `src/app.rs`, `src/events.rs`, `src/tui.rs` (only `FeedItem { ... }` literals the compiler reports)

**Interfaces:**
- Consumes: Task 1's `PreparedBody`, `BodyImage`; `CleanDecorator`.
- Produces:
  ```rust
  impl PreparedBody {
      /// html2text at width 10_000 with CleanDecorator; sentinels survive.
      pub(crate) fn render_text(&self) -> String;
  }

  // On FeedItem:
  #[serde(skip)]
  pub prepared_body: Option<PreparedBody>,

  impl FeedItem {
      /// Flatten, prepare and cache on first use. `base_url` is normally `self.link`.
      pub fn prepared_body(&mut self, base_url: Option<&str>) -> &PreparedBody;
  }
  ```

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn prepared_body_render_text_keeps_sentinels_and_inline_alt() {
    let html = r#"<p><img src="https://ex.com/a.png" alt="A"></p><p>text <img src="https://ex.com/b.png" alt="B"></p>"#;
    let prepared = prepare_body_html(html, Some("https://ex.com/post"));
    let text = prepared.render_text();
    assert!(text.contains('\u{E000}'));
    assert!(text.contains("[B]"));
    assert!(!text.contains("\u{E000}1\u{E001}"));
}

#[test]
fn feed_item_prepared_body_is_lazy_and_skipped_by_serde() {
    let mut item = /* build a FeedItem with description containing one figure image */;
    assert!(item.prepared_body.is_none());
    let prepared = item.prepared_body(Some("https://ex.com/post"));
    assert_eq!(prepared.images.len(), 1);
    let json = serde_json::to_string(&item).unwrap();
    assert!(!json.contains("prepared_body"));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib prepared_body_`
Expected: FAIL (method missing).

- [ ] **Step 3: Implement**

- `render_text` calls `html2text::from_read_with_decorator(self.html.as_bytes(), 10_000, CleanDecorator::new())`.
- Add `#[serde(skip)] pub prepared_body: Option<PreparedBody>` to `FeedItem` and set `prepared_body: None` in every literal the compiler reports.
- `FeedItem::prepared_body` computes `prepare_body_html(&flatten_description_html(desc), base_url)` once from `self.description` and stores it. `None` description yields an empty `PreparedBody`.
- Keep `plain_text` unchanged.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib prepared_body_`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/feed.rs src/app.rs src/events.rs src/tui.rs
git commit -m "feat(feed): render and cache prepared body text"
```

---

### Task 3: Body layout, sentinel parsing and word wrapping

**Files:**
- Create: `src/body.rs`
- Modify: `src/lib.rs` (`pub mod body;`)

**Interfaces:**
- Consumes: none beyond `unicode-width`.
- Produces:
  ```rust
  #[derive(Clone, Debug, PartialEq, Eq)]
  pub enum RowKind {
      Text { line: usize, start: usize, end: usize },
      Image(usize),
  }

  #[derive(Clone, Debug, Default)]
  pub struct BodyLayout {
      pub rows: Vec<RowKind>,
      pub logical_lines: Vec<String>,
  }

  impl BodyLayout {
      /// Display row of the first screen row of each logical line.
      pub fn row_of_line(&self, image_rows: &[u16]) -> Vec<u16>;
  }

  /// Split one logical line into rows as byte ranges. Whitespace at a break
  /// is consumed; width is terminal cells.
  pub(crate) fn wrap_line(line: &str, width: usize) -> Vec<(usize, usize)>;

  /// Sentinel text -> rows and logical lines.
  pub fn layout_body(sentinel_text: &str, width: usize) -> BodyLayout;
  ```

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn wrap_line_breaks_on_words() {
    assert_eq!(
        wrap_line("the quick brown fox", 10),
        vec![(0, 9), (10, 15), (16, 19)]
    );
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
fn layout_splits_caption_around_sentinel() {
    let layout = layout_body("\u{E000}0\u{E001}Caption", 80);
    assert_eq!(
        layout.rows,
        vec![
            RowKind::Image(0),
            RowKind::Text { line: 0, start: 0, end: 7 }
        ]
    );
    assert_eq!(layout.logical_lines, vec!["Caption".to_string()]);
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib body::tests::`
Expected: FAIL (module missing).

- [ ] **Step 3: Implement `src/body.rs`**

- `wrap_line`: iterate `char_indices`, track cell width with `unicode_width::UnicodeWidthChar`; break at the last whitespace that fits, consume that whitespace, treat a too-wide char as its own break point, break long words at the width, and return byte ranges. A width of 0 yields one `(0, line.len())` row. Blank input yields `[(0, 0)]`.
- `layout_body`: split the text on `'\n'`. Scan each line for sentinel tokens `\u{E000}<digits>\u{E001}`:
  - text before a token becomes a text segment;
  - the token becomes `RowKind::Image(index)`;
  - text after becomes another segment;
  - a line with no token is one text segment (possibly empty).
  Trim trailing whitespace from segments but keep leading indentation. Collapse runs of blank text segments to at most one blank logical line. Each non-empty text segment is appended to `logical_lines` and wrapped into `RowKind::Text` rows with its logical-line index. Each image token appends one `RowKind::Image(index)` row.
- `row_of_line`: walk rows, maintaining a display row counter. On the first `Text { line, .. }` for each line, record the counter. On `Image(i)`, add `image_rows.get(i).copied().unwrap_or(1)`. Text rows add 1.
- Reject sentinel indices not parsed as `usize`; treat the malformed token as text.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib body::tests::`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/body.rs src/lib.rs
git commit -m "feat(body): lay out text rows and image blocks"
```

---

### Task 4: Placeholder cells, loading rows and diacritics

**Files:**
- Create: `src/ui/placeholder.rs`
- Create: `src/ui/rowcolumn_diacritics.rs`
- Modify: `src/ui/mod.rs`

**Interfaces:**
- Consumes: `ratatui::{text::{Line, Span}, style::{Color, Style}}`, `crate::ui::ColorScheme`.
- Produces:
  ```rust
  pub(crate) const DEFAULT_IMAGE_ROWS: u16 = 12;
  pub(crate) const MAX_IMAGE_ROWS: u16 = 60;

  pub(crate) fn display_rows(dims: Option<(u32, u32)>, cols: u16) -> u16;
  pub(crate) fn placeholder_lines(id: u32, cols: u16, rows: u16) -> Vec<Line<'static>>;
  pub(crate) fn loading_lines(alt: &str, cols: u16, rows: u16, colors: &ColorScheme) -> Vec<Line<'static>>;
  pub(crate) fn alt_lines(alt: &str, colors: &ColorScheme) -> Vec<Line<'static>>;
  ```

- [ ] **Step 1: Generate the diacritic table**

Run:

```bash
curl -s https://raw.githubusercontent.com/kovidgoyal/kitty/master/gen/rowcolumn-diacritics.txt \
  | grep -E '^[0-9A-F]{4,6};' | head -n 256 | cut -d';' -f1 \
  | awk '{printf "    '\''\\u{%s}'\'',\n", $1}'
```

Paste the output into `src/ui/rowcolumn_diacritics.rs`:

```rust
//! Generated from kitty `gen/rowcolumn-diacritics.txt` (Unicode 6.0, combining class 230).
pub(crate) const ROWCOL_DIACRITICS: [char; 256] = [
    // ... 256 entries ...
];
```

Write a file, not a runtime fetch. The first entries must be `'\u{0305}'`, `'\u{030D}'`, `'\u{030E}'`.

- [ ] **Step 2: Write the failing tests**

```rust
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
fn loading_and_alt_lines_have_the_right_height() {
    let colors = ColorScheme::dark();
    assert_eq!(loading_lines("A", 40, 3, &colors).len(), 3);
    assert_eq!(alt_lines("A", &colors).len(), 1);
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test --lib ui::placeholder::tests::`
Expected: FAIL (module missing).

- [ ] **Step 4: Implement**

- `display_rows`: `rows = round(0.5 * cols * h / w)` with `CELL_ASPECT = 0.5`; clamp `1..=MAX_IMAGE_ROWS`; `None` returns `DEFAULT_IMAGE_ROWS`. Define `const CELL_ASPECT: f32 = 0.5` in `placeholder.rs`; the old constant in `image.rs` is removed with the strip code.
- `placeholder_lines`: one `Line` per row; each line is a single `Span` whose content is `content_cols` repetitions of `U+10EEEE + ROWCOL_DIACRITICS[row] + ROWCOL_DIACRITICS[col]`; style foreground `Color::Rgb((id >> 16) as u8, (id >> 8) as u8, id as u8)`.
- `loading_lines`: first row dim `[loading image: <alt>]` (truncated to `cols`), remaining rows blank; each `Line` has exactly one span.
- `alt_lines`: one dim line `[<alt>]` (or `[image]` when alt is empty), truncated to `cols`.
- Register `pub(crate) mod placeholder;` and `mod rowcolumn_diacritics;` in `src/ui/mod.rs`.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --lib ui::placeholder::tests::`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/ui/placeholder.rs src/ui/rowcolumn_diacritics.rs src/ui/mod.rs
git commit -m "feat(ui): render Kitty placeholder cells and image rows"
```

---

### Task 5: Image cache rewrite — PNG-only entries and lazy fetching

**Files:**
- Modify: `src/image.rs`

**Interfaces:**
- Consumes: existing `fetch_and_decode`, `downscale`, `encode_png`, `is_safe_auto_url`, `MAX_IMAGE_BYTES`, decode limits.
- Produces:
  ```rust
  pub struct ReadyImage {
      pub id: u32,
      pub width: u32,
      pub height: u32,
      pub transmitted: bool,
  }

  pub enum ImageView<'a> {
      Missing,
      Loading,
      Ready(&'a ReadyImage),
      Failed,
  }

  impl ImageCache {
      pub fn view(&self, url: &str) -> ImageView<'_>;
      pub fn ensure_fetches(&mut self, urls: &[String]);
      pub fn poll_completed(&mut self) -> bool;
      pub fn set_pinned(&mut self, urls: HashSet<String>);
  }
  ```

- [ ] **Step 1: Write the failing tests**

Add tests named:
- `view_reports_missing_loading_ready_failed` — absent URL → `Missing`; a test-injected in-flight entry → `Loading`; ready entry → `Ready` with dims/id; failed entry → `Failed`.
- `poll_completed_stores_encoded_png_and_dimensions` — a test-injected `EncodedImage` becomes `Ready`, stores the PNG, sets `transmitted == false`.
- `ensure_fetches_is_idempotent_for_known_urls` — calling it twice for a known-failed URL leaves one `Failed` entry and does not re-queue.
- `lru_never_evicts_pinned_urls` — fill past 32 with one URL pinned; the pinned URL remains.
- `lru_eviction_of_transmitted_image_queues_terminal_delete` — evict a transmitted ready entry; its id lands in the pending-delete queue.
- `start_fetch_noop_in_none_protocol` (existing, keep)
- `start_fetch_rejects_unsafe_urls` (existing, keep)

Do not use real network fetches in tests. Use the `ImageCache::insert_ready_for_test` and `ImageCache::insert_state_for_test` methods.

Use `#[cfg(test)]` methods on `ImageCache` so other modules' tests can use them:

```rust
#[cfg(test)]
impl ImageCache {
    pub(crate) fn insert_ready_for_test(&mut self, url: &str, id: u32, w: u32, h: u32, transmitted: bool);
    pub(crate) fn insert_state_for_test(&mut self, url: &str, state: TestState); // InFlight | Failed
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib image::tests::view_`
Expected: FAIL.

- [ ] **Step 3: Implement**

- Replace `images: HashMap<String, Option<Arc<DynamicImage>>>` with `entries: HashMap<String, Entry>`:
  ```rust
  enum Entry {
      InFlight,
      Ready { image: ReadyImage, png: Option<Arc<Vec<u8>>> },
      Failed,
  }
  ```
  `view` returns `Ready(&entry.image)`; `pump` and eviction take `png` out of the entry when transmitting.
- The worker decodes, downscales and **encodes PNG** (`fetch_encode_and_decode` wrapper), then sends `(url, Option<EncodedImage>)` where `EncodedImage { png: Vec<u8>, width: u32, height: u32 }`. Drop `DynamicImage` in the worker.
- Allocate the Kitty id when the fetch result is inserted (same wrapping rules as today, skipping 0). `ImageView` maps `Missing` for absent, `Loading` for InFlight, `Failed` for Failed, `Ready(&ReadyImage)` for Ready.
- `ensure_fetches`: for each URL, call `start_fetch`; keep the 4-in-flight cap and the permanent-failure semantics.
- `set_pinned`: store a `pinned: HashSet<String>`; LRU eviction skips pinned URLs. If all entries are pinned, allow growth (bounded by the 40-image article cap).
- Keep the 5 MB/decode/SSRF guards and panic-catching worker.
- Remove decoded-image storage; `MAX_CACHED_IMAGES` becomes 32 entries.
- Remove the old single-placement API (`render_at`, `display_cells`, `clear_terminal`, `kitty_images`, `last_placed_id`). Keep `pending_deletes`, `protocol()`, `force_protocol` and the `Debug` impl (update its fields).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib image::tests::`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/image.rs
git commit -m "refactor(image): store encoded images and expose cache views"
```

---

### Task 6: Transmission budget and virtual placements

**Files:**
- Modify: `src/image.rs`

**Interfaces:**
- Consumes: Task 5's `Entry`, `ReadyImage`, transmit helpers.
- Produces:
  ```rust
  #[derive(Clone, Debug, PartialEq, Eq)]
  pub struct PlacementSpec {
      pub url: String,
      pub cols: u16,
      pub rows: u16,
  }

  impl ImageCache {
      /// Pin `specs`, evict, transmit at most one image, and sync `U=1`
      /// placements whose cols/rows changed.
      pub fn pump(&mut self, stdout: &mut impl Write, specs: &[PlacementSpec]) -> io::Result<()>;
  }
  ```

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn pump_transmits_one_image_per_call_then_places_it() {
    let mut cache = ImageCache::new();
    cache.force_protocol(ImageProtocol::Kitty);
    cache.insert_ready_for_test("a", 1, 40, 10, false);
    cache.insert_ready_for_test("b", 2, 40, 10, false);
    let specs = [
        PlacementSpec { url: "a".into(), cols: 40, rows: 10 },
        PlacementSpec { url: "b".into(), cols: 40, rows: 10 },
    ];

    let mut out = Vec::new();
    cache.pump(&mut out, &specs).unwrap();
    let s = String::from_utf8_lossy(&out);
    assert_eq!(s.matches("a=t").count(), 1, "{s}");
    assert!(s.contains("a=p,U=1,i="), "{s}");

    let mut out2 = Vec::new();
    cache.pump(&mut out2, &specs).unwrap();
    assert!(String::from_utf8_lossy(&out2).contains("a=t"));

    let mut out3 = Vec::new();
    cache.pump(&mut out3, &specs).unwrap();
    assert!(out3.is_empty(), "both transmitted and placed: {:?}", out3);
}

#[test]
fn pump_replaces_virtual_placement_on_resize_without_retransmit() {
    let mut cache = ImageCache::new();
    cache.force_protocol(ImageProtocol::Kitty);
    cache.insert_ready_for_test("a", 1, 40, 10, true);
    let mut out = Vec::new();
    cache.pump(&mut out, &[PlacementSpec { url: "a".into(), cols: 40, rows: 10 }]).unwrap();
    assert!(out.is_empty());

    let mut resized = Vec::new();
    cache.pump(&mut resized, &[PlacementSpec { url: "a".into(), cols: 60, rows: 15 }]).unwrap();
    let s = String::from_utf8_lossy(&resized);
    assert!(s.contains("a=p,U=1,i=1,p=1,c=60,r=15"), "{s}");
    assert!(!s.contains("a=t"), "{s}");
}
```

Also add:
- `pump_does_nothing_on_none_protocol` — `force_protocol(ImageProtocol::None)` with a ready image yields empty output.
- `pump_frees_evicted_terminal_data` — evicting a transmitted, unpinned image makes the next `pump` emit `a=d,d=I,i=<id>,q=2`.
- `pump_pins_spec_urls_against_eviction` — insert 32 ready entries, `pump` with 32 specs, then insert a 33rd; the original 32 are still `Ready` (pinning bypasses the 32-entry cap).

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib image::tests::pump_`
Expected: FAIL.

- [ ] **Step 3: Implement**

- `pump`:
  1. `self.set_pinned(specs.iter().map(|s| s.url.clone()).collect())`, run LRU eviction, flush queued `a=d,d=I,i=<id>,q=2` deletes.
  2. For each spec whose entry is `Ready { image, .. }` with `image.transmitted == true` and whose `(cols, rows)` differs from `last_placement: HashMap<u32, (u16, u16)>`, emit `\x1b_Ga=p,U=1,i=<id>,p=1,c=<cols>,r=<rows>,q=2;\x1b\\` and update the map.
  3. If no image was transmitted this call, pick the first spec whose entry is `Ready { image, png: Some(..) }` with `!image.transmitted`, transmit the PNG with the existing chunked `a=t,f=100,t=d,i=<id>,q=2,m=...` sequence, set `image.transmitted = true`, drop `png`, emit the virtual placement, and stop.
  4. Remove evicted ids from `last_placement` so a later resize isn't suppressed by stale state.
- Keep return `io::Result<()>`; write errors propagate to the caller but are swallowed by the TUI.
- 4 MB base64 budget: the plan's one-image-per-call rule is stricter than 4 MB for typical images; still track `written_bytes` and stop if the chunk total would exceed 4 MB (an image larger than that is still sent alone to avoid deadlock).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib image::tests::`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/image.rs
git commit -m "feat(image): transmit one image per frame and sync virtual placements"
```

---

### Task 7: Terminal detection — placeholders or alt text

**Files:**
- Modify: `src/image.rs`

**Interfaces:**
- Produces:
  ```rust
  pub enum ImageProtocol { UnicodePlaceholders, None }

  fn protocol_for(
      tmux: bool,
      kitty_window: bool,
      term: Option<&str>,
      term_program: Option<&str>,
  ) -> ImageProtocol;
  ```

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn protocol_detection_supports_kitty_and_ghostty_only() {
    use ImageProtocol::*;
    assert_eq!(protocol_for(false, true, None, None), UnicodePlaceholders);
    assert_eq!(protocol_for(false, false, Some("xterm-kitty"), None), UnicodePlaceholders);
    assert_eq!(protocol_for(false, false, Some("xterm-ghostty"), None), UnicodePlaceholders);
    assert_eq!(protocol_for(false, false, Some("xterm-256color"), Some("ghostty")), UnicodePlaceholders);
    assert_eq!(protocol_for(false, false, Some("xterm-256color"), Some("wezterm")), None);
    assert_eq!(protocol_for(true, true, Some("xterm-kitty"), None), None);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib image::tests::protocol_detection_`
Expected: FAIL.

- [ ] **Step 3: Implement**

- Replace `ImageProtocol::Kitty` with `UnicodePlaceholders` everywhere (including tests and `ui/detail.rs` checks).
- `detect_protocol` reads env and delegates to `protocol_for`.
- `wezterm` is no longer accepted. Keep the tmux veto first.
- Update `start_fetch`/`pump` no-op checks for the new variant name.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib image::tests::protocol_detection_`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/image.rs
git commit -m "feat(image): gate inline images to Kitty and Ghostty placeholders"
```

---

### Task 8: App body-layout cache and search row mapping

**Files:**
- Modify: `src/app.rs`
- Modify: `src/ui/detail.rs` (only the parts compiling against the old fields)

**Interfaces:**
- Consumes: `crate::body::BodyLayout`.
- Produces:
  ```rust
  #[derive(Clone, Debug, PartialEq, Eq)]
  pub struct BodyKey {
      pub feed_url: String,
      pub item_key: String,
      pub width: u16,
      pub fulltext: bool,
  }

  #[derive(Clone, Debug)]
  pub struct CachedBodyLayout {
      pub key: BodyKey,
      pub layout: crate::body::BodyLayout,
  }

  // New App fields:
  pub detail_body_layout: Option<CachedBodyLayout>,
  pub detail_image_rows: Vec<u16>,
  pub detail_row_of_line: Vec<u16>,
  pub visible_image_specs: Vec<crate::image::PlacementSpec>,
  ```

- [ ] **Step 1: Write the failing tests**

Add to `src/app.rs` tests:

```rust
#[test]
fn scroll_to_match_uses_exact_display_row_with_images() {
    // layout_body("alpha\n\u{E000}0\u{E001}\nbeta", 80)
    // detail_image_rows = [12], detail_row_of_line = [0, 13]
    // article_search_matches = [{ line: 1, start: 0, end: 4 }]  // "beta"
    // after scroll_to_current_article_match:
    assert_eq!(app.detail_vertical_scroll, 11); // row 13 - 2 rows of context
}
```

Also:
- `body_layout_cache_invalidates_on_width_item_and_fulltext` — changing any part of the key replaces `detail_body_layout`.
- `detail_row_of_line_recomputes_when_image_rows_change` — same layout with `image_rows = [1]` vs `[12]` yields different rows for the following logical line.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib app::tests::scroll_to_match_uses_exact_display_row`
Expected: FAIL.

- [ ] **Step 3: Implement**

- Add fields to `App::new` (empty/None).
- Add `pub fn body_key(&self, width: u16, fulltext: bool) -> BodyKey` using the current feed URL and a stable item key (`item.link` or `item.title`).
- `scroll_to_current_article_match` reads `self.detail_row_of_line.get(m.line)` (falling back to `m.line`) and subtracts 2.
- Remove `article_body_width_cache`; keep `article_body_cache` for the search text (populated by the detail renderer).
- Remove `wrapped_row_of_line` once unused.
- Add a helper `pub fn set_detail_layout(&mut self, key: BodyKey, layout: BodyLayout)` that recomputes `detail_row_of_line` from `detail_image_rows`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib app::tests::`
Expected: PASS (some old tests may need updating for the removed field; update them with the same expectations).

- [ ] **Step 5: Commit**

```bash
git add src/app.rs src/ui/detail.rs
git commit -m "feat(app): cache body layout and map search to display rows"
```

---

### Task 9: Detail view — inline image rows, fallbacks and search highlight

**Files:**
- Modify: `src/ui/detail.rs`

**Interfaces:**
- Consumes: `BodyLayout`, `RowKind`, `ImageCache::{view, ensure_fetches}`, `ImageView`, `placeholder::{display_rows, placeholder_lines, loading_lines, alt_lines}`, `ImageProtocol`, `PlacementSpec`.
- Produces: `App::visible_image_specs` populated each frame; body rendered from the layout.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn detail_renders_placeholder_cells_for_transmitted_image() {
    // App with a feed item whose description is:
    //   <figure><img src="https://ex.com/a.png" alt="A"></figure>
    // force protocol UnicodePlaceholders; cache.insert_ready_for_test("https://ex.com/a.png", 1, 100, 50, true)
    let mut terminal = Terminal::new(TestBackend::new(80, 40)).unwrap();
    terminal.draw(|f| crate::ui::render(f, &mut app)).unwrap();
    let buf = terminal.backend().buffer();
    let cells = buf.content.iter().filter(|c| c.symbol() == "\u{10EEEE}").count();
    assert!(cells > 0);
}

#[test]
fn detail_falls_back_to_alt_text_on_none_protocol() {
    // same item, force ImageProtocol::None
    // assert no U+10EEEE cells and the buffer contains "[A]"
}

#[test]
fn detail_scrolls_placeholder_rows_with_the_body() {
    // draw, record row of first U+10EEEE; set detail_vertical_scroll = 5; draw;
    // assert the first U+10EEEE row moved up by 5 (or disappeared).
}

#[test]
fn search_highlight_spans_wrapped_rows() {
    // body: "the quick brown fox\n\u{E000}0\u{E001}\ntail"; width small enough
    // to wrap "quick brown" onto two rows; query "quick brown";
    // assert both rows contain the highlight style.
}

#[test]
fn detail_modal_covers_placeholder_cells() {
    // Build a body with enough text before the image that the image block sits
    // inside the help-overlay rect on an 80x40 screen. Draw with the overlay
    // open and assert no "\u{10EEEE}" cells remain inside that rect.
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib ui::detail::tests::detail_renders_placeholder_cells`
Expected: FAIL.

- [ ] **Step 3: Implement**

- Delete the image-strip setup: `IMAGE_STRIP_ROWS`, `MIN_AREA_HEIGHT_FOR_IMAGE`, the conditional `Constraint`, `pending_image_render`, `last_frame_had_image` usage, and the strip comment.
- Resolve the body source as today. Because `prepared_body` needs `&mut FeedItem`, resolve it through the selected feed/item indices *before* taking the `current_item()` immutable borrow (the same hoisting pattern the old image-strip setup used), then call `render_text()` and `layout_body(text, content_width)`. For `Extracted` / `Failed` / `Extracting`, use `format_content_for_reading(text)` and `layout_body(text, width)` (no images).
- Cache the layout in `app.detail_body_layout` keyed by `BodyKey`; recompute when the key changes.
- Compute `detail_image_rows`:
  - `ImageProtocol::None` → every entry is 1.
  - Otherwise, for each `BodyImage`, match `cache.view(url)`:
    - `Ready(r)` → `display_rows(Some((r.width, r.height)), cols)`
    - `Failed` → 1 (rendered as `alt_lines`)
    - `Missing` / `Loading` → `display_rows(declared_w.zip(declared_h), cols)` (12 rows when undeclared)
  - If the vector changed, recompute `detail_row_of_line` and clamp scroll.
- Build `Vec<Line>` by walking `layout.rows`:
  - `Text { line, start, end }`: slice `logical_lines[line][start..end]`; apply match highlighting by intersecting `[start, end)` with each `ArticleMatch` on that logical line. A match can contribute to several rows.
  - `Image(i)`: if protocol `None` or `view == Failed` → `alt_lines`; if `Ready(r)` and `r.transmitted` → `placeholder_lines(r.id, cols, rows)`; else `loading_lines(alt, cols, rows, colors)`.
- When `protocol == ImageProtocol::UnicodePlaceholders`, build `app.visible_image_specs` for images whose display row range intersects the viewport expanded by one viewport height: `PlacementSpec { url, cols, rows }`, and call `app.image_cache.ensure_fetches(&visible_urls)`. On `None`, leave the vector empty and start no fetches.
- `Paragraph` renders without `.wrap()`; `detail_max_scroll = total_rows - viewport_height`.
- Populate `article_body_cache` with `layout.logical_lines.join("\n")` when search is active; compute matches from it.
- Modals: leave rendering order unchanged; no image-specific suppression.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib ui::detail::tests::`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/ui/detail.rs
git commit -m "feat(detail): render inline image blocks and alt fallbacks"
```

---

### Task 10: TUI loop — pump images before each draw

**Files:**
- Modify: `src/tui.rs`

**Interfaces:**
- Consumes: `ImageCache::pump`, `App::visible_image_specs`.
- Produces: `fn pump_images_with(app: &mut App, out: &mut impl Write) -> io::Result<()>`; `pump_images(app)` locks stdout.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn pump_images_is_a_noop_on_none_protocol() {
    let mut app = App::new();
    app.image_cache.force_protocol(crate::image::ImageProtocol::None);
    let mut out: Vec<u8> = Vec::new();
    pump_images_with(&mut app, &mut out).unwrap();
    assert!(out.is_empty());
}
```

Also add `pump_images_transmits_ready_image_before_draw`.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib tui::tests::pump_images`
Expected: FAIL.

- [ ] **Step 3: Implement**

- Delete `emit_inline_image` and its call after `terminal.draw()`.
- At the top of the loop, before `terminal.draw()`: `app.image_cache.poll_completed();` then `pump_images(app)?;`.
- `pump_images_with` calls `app.image_cache.pump(out, &app.visible_image_specs)`.
- Remove `pending_image_render` / `last_frame_had_image` imports and uses.
- No `clear_terminal` on view change is needed: virtual placements have no visible cells outside the body.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib tui::tests::pump_images`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/tui.rs
git commit -m "feat(tui): pump image transmissions before each draw"
```

---

### Task 11: Cleanup, docs and full verification

**Files:**
- Modify: `README.md`
- Modify: `CLAUDE.md`
- Modify: any file still referencing removed symbols

- [ ] **Step 1: Remove dead code**

Delete `wrapped_row_of_line`, `article_body_width_cache`, `PendingImage` and any remaining strip-only tests. Run `cargo build` and fix every unused-import/dead-code warning the compiler reports.

- [ ] **Step 2: Update README**

Replace the Inline Images bullet with:

- Inline images: every block-level image in the article body renders at its position via the Kitty graphics protocol's Unicode placeholders. Requires Kitty >= 0.28 or Ghostty. WezTerm and tmux show the image's alt text. Inline (mid-sentence) images and the full-text extracted view are not rendered as images yet.

Update the terminal list wherever it says Kitty/Ghostty/WezTerm.

- [ ] **Step 3: Update CLAUDE.md**

Rewrite the "Inline images (Kitty protocol)" paragraph to describe the new pipeline: prepared body, sentinel tokens, `BodyLayout`, placeholder cells, `U=1` virtual placements, transmit budget, terminal gating, and the removal of the 10-row strip.

- [ ] **Step 4: Run formatting, lints and the whole suite**

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

Expected: clean fmt, no clippy warnings, all tests pass.

- [ ] **Step 5: Manual verification**

- Kitty: open an article with several images; scroll through; resize; open/close help overlay; search a phrase before/after an image; switch articles; confirm partially scrolled images render correctly.
- Ghostty: same smoke test.
- WezTerm (or `TERM_PROGRAM=wezterm` with protocol forced to `None`): alt text, no placeholders, no image traffic.
- Confirm `%i` and `%m` macros still expand.

- [ ] **Step 6: Commit**

```bash
git add README.md CLAUDE.md src/
git commit -m "docs: document inline image support and clean up old strip code"
```

---

## Self-Review Notes

- Spec coverage: discovery (Task 1), prepared text (Task 2), layout/rows/search mapping (Task 3, 8), placeholder cells (Task 4), cache/fetch (Task 5), transmit/placements (Task 6), terminal gating (Task 7), detail rendering (Task 9), loop wiring (Task 10), docs/removal (Task 11).
- Deliberate deviation from the spec's `BodyLayout` snippet: `row_of_line` is a method taking `image_rows`, because image heights are known only after decode. The spec was updated to match.
- Types used consistently: `BodyImage`, `PreparedBody`, `RowKind`, `BodyLayout`, `ImageView`, `ReadyImage`, `PlacementSpec`, `ImageProtocol::UnicodePlaceholders`.
- Test-environment note: the plan assumes a working cargo registry. The umwelt sandbox has no registry cache or network; execution needs either a host build or an egress grant for crates.io / static.rust-lang.org.
