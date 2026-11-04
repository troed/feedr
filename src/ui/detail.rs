use crate::app::{find_article_matches, App, ArticleMatch, ExtractionState, InputMode};
use crate::body::{body_hash, layout_body, BodyLayout, RowKind};
use crate::feed::BodyImage;
use crate::image::{ImageCache, ImageProtocol, ImageView, PlacementSpec};
use crate::keybindings::{key_display, KeyAction};
use crate::ui::placeholder::{
    alt_lines, display_rows, loading_lines, placeholder_lines, MAX_PLACEHOLDER_COLS,
};
use crate::ui::utils::{format_content_for_reading, truncate_str, truncate_url};
use crate::ui::ColorScheme;
use ratatui::{
    backend::Backend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Padding, Paragraph},
    Frame,
};
use std::collections::HashSet;
use unicode_width::UnicodeWidthStr;

/// Which body the detail view is currently showing. Owns its strings so
/// the immutable borrow on `app.extracted` is released before we call any
/// `&mut App` method later in the render (e.g. `set_detail_layout`).
enum BodySource {
    Summary,
    Extracted(String),
    Extracting,
    /// Failed extraction — body falls back to the summary, and `reason` is
    /// shown as a small dim hint above it.
    Failed(String),
}

/// Rows the article header occupies in compact mode: two border rows plus
/// the title, plus one more for the metadata/link row when the item has
/// either. Non-compact keeps its fixed 9-row header (see `render_item_detail`).
fn compact_header_height(has_meta: bool, has_link: bool) -> u16 {
    if has_meta || has_link {
        4
    } else {
        3
    }
}

/// Append `text` to `spans` only as far as the display width allows, and
/// advance `used` by what was actually kept. Text that cannot fit is
/// truncated with `truncate_str` when there is room for the ellipsis, and
/// dropped otherwise — compact rows must never wrap into the body.
fn push_clamped(
    spans: &mut Vec<Span<'static>>,
    used: &mut u16,
    text: String,
    style: Style,
    width: u16,
) {
    let remaining = width.saturating_sub(*used);
    if remaining == 0 {
        return;
    }
    let text_width = UnicodeWidthStr::width(text.as_str()).min(u16::MAX as usize) as u16;
    let kept = if text_width <= remaining {
        text
    } else if remaining > 3 {
        truncate_str(&text, remaining as usize)
    } else {
        return;
    };
    *used =
        used.saturating_add(UnicodeWidthStr::width(kept.as_str()).min(u16::MAX as usize) as u16);
    spans.push(Span::styled(kept, style));
}

/// Build the article header lines.
///
/// Non-compact reproduces the historical five-row shape: title, blank,
/// metadata, blank, link. Compact packs the title and a single
/// metadata/link row, clamping every piece to `inner_width` (the text
/// columns inside the block, borders and padding excluded) so the row can
/// never wrap and push the body out.
#[allow(clippy::too_many_arguments)]
fn build_header_lines(
    title: &str,
    read_status: Option<bool>,
    author: Option<&str>,
    date: Option<&str>,
    link: Option<&str>,
    compact: bool,
    inner_width: u16,
    colors: &ColorScheme,
) -> Vec<Line<'static>> {
    let title_style = Style::default()
        .fg(colors.text)
        .add_modifier(Modifier::BOLD);
    let title_text = if compact {
        truncate_str(title, inner_width as usize)
    } else {
        title.to_string()
    };

    // Metadata in display order: read status, author, date.
    let mut parts: Vec<(String, Style)> = Vec::new();
    if let Some(is_read) = read_status {
        parts.push((
            if is_read {
                "✓ Read".to_string()
            } else {
                "○ Unread".to_string()
            },
            Style::default().fg(if is_read {
                colors.success
            } else {
                colors.highlight
            }),
        ));
    }
    if let Some(author) = author {
        parts.push((
            author.to_string(),
            Style::default()
                .fg(colors.secondary)
                .add_modifier(Modifier::ITALIC),
        ));
    }
    if let Some(date) = date {
        parts.push((date.to_string(), Style::default().fg(colors.text_secondary)));
    }

    let separator_style = Style::default().fg(colors.muted);
    let link_style = Style::default()
        .fg(colors.primary)
        .add_modifier(Modifier::UNDERLINED);

    if !compact {
        let mut lines = vec![
            Line::from(vec![Span::styled(title_text, title_style)]),
            Line::from(""),
        ];
        if !parts.is_empty() {
            let mut spans: Vec<Span<'static>> = Vec::with_capacity(parts.len() * 2);
            for (idx, (text, style)) in parts.into_iter().enumerate() {
                if idx > 0 {
                    spans.push(Span::styled(" · ", separator_style));
                }
                spans.push(Span::styled(text, style));
            }
            lines.push(Line::from(spans));
        }
        lines.push(Line::from(""));
        if let Some(link) = link {
            lines.push(Line::from(vec![
                Span::styled("🔗 ", separator_style),
                Span::styled(truncate_url(link, 70), link_style),
            ]));
        }
        return lines;
    }

    // Compact: one metadata/link row under the title.
    const LINK_SEPARATOR: &str = " · ";
    const LINK_ICON: &str = "🔗 ";
    let mut lines = vec![Line::from(vec![Span::styled(title_text, title_style)])];
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(parts.len() * 2 + 3);
    let mut used: u16 = 0;
    for (idx, (text, style)) in parts.into_iter().enumerate() {
        if idx > 0 {
            push_clamped(
                &mut spans,
                &mut used,
                LINK_SEPARATOR.to_string(),
                separator_style,
                inner_width,
            );
        }
        push_clamped(&mut spans, &mut used, text, style, inner_width);
    }
    if let Some(link) = link {
        // Trailing " · 🔗 " plus at least a few URL columns, or the link is
        // dropped rather than rendered as a bare "…".
        let overhead = UnicodeWidthStr::width(LINK_SEPARATOR) + UnicodeWidthStr::width(LINK_ICON);
        let remaining = inner_width.saturating_sub(used) as usize;
        if remaining >= overhead + 8 {
            spans.push(Span::styled(LINK_SEPARATOR, separator_style));
            spans.push(Span::styled(LINK_ICON, separator_style));
            spans.push(Span::styled(
                truncate_url(link, remaining - overhead),
                link_style,
            ));
        }
    }
    if !spans.is_empty() {
        lines.push(Line::from(spans));
    }
    lines
}

pub(super) fn render_item_detail<B: Backend>(
    f: &mut Frame<B>,
    app: &mut App,
    area: Rect,
    colors: &ColorScheme,
) {
    // Reset the `show_extracted` toggle on article focus change. Done at
    // the top so we hold no immutable borrow on `app` (the `current_item`
    // call below would conflict). Logic lives on `App` so it's
    // unit-testable without spinning up a TestBackend.
    app.sync_detail_focus_anchor();
    // The search footer is visible whenever the user is actively typing a
    // query OR a committed query has highlights still pinned. Computing
    // this once up front lets the layout split, footer render, and title
    // suffix all agree on visibility.
    let search_footer_visible =
        matches!(app.input_mode, InputMode::ArticleSearch) || !app.article_search_query.is_empty();

    let Some((feed_idx, item_idx)) = app.current_article_indices() else {
        return;
    };

    // ── Resolve the body source before the header borrow ─────────────────
    // `prepared_body` needs `&mut FeedItem`, so it is hoisted through the
    // selected indices here; the header renderer below takes its own
    // short-lived immutable borrow.
    let body_source: BodySource = if app.show_extracted {
        match app.extraction_state_for(feed_idx, item_idx) {
            Some(ExtractionState::Ready(article)) => {
                BodySource::Extracted(article.plain_text.clone())
            }
            Some(ExtractionState::Pending { .. }) => BodySource::Extracting,
            Some(ExtractionState::Failed(reason)) => BodySource::Failed(reason.clone()),
            None => BodySource::Summary,
        }
    } else {
        BodySource::Summary
    };

    // The summary is the only source that owns image blocks. Prepare it
    // once per frame (the item caches the DOM/sentinel pass) and reuse the
    // resulting text for both layout and search.
    let prepared_summary: Option<(String, Vec<BodyImage>)> =
        if matches!(body_source, BodySource::Summary) {
            let (base_url, has_description) = app
                .feeds
                .get(feed_idx)
                .and_then(|feed| feed.items.get(item_idx))
                .map(|item| (item.link.clone(), item.description.is_some()))
                .unwrap_or((None, false));
            if has_description {
                app.feeds
                    .get_mut(feed_idx)
                    .and_then(|feed| feed.items.get_mut(item_idx))
                    .map(|item| {
                        let prepared = item.prepared_body(base_url.as_deref());
                        (prepared.render_text().to_string(), prepared.images.clone())
                    })
            } else {
                None
            }
        } else {
            None
        };

    // Plain summary text for the in-flight / failed banners. Those states
    // deliberately show no image blocks (a half-extracted article is a
    // fallback view, not the reading view), so they use the old
    // HTML→text path instead of the sentinel-aware one.
    let summary_fallback = || -> String {
        app.feeds
            .get(feed_idx)
            .and_then(|feed| feed.items.get(item_idx))
            .and_then(|item| item.description.as_deref())
            .map(|desc| format_content_for_reading(&crate::feed::render_clean_html(desc, 100)))
            .unwrap_or_else(|| "No description available".to_string())
    };

    // The retry / toggle hints embed the *current* binding for
    // FetchFullText so they stay correct if the user remaps it.
    let fulltext_key = key_display(&KeyAction::FetchFullText, &app.keybindings);

    let (body_text, body_images): (String, Vec<BodyImage>) = match &body_source {
        BodySource::Summary => {
            prepared_summary.unwrap_or_else(|| ("No description available".to_string(), Vec::new()))
        }
        BodySource::Extracted(text) => (format_content_for_reading(text), Vec::new()),
        // Keep the summary visible while the worker is in flight — a slow
        // site can take several seconds, and replacing the body with
        // "Fetching…" leaves the user with nothing to read AND no way to
        // revert (the FetchFullText key is a no-op on Pending). The
        // scroll-indicator title carries the "Extracting…" status.
        BodySource::Extracting => (
            format!(
                "[Fetching full text… — showing summary]\n\n{}",
                summary_fallback()
            ),
            Vec::new(),
        ),
        BodySource::Failed(reason) => (
            format!(
                "[Full-text extraction failed: {}]\n[Press {} to retry — showing summary]\n\n{}",
                reason,
                fulltext_key,
                summary_fallback()
            ),
            Vec::new(),
        ),
    };

    // Pin every image the current article references, not just the ones on
    // screen: scrolling back must not evict and re-transmit an image that
    // was visible a moment ago. The detail renderer owns this pin set;
    // `pump` only consumes it. `ui::render` clears it outside this view.
    let article_urls: HashSet<String> = body_images.iter().map(|image| image.url.clone()).collect();
    app.image_cache.set_pinned(article_urls);

    // Split the area into header, content, and (optionally) a 1-row search
    // footer. The footer is omitted when in-article search is inactive so
    // quiet reading is unchanged.
    // Header height: compact shrinks it to title + one metadata/link row
    // plus borders (3 or 4 rows); non-compact keeps the historical 9. The
    // two flags need only a short-lived immutable borrow so the `&mut app`
    // writes above/below stay legal.
    const HEADER_ROWS: u16 = 9;
    let (has_header_meta, has_header_link) = app
        .current_item()
        .map(|item| {
            (
                item.author.is_some()
                    || item.formatted_date.is_some()
                    || (app.selected_feed.is_some() && app.selected_item.is_some()),
                item.link.is_some(),
            )
        })
        .unwrap_or((false, false));
    let header_rows = if app.compact {
        compact_header_height(has_header_meta, has_header_link)
    } else {
        HEADER_ROWS
    };
    let mut constraints: Vec<Constraint> =
        vec![Constraint::Length(header_rows), Constraint::Min(0)];
    if search_footer_visible {
        constraints.push(Constraint::Length(1));
    }
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    let content_chunk = chunks[1];
    let footer_chunk_idx: Option<usize> = search_footer_visible.then_some(2);

    render_article_header(f, app, chunks[0], colors);

    // Viewport height (borders + padding) and the width the body layout
    // wraps to. Both must match the Paragraph's inner area exactly.
    let viewport_height = content_chunk
        .height
        .saturating_sub(2) // borders (top and bottom)
        .saturating_sub(4); // padding (top and bottom)
    let content_width = usize::from(
        content_chunk
            .width
            .saturating_sub(2) // borders (left and right)
            .saturating_sub(8), // padding for better reading width
    );
    let cols = content_width as u16;
    // Placeholder cells address columns through one byte of diacritics, so
    // an image may span at most 256 content columns. Text still wraps at the
    // full content width; only image rows/specs use the clamped value.
    let image_cols = cols.min(MAX_PLACEHOLDER_COLS);
    let protocol = app.image_cache.protocol();

    // ── Body layout cache ────────────────────────────────────────────────
    // The key catches article / width / summary-vs-fulltext changes; the
    // body hash catches source changes the key cannot see (refresh
    // replacing the summary, extraction landing, Ready → Failed).
    let key = app.body_key(cols, app.show_extracted);
    let hash = body_hash(&body_text, body_images.len());
    let layout_is_current = app
        .detail_body_layout
        .as_ref()
        .map(|cached| cached.matches(&key, hash))
        .unwrap_or(false);
    if !layout_is_current {
        let layout = layout_body(&body_text, content_width);
        app.set_detail_layout(key, hash, layout);
    }

    // ── Image block heights ──────────────────────────────────────────────
    let image_rows = image_display_rows(&body_images, protocol, &app.image_cache, image_cols);
    if image_rows != app.detail_image_rows {
        app.set_detail_image_rows(image_rows);
    }

    let total_rows = app
        .detail_body_layout
        .as_ref()
        .map(|cached| total_display_rows(&cached.layout, &app.detail_image_rows))
        .unwrap_or(0);
    app.update_detail_max_scroll(total_rows, viewport_height);
    app.clamp_detail_scroll();

    // ── In-article search ────────────────────────────────────────────────
    // Search work is skipped entirely when the user isn't typing a query
    // and has no committed query pinned — keeps the steady-state detail
    // view allocation-free.
    let search_active = search_footer_visible;

    let (matches, current_idx) = if search_active {
        // Cache the laid-out body so `n` / `N` (handled in event dispatch)
        // can resolve a match line back to a scroll target without
        // re-deriving the body source. Logical lines are what search scans
        // and what `detail_row_of_line` indexes.
        let body_for_search = app
            .detail_body_layout
            .as_ref()
            .map(|cached| cached.layout.logical_lines.join("\n"))
            .unwrap_or_default();
        app.article_body_cache.clear();
        app.article_body_cache.push_str(&body_for_search);

        // Recompute matches against the freshly laid-out body. Clamp the
        // current-match cursor so it stays valid as the query shrinks or
        // the body changes (e.g. after `Shift+F` toggles full-text).
        let matches = find_article_matches(&app.article_body_cache, &app.article_search_query);
        if matches.is_empty() {
            app.article_search_current = None;
        } else {
            let cur = app
                .article_search_current
                .unwrap_or(0)
                .min(matches.len() - 1);
            app.article_search_current = Some(cur);
        }
        let current_idx = app.article_search_current;
        app.article_search_matches = matches.clone();
        (matches, current_idx)
    } else {
        // Search inactive: ensure stale state can't satisfy a future
        // `n`/`N` press, then skip the work.
        app.article_search_matches.clear();
        app.article_search_current = None;
        (Vec::new(), None)
    };

    // Create theme-specific scroll indicator
    let scroll_arrows = if colors.border_normal == BorderType::Double {
        ("▼", "▲") // Dark: solid arrows
    } else {
        ("↓", "↑") // Light: simple arrows
    };

    let body_label = match &body_source {
        BodySource::Summary => "Summary",
        BodySource::Extracted(_) => "Full-text",
        BodySource::Extracting => "Extracting…",
        BodySource::Failed(_) => "Full-text failed",
    };

    let match_suffix = if app.article_search_query.is_empty() {
        String::new()
    } else if matches.is_empty() {
        " · no matches".to_string()
    } else {
        format!(
            " · {}/{} matches",
            current_idx.map(|i| i + 1).unwrap_or(0),
            matches.len()
        )
    };

    let scroll_indicator = if app.detail_max_scroll > 0 {
        let scroll_pct =
            (app.detail_vertical_scroll as f32 / app.detail_max_scroll as f32 * 100.0) as u16;
        if app.detail_vertical_scroll == 0 {
            format!(
                " {} {} · Scroll {} for more{} ",
                colors.get_icon_article(),
                body_label,
                scroll_arrows.0,
                match_suffix
            )
        } else if app.detail_vertical_scroll >= app.detail_max_scroll {
            format!(
                " {} {} · End of article{} ",
                colors.get_icon_article(),
                body_label,
                match_suffix
            )
        } else {
            format!(
                " {} {} · {}%{} ",
                colors.get_icon_article(),
                body_label,
                scroll_pct,
                match_suffix
            )
        }
    } else {
        format!(
            " {} {}{} ",
            colors.get_icon_article(),
            body_label,
            match_suffix
        )
    };

    // Build the body from the pre-wrapped layout. `Paragraph` renders
    // without `.wrap()`: every row is already exactly one display row, so
    // the buffer rows and `detail_row_of_line` agree. The borrow on the
    // cached layout ends with this block, before the placement pass below.
    if let Some(cached) = app.detail_body_layout.as_ref() {
        let ctx = BodyRowContext {
            images: &body_images,
            image_rows: &app.detail_image_rows,
            matches: &matches,
            current_idx,
            protocol,
            cache: &app.image_cache,
            colors,
            cols: image_cols,
        };
        let body_lines = build_body_lines(&cached.layout, &ctx);

        // Create content paragraph with theme-specific styling
        let content = Paragraph::new(body_lines)
            .block(
                Block::default()
                    .title(scroll_indicator)
                    .title_alignment(Alignment::Center)
                    .borders(Borders::ALL)
                    .border_type(colors.border_normal)
                    .border_style(Style::default().fg(colors.border))
                    .style(Style::default().bg(colors.surface))
                    .padding(Padding::new(4, 4, 2, 2)), // Generous padding for reading comfort
            )
            .style(Style::default().fg(colors.text))
            .scroll((app.detail_vertical_scroll, 0))
            .alignment(Alignment::Left);

        f.render_widget(content, content_chunk);
    }

    // ── Visible placements ───────────────────────────────────────────────
    // Placeholder cells only exist on Kitty/Ghostty. Specs are built for
    // image blocks intersecting the viewport expanded by one viewport
    // height in each direction, and fetches are queued for those URLs.
    // On `None` the vector stays empty (cleared by `ui::render`) and no
    // fetch is started.
    if protocol == ImageProtocol::UnicodePlaceholders {
        let mut specs: Vec<PlacementSpec> = Vec::new();
        let mut urls: Vec<String> = Vec::new();
        let scroll = u32::from(app.detail_vertical_scroll);
        let visible = u32::from(viewport_height).max(1);
        let window_start = scroll.saturating_sub(visible);
        let window_end = scroll.saturating_add(visible.saturating_mul(2));

        if let Some(cached) = app.detail_body_layout.as_ref() {
            let mut display_row: u32 = 0;
            for row in &cached.layout.rows {
                match *row {
                    RowKind::Text { .. } => display_row = display_row.saturating_add(1),
                    RowKind::Image(index) => {
                        let rows =
                            u32::from(app.detail_image_rows.get(index).copied().unwrap_or(1));
                        let intersects = rows > 0
                            && display_row < window_end
                            && display_row + rows > window_start;
                        if intersects {
                            if let Some(image) = body_images.get(index) {
                                if matches!(app.image_cache.view(&image.url), ImageView::Ready(_)) {
                                    specs.push(PlacementSpec {
                                        url: image.url.clone(),
                                        cols: image_cols,
                                        rows: rows as u16,
                                    });
                                }
                                urls.push(image.url.clone());
                            }
                        }
                        display_row = display_row.saturating_add(rows);
                    }
                }
            }
        }

        app.image_cache.ensure_fetches(&urls);
        app.visible_image_specs = specs;
    }

    if let Some(idx) = footer_chunk_idx {
        render_search_footer(f, app, chunks[idx], colors);
    }
}

/// Render the article header (title, read status, author, date, link).
///
/// Takes its own short-lived immutable `App` borrow; the caller resolves
/// the body before this so the `&mut FeedItem` needed by `prepared_body`
/// never overlaps.
fn render_article_header<B: Backend>(
    f: &mut Frame<B>,
    app: &App,
    area: Rect,
    colors: &ColorScheme,
) {
    let Some(item) = app.current_item() else {
        return;
    };

    // Text columns inside the header block: borders (2) plus three columns
    // of horizontal padding on each side.
    let header_inner_width = area.width.saturating_sub(8);
    let read_status = match (app.selected_feed, app.selected_item) {
        (Some(feed_idx), Some(item_idx)) => Some(app.is_item_read(feed_idx, item_idx)),
        _ => None,
    };
    let header_lines = build_header_lines(
        &item.title,
        read_status,
        item.author.as_deref(),
        item.formatted_date.as_deref(),
        item.link.as_deref(),
        app.compact,
        header_inner_width,
        colors,
    );

    let article_icon = colors.get_icon_article();
    let header = Paragraph::new(header_lines)
        .block(
            Block::default()
                .title(format!(" {} Article ", article_icon))
                .title_alignment(Alignment::Center)
                .borders(Borders::ALL)
                .border_type(colors.border_normal)
                .border_style(Style::default().fg(colors.border))
                .style(Style::default().bg(colors.surface))
                // Compact drops the vertical padding: the header is only
                // 3-4 rows and the body needs them more.
                .padding(if app.compact {
                    Padding::new(3, 3, 0, 0)
                } else {
                    Padding::new(3, 3, 1, 1)
                }),
        )
        .style(Style::default().fg(colors.text))
        .alignment(Alignment::Left);

    f.render_widget(header, area);
}

/// Total display rows of a layout: every text row is one row, every image
/// block is its current display height. `rows.len()` is *not* the answer
/// because an image block is a single `RowKind` that occupies many rows.
fn total_display_rows(layout: &BodyLayout, image_rows: &[u16]) -> u16 {
    layout.rows.iter().fold(0u16, |total, row| match *row {
        RowKind::Text { .. } => total.saturating_add(1),
        RowKind::Image(index) => total.saturating_add(image_rows.get(index).copied().unwrap_or(1)),
    })
}

/// Display height of every image block for the current frame.
///
/// Without placeholder support every block is the one-row alt line. On a
/// supported terminal a ready image sizes to its real aspect ratio, a
/// failed fetch falls back to the one-row alt line, and pending images
/// reserve rows from their declared dimensions (or the default block).
fn image_display_rows(
    images: &[BodyImage],
    protocol: ImageProtocol,
    cache: &ImageCache,
    cols: u16,
) -> Vec<u16> {
    images
        .iter()
        .map(|image| {
            if protocol == ImageProtocol::None {
                return 1;
            }
            match cache.view(&image.url) {
                ImageView::Ready(ready) => display_rows(Some((ready.width, ready.height)), cols),
                ImageView::Failed => 1,
                ImageView::Missing | ImageView::Loading => {
                    display_rows(image.declared_w.zip(image.declared_h), cols)
                }
            }
        })
        .collect()
}

/// Everything [`build_body_lines`] needs besides the layout itself.
#[derive(Clone, Copy)]
struct BodyRowContext<'a> {
    images: &'a [BodyImage],
    image_rows: &'a [u16],
    matches: &'a [ArticleMatch],
    current_idx: Option<usize>,
    protocol: ImageProtocol,
    cache: &'a ImageCache,
    colors: &'a ColorScheme,
    cols: u16,
}

/// Walk the pre-wrapped layout and build one `Line` per display row.
///
/// `Text` rows slice `logical_lines[line][start..end]` and re-apply match
/// highlighting clipped to that slice, so a phrase spanning a soft-wrap
/// boundary highlights on both rows. `Image` rows expand to placeholder
/// cells, a loading caption, or the alt-line fallback.
fn build_body_lines<'a>(layout: &'a BodyLayout, ctx: &BodyRowContext<'a>) -> Vec<Line<'a>> {
    let BodyRowContext {
        images,
        image_rows,
        matches,
        current_idx,
        protocol,
        cache,
        colors,
        cols,
    } = *ctx;
    // Non-current matches: foreground tinted only, no background fill, no
    // bold — present but quiet. The match the `n` cursor sits on gets the
    // attention: filled `highlight` background, surface-colored text, bold.
    let highlight_style = Style::default().fg(colors.highlight);
    let current_style = Style::default()
        .bg(colors.highlight)
        .fg(colors.surface)
        .add_modifier(Modifier::BOLD);

    let mut lines = Vec::with_capacity(layout.rows.len());
    // `find_article_matches` emits matches in line-major order and layout
    // rows are line-major too, so one cursor + a per-line bucket suffices.
    let mut cursor = 0usize;
    let mut current_line = usize::MAX;
    let mut bucket_base = 0usize;
    let mut bucket_len = 0usize;

    for row in &layout.rows {
        match *row {
            RowKind::Text { line, start, end } => {
                if line != current_line {
                    current_line = line;
                    while cursor < matches.len() && matches[cursor].line < line {
                        cursor += 1;
                    }
                    bucket_base = cursor;
                    while cursor < matches.len() && matches[cursor].line == line {
                        cursor += 1;
                    }
                    bucket_len = cursor - bucket_base;
                }

                let full = layout
                    .logical_lines
                    .get(line)
                    .map(String::as_str)
                    .unwrap_or("");
                let start = start.min(full.len());
                let end = end.max(start).min(full.len());

                if bucket_len == 0 {
                    // `Line::from(&'a str)` borrows the slice rather than
                    // copying it (`Line::raw` isn't available in 0.23).
                    lines.push(Line::from(&full[start..end]));
                    continue;
                }

                let mut spans: Vec<Span<'a>> = Vec::new();
                let mut col = start;
                for offset in 0..bucket_len {
                    let m = &matches[bucket_base + offset];
                    let match_start = m.start.max(start);
                    let match_end = m.end.min(end);
                    if match_end <= match_start {
                        continue;
                    }
                    if match_start > col {
                        spans.push(Span::raw(&full[col..match_start]));
                    }
                    let style = if Some(bucket_base + offset) == current_idx {
                        current_style
                    } else {
                        highlight_style
                    };
                    spans.push(Span::styled(&full[match_start..match_end], style));
                    col = match_end;
                }
                if col < end {
                    spans.push(Span::raw(&full[col..end]));
                }
                lines.push(Line::from(spans));
            }
            RowKind::Image(index) => {
                let rows = image_rows.get(index).copied().unwrap_or(1);
                let image = images.get(index);
                let alt = image.map(|img| img.alt.as_str()).unwrap_or("");
                let view = image.map(|img| cache.view(&img.url));

                let image_lines = if protocol == ImageProtocol::None {
                    alt_lines(alt, colors)
                } else {
                    match view {
                        Some(ImageView::Ready(ready)) if ready.transmitted => {
                            placeholder_lines(ready.id, cols, rows)
                        }
                        Some(ImageView::Ready(_))
                        | Some(ImageView::Loading)
                        | Some(ImageView::Missing) => loading_lines(alt, cols, rows, colors),
                        Some(ImageView::Failed) | None => alt_lines(alt, colors),
                    }
                };
                lines.extend(image_lines);
            }
        }
    }

    lines
}

/// Render the 1-row search footer at the bottom of the detail view.
/// Shows `/<query>` and (when not actively typing) the match indicator.
/// Sets the terminal cursor on the line when in `ArticleSearch` input
/// mode so the user sees where input is going.
fn render_search_footer<B: Backend>(f: &mut Frame<B>, app: &App, area: Rect, colors: &ColorScheme) {
    let typing = matches!(app.input_mode, InputMode::ArticleSearch);
    let match_count = app.article_search_matches.len();
    let current = app.article_search_current;

    let mut spans = vec![
        Span::styled(
            "/",
            Style::default()
                .fg(colors.highlight)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            app.article_search_query.clone(),
            Style::default().fg(colors.text),
        ),
    ];

    if !typing {
        let status = if app.article_search_query.is_empty() {
            String::new()
        } else if match_count == 0 {
            "  no matches".to_string()
        } else {
            format!(
                "  {}/{} (n/N to jump · ESC to clear)",
                current.map(|i| i + 1).unwrap_or(0),
                match_count
            )
        };
        if !status.is_empty() {
            spans.push(Span::styled(status, Style::default().fg(colors.muted)));
        }
    }

    let footer = Paragraph::new(Line::from(spans))
        .style(Style::default().bg(colors.surface))
        .alignment(Alignment::Left);
    f.render_widget(footer, area);

    if typing {
        // Cursor sits just past the rendered query: 1 column for "/" plus
        // the display width of the query. Clamp to the visible row width.
        let q_width =
            unicode_width::UnicodeWidthStr::width(app.article_search_query.as_str()) as u16;
        let cursor_x = area.x + 1 + q_width;
        f.set_cursor(cursor_x.min(area.x + area.width.saturating_sub(1)), area.y);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feed::{Feed, FeedItem};
    use crate::image::{ImageProtocol, ImageView};
    use ratatui::buffer::Buffer;
    use ratatui::style::Color;
    use ratatui::{backend::TestBackend, Terminal};

    /// First screen row of the article body's text area on an 80x40 or 24x40
    /// terminal: margin (1) + title bar (4) + article header (9) + border (1)
    /// + padding (2).
    const BODY_TOP: u16 = 17;

    fn feed_item(html: &str) -> FeedItem {
        FeedItem {
            title: "Post".to_string(),
            title_lower: "post".to_string(),
            link: Some("https://ex.com/post".to_string()),
            description: Some(html.to_string()),
            pub_date: None,
            author: None,
            formatted_date: None,
            parsed_date: None,
            plain_text: None,
            prepared_body: None,
            media: Vec::new(),
            thumbnail: None,
        }
    }

    fn app_with_body(html: &str, protocol: ImageProtocol) -> App {
        let mut app = App::new();
        app.feeds = vec![Feed {
            url: "https://ex.com/feed.xml".to_string(),
            title: "Feed".to_string(),
            title_lower: "feed".to_string(),
            items: vec![feed_item(html)],
        }];
        app.view = crate::app::View::FeedItemDetail;
        app.selected_feed = Some(0);
        app.selected_item = Some(0);
        app.image_cache.force_protocol(protocol);
        app
    }

    fn draw(app: &mut App, width: u16, height: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| crate::ui::render(f, app)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn row_text(buf: &Buffer, y: u16) -> String {
        (0..buf.area.width)
            .map(|x| buf.get(x, y).symbol.clone())
            .collect()
    }

    fn buffer_text(buf: &Buffer) -> String {
        (0..buf.area.height)
            .map(|y| row_text(buf, y))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn find_row(buf: &Buffer, needle: &str) -> Option<u16> {
        (0..buf.area.height).find(|&y| row_text(buf, y).contains(needle))
    }

    fn placeholder_cells(buf: &Buffer) -> Vec<(u16, u16)> {
        let mut cells = Vec::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                if buf.get(x, y).symbol.contains('\u{10EEEE}') {
                    cells.push((x, y));
                }
            }
        }
        cells
    }

    fn first_placeholder_row(buf: &Buffer) -> Option<u16> {
        placeholder_cells(buf).iter().map(|&(_, y)| y).min()
    }

    /// Six short paragraphs guarantee at least six body rows, so the image
    /// block starts far enough down that a 5-row scroll still leaves it on
    /// screen.
    fn filler_then_image() -> String {
        let mut html = String::new();
        for word in ["one", "two", "three", "four", "five", "six"] {
            html.push_str(&format!("<p>{word}</p>"));
        }
        html.push_str(r#"<figure><img src="https://ex.com/a.png" alt="A"></figure>"#);
        html
    }

    #[test]
    fn detail_renders_placeholder_cells_for_transmitted_image() {
        let mut app = app_with_body(
            filler_then_image().as_str(),
            ImageProtocol::UnicodePlaceholders,
        );
        app.image_cache
            .insert_ready_for_test("https://ex.com/a.png", 1, 100, 50, true);

        let buf = draw(&mut app, 80, 40);
        let cells = placeholder_cells(&buf);
        assert!(
            !cells.is_empty(),
            "transmitted ready image must render U+10EEEE placeholder cells"
        );
        // The image id rides on the foreground colour (id 1 -> RGB 0,0,1).
        assert_eq!(buf.get(cells[0].0, cells[0].1).fg, Color::Rgb(0, 0, 1));
    }

    #[test]
    fn detail_falls_back_to_alt_text_on_none_protocol() {
        let mut app = app_with_body(filler_then_image().as_str(), ImageProtocol::None);
        app.image_cache
            .insert_ready_for_test("https://ex.com/a.png", 1, 100, 50, true);

        let buf = draw(&mut app, 80, 40);
        assert!(
            placeholder_cells(&buf).is_empty(),
            "no placeholder cells without Kitty support"
        );
        let text = buffer_text(&buf);
        assert!(
            text.contains("[A]"),
            "expected alt-text fallback, got buffer: {text:?}"
        );
    }

    #[test]
    fn detail_none_protocol_queues_no_fetch_or_placement() {
        let mut app = app_with_body(
            r#"<figure><img src="https://ex.com/a.png" alt="A"></figure>"#,
            ImageProtocol::None,
        );
        let _ = draw(&mut app, 80, 40);

        assert!(app.visible_image_specs.is_empty());
        // No fetch was queued and no cache entry was created.
        assert!(matches!(
            app.image_cache.view("https://ex.com/a.png"),
            ImageView::Missing
        ));
    }

    #[test]
    fn detail_rebuilds_layout_when_summary_text_changes() {
        let mut app = app_with_body("<p>alpha</p>", ImageProtocol::None);
        let _ = draw(&mut app, 80, 40);
        assert_eq!(
            app.detail_body_layout
                .as_ref()
                .unwrap()
                .layout
                .logical_lines,
            vec!["alpha".to_string()]
        );

        // Same article (same BodyKey) but a refresh replaced the summary:
        // the body hash must force a rebuild.
        app.feeds[0].items[0].description = Some("<p>beta</p>".to_string());
        app.feeds[0].items[0].prepared_body = None;
        let _ = draw(&mut app, 80, 40);
        assert_eq!(
            app.detail_body_layout
                .as_ref()
                .unwrap()
                .layout
                .logical_lines,
            vec!["beta".to_string()]
        );
    }

    #[test]
    fn detail_scrolls_placeholder_rows_with_the_body() {
        let mut app = app_with_body(
            filler_then_image().as_str(),
            ImageProtocol::UnicodePlaceholders,
        );
        app.image_cache
            .insert_ready_for_test("https://ex.com/a.png", 1, 100, 50, true);

        let before = draw(&mut app, 80, 40);
        let first_before = first_placeholder_row(&before).expect("image must be on screen");

        app.detail_vertical_scroll = 5;
        let after = draw(&mut app, 80, 40);
        let first_after = first_placeholder_row(&after).expect("image must still be on screen");

        assert_eq!(
            first_after,
            first_before - 5,
            "scrolling 5 rows must move the first placeholder row up by 5"
        );
    }

    #[test]
    fn search_highlight_spans_wrapped_rows() {
        // Enough trailing body that the `n` jump's target scroll is not
        // clamped to zero by a body shorter than the viewport.
        let mut html = String::from("<p>one</p><p>two</p><p>three</p>");
        html.push_str("<p>the quick brown fox</p><p>tail</p>");
        for n in 0..10 {
            html.push_str(&format!("<p>after {n}</p>"));
        }
        let mut app = app_with_body(&html, ImageProtocol::None);
        // First draw establishes the focus anchor; the query survives the
        // next frame's `sync_detail_focus_anchor`.
        let _ = draw(&mut app, 22, 40);

        app.article_search_query = "quick brown".to_string();
        let buf = draw(&mut app, 22, 40);

        assert_eq!(app.article_search_matches.len(), 1, "expected one match");
        let m = app.article_search_matches[0].clone();
        let row = app.detail_row_of_line[m.line];
        assert!(row >= 2, "fixture must leave context above the match");

        let quick_y = find_row(&buf, "quick").expect("quick visible");
        let brown_y = find_row(&buf, "brown").expect("brown visible");
        assert_eq!(
            quick_y,
            BODY_TOP + row,
            "match must start at the logical line's first display row"
        );
        assert_eq!(brown_y, quick_y + 1, "phrase must wrap onto the next row");

        let highlight = app.color_scheme.highlight;
        let quick_x = row_text(&buf, quick_y).find("quick").unwrap() as u16;
        let brown_x = row_text(&buf, brown_y).find("brown").unwrap() as u16;
        assert_eq!(buf.get(quick_x, quick_y).bg, highlight);
        assert_eq!(buf.get(brown_x, brown_y).bg, highlight);

        // `n` jumps to the display row of the match, minus two context rows.
        app.detail_vertical_scroll = 0;
        app.next_article_match();
        let expected_scroll = row.saturating_sub(2);
        assert_eq!(app.detail_vertical_scroll, expected_scroll);

        let scrolled = draw(&mut app, 22, 40);
        let scrolled_quick_y = find_row(&scrolled, "quick").expect("quick visible after jump");
        assert_eq!(scrolled_quick_y, BODY_TOP + row - expected_scroll);
    }

    #[test]
    fn detail_modal_covers_placeholder_cells() {
        let mut app = app_with_body(
            filler_then_image().as_str(),
            ImageProtocol::UnicodePlaceholders,
        );
        app.image_cache
            .insert_ready_for_test("https://ex.com/a.png", 1, 100, 50, true);

        let no_modal = draw(&mut app, 80, 40);
        assert!(
            !placeholder_cells(&no_modal).is_empty(),
            "fixture must put placeholder cells on screen"
        );

        app.show_help_overlay = true;
        let modal = draw(&mut app, 80, 40);
        // Help overlay rect on 80x40: 80% width (64) x 85% height (34),
        // centred -> x 8..72, y 3..37.
        let leaked: Vec<_> = placeholder_cells(&modal)
            .into_iter()
            .filter(|&(x, y)| (8..72).contains(&x) && (3..37).contains(&y))
            .collect();
        assert!(
            leaked.is_empty(),
            "modal must cover every placeholder cell in its rect; leaked: {leaked:?}"
        );
    }

    /// Extract the printable text of a `Line` so tests can assert content
    /// independent of span styling. Joins all span text in order.
    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

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
            "Hello",
            Some(false),
            Some("Ada"),
            Some("Nov 4"),
            Some("https://example.com/a"),
            true,
            60,
            &colors,
        );
        assert_eq!(lines.len(), 2);
        let second = line_text(&lines[1]);
        assert!(second.contains("Ada"), "got {second}");
        assert!(second.contains("Nov 4"), "got {second}");
        assert!(second.contains("example.com/a"), "got {second}");
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
            "T",
            Some(false),
            Some(&author),
            Some("Nov 4"),
            Some("https://example.com/a/very/long/path/that/should/not/fit"),
            true,
            40,
            &colors,
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
    fn compact_header_fits_the_inner_width_at_every_width_from_zero_to_sixty() {
        let colors = ColorScheme::dark();
        let author = "Ada Lovelace and the Analytical Engine ".repeat(2);
        let title = "記事 ".repeat(30);
        let link = "https://example.com/a/very/long/path/that/keeps/going";
        for inner_width in 0..=60u16 {
            let lines = build_header_lines(
                &title,
                Some(false),
                Some(&author),
                Some("Nov 4, 2026"),
                Some(link),
                true,
                inner_width,
                &colors,
            );
            for line in &lines {
                assert!(
                    UnicodeWidthStr::width(line_text(line).as_str()) <= inner_width as usize,
                    "inner_width {inner_width}: {:?} overflows",
                    line_text(line)
                );
            }
        }
    }

    #[test]
    fn non_compact_header_keeps_spacers_and_separate_link_row() {
        let colors = ColorScheme::dark();
        let lines = build_header_lines(
            "Hello",
            Some(true),
            Some("Ada"),
            Some("Nov 4"),
            Some("https://example.com/a"),
            false,
            60,
            &colors,
        );
        assert_eq!(lines.len(), 5);
        assert_eq!(line_text(&lines[1]), "");
        assert!(line_text(&lines[2]).contains("Ada"));
        assert_eq!(line_text(&lines[3]), "");
        assert!(line_text(&lines[4]).starts_with('\u{1F517}'));
        assert!(!line_text(&lines[2]).contains("example.com"));
    }
}
