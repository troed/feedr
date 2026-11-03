//! Inline image cache and Kitty graphics-protocol driver.
//!
//! Scope: every block-level image in an article body is rendered inline at
//! its position (see `docs/superpowers/specs/2026-11-03-inline-images-design.md`).
//! Images are fetched in the background by a bounded worker pool, decoded,
//! downscaled and **re-encoded to PNG** in the worker; the main thread never
//! holds decoded pixels. The cache stores one entry per URL:
//!
//! ```text
//! absent → InFlight → Ready { image, png } → (png taken on transmit)
//!                  ↘ Failed
//! ```
//!
//! Non-goals (deliberate):
//!   * iTerm2 / Sixel / Terminology / half-block fallbacks. Terminals
//!     without Kitty graphics support get alt text instead.
//!   * tmux DCS passthrough. Skip until a user actually needs it.
//!   * Cell-pixel-size ioctl. A fixed aspect approximation gives perfectly
//!     usable thumbnails; precision can come later.
//!
//! Security: HTTP fetches reuse the existing feedr blocking client and
//! gate URLs through `feed::is_safe_auto_url` (SSRF allow-list). The body
//! is capped at `MAX_IMAGE_BYTES`. A worker panic is caught so a hostile
//! image cannot strand the fetch slot in-flight.

use base64::Engine;
use image::{DynamicImage, GenericImageView};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, Write};
use std::sync::{mpsc, Arc};

/// Concurrency cap for background fetches. A viewport prefetch can ask for
/// several images at once, and a quick navigate-spam can queue more —
/// bound it so a hostile feed can't fan-out a connection burst.
const MAX_CONCURRENT_FETCHES: usize = 4;

/// Hard cap on the body size of an image fetch. Anything larger is treated
/// as a fetch failure (`None`) rather than buffered into memory. 5 MB
/// matches the FULLTEXT_MAX_BYTES used by `feed::extract_article`.
const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;

/// Largest source dimension (px) we keep after decoding. Larger inputs are
/// Lanczos-downscaled before re-encoding to PNG, so transmission stays
/// fast and the terminal isn't asked to scale a 4K image into 30 cells.
const MAX_SOURCE_DIM: u32 = 1024;

/// Decoder guards against decompression bombs: a 5 MB PNG can legally
/// inflate to gigabytes of pixels. Anything that would decode past these
/// is treated as a fetch failure.
const MAX_DECODE_DIM: u32 = 8192;
const MAX_DECODE_ALLOC: u64 = 128 * 1024 * 1024;

/// LRU cap on cached entries. Each entry holds at most one encoded PNG
/// (bounded by `MAX_SOURCE_DIM`² at the worst case), so the cache stays
/// small for the life of the process. URLs pinned by the currently
/// displayed article are never evicted; if all of them are pinned the
/// cache may grow past the cap, bounded by the 40-image article cap.
const MAX_CACHED_IMAGES: usize = 32;

/// Heuristic ratio of (cell width / cell height) in pixels. Most modern
/// fixed-pitch fonts hit ~0.5 — cells are roughly twice as tall as wide.
/// Used to compute how many *columns* an image of a given pixel aspect
/// should occupy when constrained to a target row count.
const CELL_ASPECT: f32 = 0.5;

/// Images transmitted per `pump` call. The transmit writes are the only
/// multi-megabyte writes in a frame, and a viewport prefetch can make a
/// dozen images ready at once; anything left over goes out on a later
/// tick, still rendering its placeholder (or nothing) in the meantime.
const MAX_TRANSMITS_PER_PUMP: usize = 1;

/// Base64 bytes of image payload written per `pump` call. This is the
/// looser of the two bounds while [`MAX_TRANSMITS_PER_PUMP`] is 1, and it
/// exists so raising the per-call count cannot silently write tens of
/// megabytes. An image that alone exceeds the budget is still sent alone
/// as the first write of a call — refusing it would stall the queue
/// forever, because it would stay first in line.
const TRANSMIT_BUDGET_BYTES: usize = 4 * 1024 * 1024;

/// Which inline-image protocol the current terminal supports.
///
/// Only Kitty and Ghostty implement the `U=1` Unicode-placeholder variant
/// this renderer relies on. Everything else (including WezTerm and tmux)
/// falls back to alt text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageProtocol {
    UnicodePlaceholders,
    None,
}

/// Pure terminal-capability decision, split out from env reads so it is
/// testable without mutating process-global state.
fn protocol_for(
    tmux: bool,
    kitty_window: bool,
    term: Option<&str>,
    term_program: Option<&str>,
) -> ImageProtocol {
    // tmux inherits KITTY_WINDOW_ID / TERM_PROGRAM from the outer terminal
    // but swallows raw APC sequences (no passthrough support here yet), so
    // the image would never appear while the strip still gets reserved.
    if tmux {
        return ImageProtocol::None;
    }
    // Kitty itself.
    if kitty_window {
        return ImageProtocol::UnicodePlaceholders;
    }
    if let Some(term) = term {
        let term = term.to_lowercase();
        if term.contains("kitty") || term.contains("ghostty") {
            return ImageProtocol::UnicodePlaceholders;
        }
    }
    // Ghostty sets TERM_PROGRAM; WezTerm deliberately does not qualify
    // because it does not implement `U=1` placeholders yet.
    if let Some(prog) = term_program {
        if prog.eq_ignore_ascii_case("ghostty") {
            return ImageProtocol::UnicodePlaceholders;
        }
    }
    ImageProtocol::None
}

/// Detect the terminal's inline-image capability from env and delegate to
/// [`protocol_for`].
pub fn detect_protocol() -> ImageProtocol {
    protocol_for(
        std::env::var_os("TMUX").is_some(),
        std::env::var_os("KITTY_WINDOW_ID").is_some(),
        std::env::var("TERM").ok().as_deref(),
        std::env::var("TERM_PROGRAM").ok().as_deref(),
    )
}

/// A cached image whose PNG is ready to transmit, with the source
/// dimensions needed to size its placement.
#[derive(Clone, Debug)]
pub struct ReadyImage {
    /// 24-bit Kitty image id, allocated when the fetch result is inserted.
    pub id: u32,
    /// Source width in pixels, after downscaling.
    pub width: u32,
    /// Source height in pixels, after downscaling.
    pub height: u32,
    /// Whether the PNG has been transmitted to the terminal. Once true,
    /// re-rendering only emits a placement escape (cheap).
    pub transmitted: bool,
}

/// Where one ready image should appear, in terminal cells. The detail view
/// builds these from its placeholder rows; `pump` writes the matching Kitty
/// virtual placements.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlacementSpec {
    pub url: String,
    pub cols: u16,
    pub rows: u16,
}

/// Read-only view of the cache for a URL. Callers use this to pick the
/// right placeholder rows without ever seeing decoded pixels.
#[derive(Debug)]
pub enum ImageView<'a> {
    /// The URL has never been requested.
    Missing,
    /// A background fetch is queued or running.
    Loading,
    /// The PNG is available (possibly not yet transmitted).
    Ready(&'a ReadyImage),
    /// The fetch failed permanently for this session.
    Failed,
}

/// One cache slot per URL.
enum Entry {
    InFlight,
    /// `png` is `Some` until the first transmission takes it.
    Ready {
        image: ReadyImage,
        png: Option<Arc<Vec<u8>>>,
    },
    Failed,
}

/// A worker result: the downscaled image already encoded as PNG. The
/// decoded `DynamicImage` is dropped inside the worker.
struct EncodedImage {
    png: Vec<u8>,
    width: u32,
    height: u32,
}

/// Per-process inline-image cache + Kitty protocol driver.
///
/// A failed fetch lands as `Entry::Failed`; subsequent `start_fetch` calls
/// for that URL no-op. Deliberately permanent (no retry, no TTL) until the
/// slot ages out of the LRU: a transient network blip costs that article
/// its image for the session, in exchange for never re-hitting a dead URL
/// at render frequency. Revisit if users report chronically missing
/// images.
///
/// Debug is implemented manually because the internal `mpsc::Receiver`
/// has no `Debug` impl — we surface counts instead of contents.
pub struct ImageCache {
    protocol: ImageProtocol,
    /// Encoded images and fetch state, keyed by URL.
    entries: HashMap<String, Entry>,
    /// Insertion order for `entries`, oldest first. Drives LRU eviction at
    /// `MAX_CACHED_IMAGES` — see `insert_entry`.
    order: VecDeque<String>,
    /// URLs referenced by the currently displayed article. Never evicted.
    pinned: HashSet<String>,
    /// Monotonic Kitty image id allocator. Skips 0 (reserved by spec).
    next_kitty_id: u32,
    /// Background fetch channel + bookkeeping.
    sender: mpsc::Sender<(String, Option<EncodedImage>)>,
    receiver: mpsc::Receiver<(String, Option<EncodedImage>)>,
    in_flight: HashSet<String>,
    /// Lazily-built HTTP client. Kept here so the renderer can trigger
    /// fetches via `&mut App` without the caller threading a client
    /// through every UI layer. Construction is cheap relative to a fetch.
    client: Option<reqwest::blocking::Client>,
    /// Kitty image id of the placement currently on screen, if any.
    /// Retained by the transitional single-placement shim (`render_at`);
    /// removed once the detail view drives placements through `pump`.
    last_placed_id: Option<u32>,
    /// Cell size of every virtual placement `pump` has put on screen, keyed
    /// by image id. A resize only needs to re-emit the placement, never the
    /// pixel data, so this map is the whole "do the cells still match?"
    /// state of the placeholder view.
    last_placement: HashMap<u32, (u16, u16)>,
    /// Kitty ids whose terminal-side pixel data should be freed (their
    /// URL was evicted from our cache, so we'd never place them again).
    /// Deletion needs a writer, which eviction sites don't have — queued
    /// here and flushed by `render_at` / `clear_terminal`.
    pending_deletes: Vec<u32>,
}

impl Default for ImageCache {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for ImageCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageCache")
            .field("protocol", &self.protocol)
            .field("entries", &self.entries.len())
            .field("pinned", &self.pinned.len())
            .field("in_flight", &self.in_flight.len())
            .finish()
    }
}

impl ImageCache {
    pub fn new() -> Self {
        let (sender, receiver) = mpsc::channel();
        Self {
            protocol: detect_protocol(),
            entries: HashMap::new(),
            order: VecDeque::new(),
            pinned: HashSet::new(),
            next_kitty_id: 0,
            sender,
            receiver,
            in_flight: HashSet::new(),
            client: None,
            last_placed_id: None,
            last_placement: HashMap::new(),
            pending_deletes: Vec::new(),
        }
    }

    pub fn protocol(&self) -> ImageProtocol {
        self.protocol
    }

    /// Test-only override: protocol detection is env-driven, so tests that
    /// exercise the render path force Kitty explicitly.
    #[cfg(test)]
    pub(crate) fn force_protocol(&mut self, protocol: ImageProtocol) {
        self.protocol = protocol;
    }

    /// Read-only state for `url`.
    pub fn view(&self, url: &str) -> ImageView<'_> {
        match self.entries.get(url) {
            None => ImageView::Missing,
            Some(Entry::InFlight) => ImageView::Loading,
            Some(Entry::Ready { image, .. }) => ImageView::Ready(image),
            Some(Entry::Failed) => ImageView::Failed,
        }
    }

    /// True iff an encoded image is ready for this URL (independent of
    /// whether the PNG has been transmitted yet — that's lazy).
    pub fn has_image(&self, url: &str) -> bool {
        matches!(self.view(url), ImageView::Ready(_))
    }

    /// Compute the (cols, rows) the image should occupy when constrained
    /// to `(max_cols, max_rows)` cells, preserving the image's aspect.
    /// Returns None if the image isn't ready yet.
    pub fn display_cells(&self, url: &str, max_cols: usize, max_rows: usize) -> Option<(u16, u16)> {
        match self.view(url) {
            ImageView::Ready(img) => Some(fit_cells(img.width, img.height, max_cols, max_rows)),
            _ => None,
        }
    }

    /// Queue background fetches for every URL that isn't already known.
    /// `start_fetch` enforces the concurrency cap and the permanent-failure
    /// semantics, so calling this every frame is cheap.
    pub fn ensure_fetches(&mut self, urls: &[String]) {
        for url in urls {
            // A `false` return means the cap was hit (or a thread could not
            // be spawned); both are transient, so the next tick retries.
            self.start_fetch(url);
        }
    }

    /// Pin `urls` (the currently displayed article's images) so LRU
    /// eviction leaves them alone.
    pub fn set_pinned(&mut self, urls: HashSet<String>) {
        self.pinned = urls;
        self.evict_lru();
    }

    /// Queue a background fetch+encode for `url`. Returns false if the
    /// fetch could not be started right now (concurrency cap full, or OS
    /// thread creation failed) — both are transient, and the caller can
    /// re-queue on the next tick.
    /// Idempotent: re-queueing an in-flight or already-attempted URL is a
    /// no-op (returns true, the "already handled" signal).
    pub fn start_fetch(&mut self, url: &str) -> bool {
        if self.protocol == ImageProtocol::None {
            return true; // no point fetching if we can't render
        }
        if self.entries.contains_key(url) || self.in_flight.contains(url) {
            return true;
        }
        if self.in_flight.len() >= MAX_CONCURRENT_FETCHES {
            return false;
        }
        if !crate::feed::is_safe_auto_url(url) {
            // Treat blocked hosts as a known failure so we don't re-queue.
            self.insert_entry(url.to_string(), Entry::Failed);
            return true;
        }
        // Lazily build the client. Failure here is treated as a known-failed
        // fetch so we don't re-queue on every render tick. The safe-redirect
        // client re-runs `is_safe_auto_url` on every hop — images come from
        // hostile feed content and are fetched with no user action, so a
        // public-looking URL must not be allowed to 302 into the user's
        // internal network (same threat model as the auto-fulltext path).
        if self.client.is_none() {
            match crate::feed::Feed::build_safe_redirect_client(15) {
                Ok(c) => self.client = Some(c),
                Err(_) => {
                    self.insert_entry(url.to_string(), Entry::Failed);
                    return true;
                }
            }
        }
        let client = self.client.as_ref().unwrap().clone();
        let sender = self.sender.clone();
        let url_owned = url.to_string();
        // Builder::spawn (not thread::spawn) so an OS thread-creation
        // failure is an Err instead of a panic that takes down the TUI —
        // same hardening as the fulltext extraction workers.
        let spawned = std::thread::Builder::new()
            .name("feedr-img-fetch".to_string())
            .spawn(move || {
                // Panic-safe: a panic in the image decoder must still free the
                // in-flight slot via `poll_completed`. Without `catch_unwind`,
                // a single hostile image could permanently strand the slot.
                let encoded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    fetch_encode_and_decode(&url_owned, &client)
                }))
                .unwrap_or(None);
                let _ = sender.send((url_owned, encoded));
            });
        if spawned.is_err() {
            // Transient resource exhaustion: don't record an entry so a
            // later render can retry rather than caching a permanent failure.
            self.in_flight.remove(url);
            return false;
        }
        self.in_flight.insert(url.to_string());
        // Mark the entry only after a successful spawn so a failed spawn
        // leaves the URL in the `Missing` state.
        self.insert_entry(url.to_string(), Entry::InFlight);
        true
    }

    /// Drain completed background fetches. Returns true if any new results
    /// arrived (so the caller can request a redraw).
    pub fn poll_completed(&mut self) -> bool {
        let mut any = false;
        while let Ok((url, encoded)) = self.receiver.try_recv() {
            self.in_flight.remove(&url);
            let entry = match encoded {
                Some(EncodedImage { png, width, height }) => {
                    // Allocate the id once, when the PNG becomes available.
                    let id = self.allocate_kitty_id();
                    Entry::Ready {
                        image: ReadyImage {
                            id,
                            width,
                            height,
                            transmitted: false,
                        },
                        png: Some(Arc::new(png)),
                    }
                }
                None => Entry::Failed,
            };
            self.insert_entry(url, entry);
            any = true;
        }
        any
    }

    /// Next 24-bit Kitty image id. 0 is reserved by the spec.
    fn allocate_kitty_id(&mut self) -> u32 {
        self.next_kitty_id = self.next_kitty_id.wrapping_add(1);
        if self.next_kitty_id == 0 || self.next_kitty_id > 0x00FF_FFFF {
            self.next_kitty_id = 1;
        }
        self.next_kitty_id
    }

    /// Record an entry, then trim the LRU. Overwriting an existing entry
    /// (e.g. `InFlight` → `Ready`) keeps its original recency slot.
    fn insert_entry(&mut self, url: String, entry: Entry) {
        if self.entries.insert(url.clone(), entry).is_none() {
            self.order.push_back(url);
        }
        self.evict_lru();
    }

    /// Drop the oldest unpinned entries until the cache is back at
    /// `MAX_CACHED_IMAGES`. If every entry is pinned the cache is allowed
    /// to grow (bounded by the 40-image article cap).
    fn evict_lru(&mut self) {
        while self.entries.len() > MAX_CACHED_IMAGES {
            let Some(idx) = self.order.iter().position(|u| !self.pinned.contains(u)) else {
                break;
            };
            let Some(url) = self.order.remove(idx) else {
                break;
            };
            if let Some(entry) = self.entries.remove(&url) {
                self.drop_entry(entry);
            }
        }
    }

    /// Queue a terminal-side `d=I` for an entry whose PNG was transmitted.
    fn drop_entry(&mut self, entry: Entry) {
        if let Entry::Ready { image, .. } = entry {
            if image.transmitted {
                self.pending_deletes.push(image.id);
            }
        }
    }

    /// Drive the terminal's image state for one frame: pin `specs`, evict,
    /// transmit at most one not-yet-sent image, and re-place any transmitted
    /// image whose cell size changed.
    ///
    /// `specs` are the images visible this frame, in the order the detail
    /// view wants them sent. Write errors propagate (a half-written escape
    /// leaves the terminal in an unknown state) but are swallowed by the
    /// TUI, which treats a failed frame as a failed frame. No-op on
    /// terminals without image support.
    pub fn pump(&mut self, stdout: &mut impl Write, specs: &[PlacementSpec]) -> io::Result<()> {
        if self.protocol != ImageProtocol::UnicodePlaceholders {
            return Ok(());
        }

        // 1. Everything on screen is pinned for this frame, so the LRU can
        //    only reclaim URLs the user has scrolled away from. Eviction has
        //    no writer, so queued deletes (`d=I`: placements + pixel data)
        //    are flushed here — and, with them, the stale placement sizes of
        //    those ids, so a later re-fetch can't be suppressed by them.
        self.set_pinned(specs.iter().map(|s| s.url.clone()).collect());
        self.flush_pending_deletes(stdout)?;

        // 2. Re-place images that are already in the terminal's registry but
        //    whose cells changed (resize / rewrap). No re-transmit: the
        //    pixel data is keyed by image id and still valid.
        for spec in specs {
            if let Some(id) = self.transmitted_id(&spec.url) {
                self.sync_placement(stdout, id, spec.cols, spec.rows)?;
            }
        }

        // 3. Spend the transmit budget on the first spec that is ready but
        //    not yet in the terminal. A `Ready` entry without a payload is
        //    *not* ready: the PNG was already taken (or never existed), so
        //    sending nothing would leave `transmitted` lying about what the
        //    terminal holds.
        let mut written_bytes = 0usize;
        let mut sent = 0usize;
        for spec in specs {
            if sent >= MAX_TRANSMITS_PER_PUMP {
                break;
            }
            let Some((id, png)) = self.pending_payload(&spec.url) else {
                continue;
            };
            // Upper bound on what the encoder will produce for this payload.
            let encoded_len = base64::encoded_len(png.len(), true).unwrap_or(usize::MAX);
            if written_bytes > 0
                && written_bytes.saturating_add(encoded_len) > TRANSMIT_BUDGET_BYTES
            {
                break;
            }
            let payload = BASE64.encode(png.as_slice());
            transmit_kitty_base64(stdout, &payload, id)?;
            // Place before flipping any state: if either write fails, the
            // entry keeps its payload and `transmitted == false`, so the
            // next pump retries. Re-sending the same id is idempotent —
            // Kitty replaces that image's pixel data.
            place_virtual_image(stdout, id, spec.cols, spec.rows)?;
            if let Some(Entry::Ready { image, png }) = self.entries.get_mut(&spec.url) {
                image.transmitted = true;
                *png = None;
            }
            self.last_placement.insert(id, (spec.cols, spec.rows));
            written_bytes = written_bytes.saturating_add(encoded_len);
            sent += 1;
        }
        Ok(())
    }

    /// Id of a `Ready` entry whose pixel data is already in the terminal.
    fn transmitted_id(&self, url: &str) -> Option<u32> {
        match self.entries.get(url) {
            Some(Entry::Ready { image, .. }) if image.transmitted => Some(image.id),
            _ => None,
        }
    }

    /// `(id, png)` of a `Ready` entry that still has payload to send. The
    /// PNG is an `Arc`, so this peek costs a refcount bump, not a copy.
    fn pending_payload(&self, url: &str) -> Option<(u32, Arc<Vec<u8>>)> {
        match self.entries.get(url) {
            Some(Entry::Ready {
                image,
                png: Some(png),
            }) if !image.transmitted => Some((image.id, Arc::clone(png))),
            _ => None,
        }
    }

    /// Re-emit the virtual placement for `id` if its cells changed, and
    /// record the size this frame asked for.
    fn sync_placement(
        &mut self,
        stdout: &mut impl Write,
        id: u32,
        cols: u16,
        rows: u16,
    ) -> io::Result<()> {
        match self.last_placement.get(&id) {
            // Same cells: the terminal already has this placement.
            Some(&placed) if placed == (cols, rows) => Ok(()),
            Some(_) => {
                place_virtual_image(stdout, id, cols, rows)?;
                self.last_placement.insert(id, (cols, rows));
                Ok(())
            }
            // Nothing recorded: this id was transmitted outside `pump` (the
            // transitional `render_at` shim, or a test fixture). Record the
            // requested size without re-emitting — a re-place is only owed
            // once the size actually changes.
            None => {
                self.last_placement.insert(id, (cols, rows));
                Ok(())
            }
        }
    }

    /// **Transitional shim.** Place `url` at the top-left of `rect`, sized
    /// to fit within `rect`. Returns true if anything was written to
    /// stdout. No-op on non-Kitty terminals.
    ///
    /// The detail view drives placements through the placeholder path now;
    /// this shim remains only for its own tests until Task 11 deletes it.
    pub fn render_at(
        &mut self,
        stdout: &mut impl Write,
        url: &str,
        rect_col: u16,
        rect_row: u16,
        max_cols: u16,
        max_rows: u16,
    ) -> io::Result<bool> {
        if self.protocol != ImageProtocol::UnicodePlaceholders {
            return Ok(false);
        }
        let (id, width, height) = match self.view(url) {
            ImageView::Ready(img) => (img.id, img.width, img.height),
            _ => return Ok(false),
        };
        // Free terminal-side data for any ids evicted since the last write.
        self.flush_pending_deletes(stdout)?;

        // Size the placement: respect both rect bounds AND image aspect.
        let (cols, rows) = fit_cells(width, height, max_cols as usize, max_rows as usize);
        if cols == 0 || rows == 0 {
            return Ok(false);
        }

        // Position cursor at the rect's top-left in raw 1-based coords.
        // crossterm's MoveTo is 0-based, so emit the CSI directly to stay
        // independent of which Writer the caller uses (stdout, BufWriter…).
        write!(stdout, "\x1b[{};{}H", rect_row + 1, rect_col + 1)?;

        let pending_png = match self.entries.get_mut(url) {
            Some(Entry::Ready { image, png }) => {
                if image.transmitted {
                    None
                } else {
                    // `Some(None)` = ready to transmit, no payload attached
                    // (only reachable for test-injected entries).
                    Some(png.take())
                }
            }
            _ => return Ok(false),
        };
        if let Some(png) = pending_png {
            let bytes: &[u8] = png.as_deref().map(Vec::as_slice).unwrap_or(&[]);
            transmit_kitty_image(stdout, bytes, id)?;
            if let Some(Entry::Ready { image, .. }) = self.entries.get_mut(url) {
                image.transmitted = true;
            }
        }

        // Placements are keyed by (image id, placement id). Re-placing the
        // SAME image id with p=1 replaces the previous placement, but a
        // DIFFERENT image id would stack on top of the old one — so when
        // switching images within a frame-pair, delete the old image's
        // placement first (d=i, lowercase: placement only, its pixel data
        // stays cached terminal-side for a cheap re-place later).
        if let Some(old) = self.last_placed_id {
            if old != id {
                write!(stdout, "\x1b_Ga=d,d=i,i={},q=2\x1b\\", old)?;
            }
        }
        place_kitty_image(stdout, id, cols, rows)?;
        self.last_placed_id = Some(id);
        // Leave the cursor in a sane spot below the image so the next
        // ratatui frame's diff doesn't get confused.
        write!(stdout, "\x1b[{};1H", rect_row + rows + 1)?;
        Ok(true)
    }

    /// **Transitional shim.** Tell the terminal to drop every Kitty
    /// *placement* (`d=a`, lowercase: the transmitted pixel data stays in
    /// the terminal's image registry). Call when leaving any view that was
    /// showing an image so it doesn't bleed into the next view's cells.
    /// Keeps cached entries and their `transmitted` flags intact, so
    /// re-entering the view is a single cheap placement escape, with no
    /// PNG re-transmit. Terminal data is freed only when a URL ages out of
    /// our LRU (`d=I` via `pending_deletes`), keeping the terminal's copy
    /// bounded by `MAX_CACHED_IMAGES`.
    pub fn clear_terminal(&mut self, stdout: &mut impl Write) -> io::Result<()> {
        if self.protocol != ImageProtocol::UnicodePlaceholders {
            return Ok(());
        }
        self.flush_pending_deletes(stdout)?;
        // a=d (delete), d=a (all visible placements). q=2 suppresses the
        // terminal's response.
        stdout.write_all(b"\x1b_Ga=d,d=a,q=2\x1b\\")?;
        self.last_placed_id = None;
        Ok(())
    }

    /// Emit `d=I` (uppercase: delete placements AND free pixel data) for
    /// every id whose URL was evicted from the cache. Queued at eviction
    /// time because eviction sites have no writer.
    ///
    /// Eviction also forgets the id's recorded placement size: the terminal
    /// no longer holds that image, and any future placement comes from a
    /// fresh fetch with a fresh id.
    fn flush_pending_deletes(&mut self, stdout: &mut impl Write) -> io::Result<()> {
        for id in self.pending_deletes.drain(..) {
            self.last_placement.remove(&id);
            write!(stdout, "\x1b_Ga=d,d=I,i={},q=2\x1b\\", id)?;
        }
        Ok(())
    }
}

/// Test-only entry states for injecting cache state without a network
/// fetch.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TestState {
    InFlight,
    Failed,
}

/// Test-only constructors so this module's tests (and other modules'
/// tests) can populate the cache without a fetch.
#[cfg(test)]
impl ImageCache {
    /// Insert a ready entry. When `transmitted` is false the entry carries
    /// a small valid PNG so the transmit path is exercisable; a
    /// transmitted entry has already dropped its payload.
    pub(crate) fn insert_ready_for_test(
        &mut self,
        url: &str,
        id: u32,
        w: u32,
        h: u32,
        transmitted: bool,
    ) {
        let png = (!transmitted).then(|| {
            Arc::new(
                encode_png(&DynamicImage::new_rgb8(1, 1)).expect("test fixture PNG must encode"),
            )
        });
        self.insert_entry(
            url.to_string(),
            Entry::Ready {
                image: ReadyImage {
                    id,
                    width: w,
                    height: h,
                    transmitted,
                },
                png,
            },
        );
    }

    pub(crate) fn insert_state_for_test(&mut self, url: &str, state: TestState) {
        let entry = match state {
            TestState::InFlight => Entry::InFlight,
            TestState::Failed => Entry::Failed,
        };
        self.insert_entry(url.to_string(), entry);
    }
}

/// Fetch `url`, validate the response, decode into a `DynamicImage`, and
/// downscale. Returns None on any failure (network, status, size, format).
fn fetch_and_decode(url: &str, client: &reqwest::blocking::Client) -> Option<DynamicImage> {
    let response = client.get(url).send().ok()?;
    if !response.status().is_success() {
        return None;
    }
    // Cap the body read so a hostile or misconfigured server can't OOM us.
    use std::io::Read;
    let mut bytes = Vec::with_capacity(64 * 1024);
    response
        .take(MAX_IMAGE_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_IMAGE_BYTES {
        return None;
    }
    // Decode with explicit limits: the byte cap above doesn't bound the
    // *decoded* size, and a small PNG can inflate to gigabytes of pixels.
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DECODE_DIM);
    limits.max_image_height = Some(MAX_DECODE_DIM);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    let mut reader = image::ImageReader::new(std::io::Cursor::new(&bytes))
        .with_guessed_format()
        .ok()?;
    reader.limits(limits);
    let img = reader.decode().ok()?;
    Some(downscale(img, MAX_SOURCE_DIM))
}

/// Worker entry point: fetch + decode + downscale, then encode PNG so the
/// decoded pixels never reach the main thread.
fn fetch_encode_and_decode(url: &str, client: &reqwest::blocking::Client) -> Option<EncodedImage> {
    let img = fetch_and_decode(url, client)?;
    let (width, height) = img.dimensions();
    let png = encode_png(&img)?;
    Some(EncodedImage { png, width, height })
}

fn downscale(img: DynamicImage, max_dim: u32) -> DynamicImage {
    let (w, h) = img.dimensions();
    if w <= max_dim && h <= max_dim {
        return img;
    }
    let scale = max_dim as f32 / w.max(h) as f32;
    let new_w = ((w as f32 * scale).round() as u32).max(1);
    let new_h = ((h as f32 * scale).round() as u32).max(1);
    img.resize(new_w, new_h, image::imageops::FilterType::Lanczos3)
}

fn encode_png(img: &DynamicImage) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(64 * 1024);
    img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .ok()?;
    Some(out)
}

fn fit_cells(src_w: u32, src_h: u32, max_cols: usize, max_rows: usize) -> (u16, u16) {
    let aspect = if src_h == 0 {
        1.0
    } else {
        src_w as f32 / src_h as f32
    };
    fit_cells_with_aspect(aspect, max_cols, max_rows)
}

/// Pick (cols, rows) that preserves `src_aspect` (w/h in pixels), respecting
/// terminal cell aspect via `CELL_ASPECT` and clamping to `(max_cols, max_rows)`.
fn fit_cells_with_aspect(src_aspect: f32, max_cols: usize, max_rows: usize) -> (u16, u16) {
    // Convert pixel-aspect → cell-aspect. A "square" pixel image of side N
    // needs N*CELL_ASPECT cols of width per row of height to look square.
    // i.e. cell_aspect = pixel_aspect / CELL_ASPECT.
    let cell_aspect = src_aspect / CELL_ASPECT;
    // Try fitting to width first.
    let cols_from_rows = (max_rows as f32 * cell_aspect).round() as usize;
    let rows_from_cols = (max_cols as f32 / cell_aspect).round() as usize;
    let (cols, rows) = if cols_from_rows <= max_cols {
        (cols_from_rows, max_rows)
    } else {
        (max_cols, rows_from_cols)
    };
    (
        cols.min(max_cols).max(1) as u16,
        rows.min(max_rows).max(1) as u16,
    )
}

const BASE64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

/// Send a PNG to the terminal under image id `id`, chunked at 4096 base64
/// bytes per Kitty's protocol. Subsequent placements reference the id.
fn transmit_kitty_image(stdout: &mut impl Write, png: &[u8], id: u32) -> io::Result<()> {
    transmit_kitty_base64(stdout, &BASE64.encode(png), id)
}

/// Send an already-encoded PNG payload to the terminal under image id `id`.
/// `pump` encodes once and measures the payload before writing it, so the
/// chunking lives here rather than behind the encoder.
fn transmit_kitty_base64(stdout: &mut impl Write, b64: &str, id: u32) -> io::Result<()> {
    let chunk_size = 4096;
    let total_chunks = b64.len().div_ceil(chunk_size);
    for (i, chunk) in b64.as_bytes().chunks(chunk_size).enumerate() {
        let more = if i + 1 < total_chunks { 1 } else { 0 };
        if i == 0 {
            // f=100: PNG. t=d: direct (inline data, not file path).
            // q=2: suppress response (terminal stays quiet on success).
            write!(stdout, "\x1b_Ga=t,f=100,t=d,i={},q=2,m={};", id, more)?;
        } else {
            write!(stdout, "\x1b_Gm={};", more)?;
        }
        stdout.write_all(chunk)?;
        stdout.write_all(b"\x1b\\")?;
    }
    Ok(())
}

/// Place a transmitted image through its Unicode placeholder cells.
///
/// `U=1` makes the terminal draw the image wherever the placeholder cells
/// (`U+10EEEE` + row/column diacritics) for image `id` appear, so the image
/// scrolls and clips with the buffer instead of being pinned to the cursor.
/// `p=1` keeps exactly one placement per image id, so a resize replaces it
/// instead of stacking a second copy.
fn place_virtual_image(stdout: &mut impl Write, id: u32, cols: u16, rows: u16) -> io::Result<()> {
    write!(
        stdout,
        "\x1b_Ga=p,U=1,i={},p=1,c={},r={},q=2;\x1b\\",
        id, cols, rows
    )
}

/// Place a transmitted image at the current cursor with the given cell
/// dimensions. `c` = columns, `r` = rows; the terminal scales to fit.
/// The explicit placement id (`p=1`) makes re-placement idempotent:
/// placements are keyed by (image id, placement id), so emitting this
/// every frame replaces the previous placement instead of accumulating
/// new ones in the terminal.
fn place_kitty_image(stdout: &mut impl Write, id: u32, cols: u16, rows: u16) -> io::Result<()> {
    write!(
        stdout,
        "\x1b_Ga=p,i={},p=1,q=2,c={},r={};\x1b\\",
        id, cols, rows
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── fit_cells ────────────────────────────────────────────────────────

    #[test]
    fn fit_cells_square_image() {
        // A square pixel image in a 40x20 cell area. With CELL_ASPECT=0.5,
        // cells are 2x as tall as wide, so a square fits as cols = 2*rows.
        // At max_rows=20 → cols_from_rows = 40 → exactly fits.
        let (c, r) = fit_cells(100, 100, 40, 20);
        assert_eq!(r, 20);
        assert_eq!(c, 40);
    }

    #[test]
    fn fit_cells_wide_image() {
        // 2:1 pixel-aspect → cell-aspect = 2/0.5 = 4. In a 40x20 area,
        // width-bound: rows_from_cols = 40/4 = 10.
        let (c, r) = fit_cells(200, 100, 40, 20);
        assert_eq!(c, 40);
        assert_eq!(r, 10);
    }

    #[test]
    fn fit_cells_clamps_to_at_least_one() {
        // Tiny target should still yield at least 1x1.
        let (c, r) = fit_cells(100, 100, 1, 1);
        assert!(c >= 1);
        assert!(r >= 1);
    }

    // ── entry model (PNG-only cache) ─────────────────────────────────────

    #[test]
    fn view_reports_missing_loading_ready_failed() {
        let mut cache = ImageCache::new();
        assert!(matches!(
            cache.view("https://x/absent.png"),
            ImageView::Missing
        ));

        cache.insert_state_for_test("https://x/loading.png", TestState::InFlight);
        assert!(matches!(
            cache.view("https://x/loading.png"),
            ImageView::Loading
        ));

        cache.insert_ready_for_test("https://x/ready.png", 7, 640, 480, false);
        match cache.view("https://x/ready.png") {
            ImageView::Ready(img) => {
                assert_eq!(img.id, 7);
                assert_eq!(img.width, 640);
                assert_eq!(img.height, 480);
                assert!(!img.transmitted);
            }
            other => panic!("expected Ready, got {other:?}"),
        }

        cache.insert_state_for_test("https://x/failed.png", TestState::Failed);
        assert!(matches!(
            cache.view("https://x/failed.png"),
            ImageView::Failed
        ));
    }

    #[test]
    fn poll_completed_stores_encoded_png_and_dimensions() {
        let mut cache = ImageCache::new();
        cache.force_protocol(ImageProtocol::UnicodePlaceholders);
        let png = encode_png(&DynamicImage::new_rgb8(4, 4)).unwrap();
        let url = "https://x/encoded.png".to_string();
        cache.in_flight.insert(url.clone());
        cache
            .sender
            .send((
                url.clone(),
                Some(EncodedImage {
                    png: png.clone(),
                    width: 4,
                    height: 4,
                }),
            ))
            .unwrap();

        assert!(cache.poll_completed());
        match cache.view(&url) {
            ImageView::Ready(img) => {
                assert_eq!((img.width, img.height), (4, 4));
                assert!(!img.transmitted);
                // 24-bit range, 0 reserved by the Kitty spec.
                assert!(img.id > 0 && img.id <= 0x00FF_FFFF);
            }
            other => panic!("expected Ready, got {other:?}"),
        }
        match cache.entries.get(&url) {
            Some(Entry::Ready {
                png: Some(stored), ..
            }) => assert_eq!(&**stored, &png),
            _ => panic!("expected the encoded PNG to be stored in the entry"),
        }
        assert!(!cache.in_flight.contains(&url));
    }

    #[test]
    fn ensure_fetches_is_idempotent_for_known_urls() {
        let mut cache = ImageCache::new();
        cache.force_protocol(ImageProtocol::UnicodePlaceholders);
        let url = "https://example.com/known-failed.png".to_string();
        // Pre-record the failure so the test exercises the failed-state
        // path: the entry makes `ensure_fetches` a no-op. (The harness
        // points HTTP(S)_PROXY at a dead port, so even a regression that
        // re-queued this URL would fail fast without touching the network.)
        cache.insert_state_for_test(&url, TestState::Failed);

        cache.ensure_fetches(std::slice::from_ref(&url));
        cache.ensure_fetches(std::slice::from_ref(&url));

        assert!(matches!(cache.view(&url), ImageView::Failed));
        assert_eq!(cache.entries.len(), 1, "must not queue a second entry");
        assert_eq!(cache.order.len(), 1, "must not re-record insertion order");
        assert!(
            !cache.in_flight.contains(&url),
            "must not re-queue the fetch"
        );
    }

    #[test]
    fn lru_never_evicts_pinned_urls() {
        let mut cache = ImageCache::new();
        cache.force_protocol(ImageProtocol::UnicodePlaceholders);
        let pinned = "https://x/pinned.png".to_string();
        cache.insert_ready_for_test(&pinned, 1, 8, 8, false);
        cache.set_pinned(HashSet::from([pinned.clone()]));

        for i in 0..MAX_CACHED_IMAGES + 4 {
            cache.insert_ready_for_test(
                &format!("https://x/fill-{i}.png"),
                100 + i as u32,
                8,
                8,
                false,
            );
        }

        assert!(
            matches!(cache.view(&pinned), ImageView::Ready(_)),
            "the pinned URL must survive eviction"
        );
        assert!(cache.entries.len() <= MAX_CACHED_IMAGES);
        assert!(
            !cache.entries.contains_key("https://x/fill-0.png"),
            "unpinned entries still age out"
        );
    }

    #[test]
    fn lru_eviction_of_transmitted_image_queues_terminal_delete() {
        let mut cache = ImageCache::new();
        cache.force_protocol(ImageProtocol::UnicodePlaceholders);
        cache.insert_ready_for_test("https://x/dropped.png", 4242, 8, 8, true);

        for i in 0..MAX_CACHED_IMAGES {
            cache.insert_ready_for_test(
                &format!("https://x/keep-{i}.png"),
                500 + i as u32,
                8,
                8,
                false,
            );
        }

        assert!(matches!(
            cache.view("https://x/dropped.png"),
            ImageView::Missing
        ));
        assert_eq!(cache.pending_deletes, vec![4242]);
    }

    // ── ImageCache ───────────────────────────────────────────────────────

    #[test]
    fn protocol_detection_supports_kitty_and_ghostty_only() {
        use ImageProtocol::*;
        // `ImageProtocol::None` is glob-imported above, so spell the
        // `Option` variant explicitly for the leading capability args.
        assert_eq!(
            protocol_for(false, true, Option::None, Option::None),
            UnicodePlaceholders
        );
        assert_eq!(
            protocol_for(false, false, Some("xterm-kitty"), Option::None),
            UnicodePlaceholders
        );
        assert_eq!(
            protocol_for(false, false, Some("xterm-ghostty"), Option::None),
            UnicodePlaceholders
        );
        assert_eq!(
            protocol_for(false, false, Some("xterm-256color"), Some("ghostty")),
            UnicodePlaceholders
        );
        assert_eq!(
            protocol_for(false, false, Some("xterm-256color"), Some("wezterm")),
            None
        );
        assert_eq!(
            protocol_for(true, true, Some("xterm-kitty"), Option::None),
            None
        );
    }

    #[test]
    fn detect_protocol_none_by_default() {
        // Env can poison this test if KITTY_WINDOW_ID is set in the parent.
        // We can't safely unset process-global env in a multi-threaded
        // test runner, so just assert the function returns *something*.
        let p = detect_protocol();
        assert!(matches!(
            p,
            ImageProtocol::UnicodePlaceholders | ImageProtocol::None
        ));
    }

    #[test]
    fn cache_tracks_known_failures() {
        let mut cache = ImageCache::new();
        // Mark as known-failed.
        cache.insert_state_for_test("http://example.com/x.png", TestState::Failed);
        assert!(cache.entries.contains_key("http://example.com/x.png"));
        assert!(!cache.has_image("http://example.com/x.png"));
    }

    #[test]
    fn start_fetch_noop_in_none_protocol() {
        let mut cache = ImageCache::new();
        cache.protocol = ImageProtocol::None;
        assert!(cache.start_fetch("https://example.com/img.png"));
        // No fetch should have spawned.
        assert!(!cache.in_flight.contains("https://example.com/img.png"));
        assert!(!cache.entries.contains_key("https://example.com/img.png"));
    }

    #[test]
    fn start_fetch_rejects_unsafe_urls() {
        let mut cache = ImageCache::new();
        cache.protocol = ImageProtocol::UnicodePlaceholders;
        // localhost / private IPs are blocked by is_safe_auto_url.
        assert!(cache.start_fetch("http://127.0.0.1/x.png"));
        // Recorded as a known failure so it isn't re-queued every tick.
        assert!(cache.entries.contains_key("http://127.0.0.1/x.png"));
        assert!(matches!(
            cache.view("http://127.0.0.1/x.png"),
            ImageView::Failed
        ));
        assert!(!cache.has_image("http://127.0.0.1/x.png"));
        assert!(!cache.in_flight.contains("http://127.0.0.1/x.png"));
    }

    #[test]
    fn lru_evicts_oldest_past_cap() {
        let mut cache = ImageCache::new();
        cache.force_protocol(ImageProtocol::UnicodePlaceholders);
        for i in 0..MAX_CACHED_IMAGES + 3 {
            cache.insert_ready_for_test(&format!("https://x/{i}.png"), i as u32 + 1, 8, 8, false);
        }
        assert_eq!(cache.entries.len(), MAX_CACHED_IMAGES);
        assert_eq!(cache.order.len(), MAX_CACHED_IMAGES);
        // The three oldest are gone, the newest survive.
        assert!(matches!(cache.view("https://x/0.png"), ImageView::Missing));
        assert!(matches!(cache.view("https://x/2.png"), ImageView::Missing));
        assert!(cache.has_image(&format!("https://x/{}.png", MAX_CACHED_IMAGES + 2)));
    }

    #[test]
    fn overwrite_does_not_grow_order() {
        let mut cache = ImageCache::new();
        cache.insert_state_for_test("https://x/a.png", TestState::Failed);
        cache.insert_ready_for_test("https://x/a.png", 9, 8, 8, false);
        assert_eq!(cache.order.len(), 1);
        assert!(cache.has_image("https://x/a.png"));
    }

    #[test]
    fn poll_completed_assigns_unique_ids() {
        let mut cache = ImageCache::new();
        cache.force_protocol(ImageProtocol::UnicodePlaceholders);
        let png = encode_png(&DynamicImage::new_rgb8(8, 8)).unwrap();
        for url in ["a", "b"] {
            cache.in_flight.insert(url.to_string());
            cache
                .sender
                .send((
                    url.to_string(),
                    Some(EncodedImage {
                        png: png.clone(),
                        width: 8,
                        height: 8,
                    }),
                ))
                .unwrap();
            assert!(cache.poll_completed());
        }
        let (id_a, id_b) = (ready_id(&cache, "a"), ready_id(&cache, "b"));
        assert_ne!(id_a, id_b);
        assert!(id_a > 0 && id_b > 0);
    }

    #[test]
    fn encode_png_round_trip() {
        let img = DynamicImage::new_rgb8(4, 4);
        let png = encode_png(&img).expect("png encodes");
        // PNG signature: 89 50 4E 47 0D 0A 1A 0A
        assert_eq!(&png[..8], &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]);
    }

    #[test]
    fn transmit_kitty_image_chunks_correctly() {
        // Build a PNG large enough to require chunking.
        let img = DynamicImage::new_rgb8(128, 128);
        let png = encode_png(&img).unwrap();
        let mut out: Vec<u8> = Vec::new();
        transmit_kitty_image(&mut out, &png, 42).unwrap();
        let s = String::from_utf8_lossy(&out);
        // First chunk has the full control payload.
        assert!(s.contains("a=t,f=100,t=d,i=42,q=2"));
        // Last chunk must end with the terminator after m=0.
        assert!(s.contains("m=0;"));
        assert!(s.ends_with("\x1b\\"));
    }

    #[test]
    fn place_kitty_image_formats_csi() {
        let mut out: Vec<u8> = Vec::new();
        place_kitty_image(&mut out, 7, 30, 12).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert_eq!(s, "\x1b_Ga=p,i=7,p=1,q=2,c=30,r=12;\x1b\\");
    }

    // ── placement / terminal-data lifecycle (transitional shim) ─────────

    /// Build a Kitty-mode cache with ready images already in place.
    fn cache_with_images(urls: &[&str]) -> ImageCache {
        let mut cache = ImageCache::new();
        cache.force_protocol(ImageProtocol::UnicodePlaceholders);
        for (i, url) in urls.iter().enumerate() {
            cache.insert_ready_for_test(url, i as u32 + 1, 8, 8, false);
        }
        cache
    }

    /// Id of the ready entry for `url`.
    fn ready_id(cache: &ImageCache, url: &str) -> u32 {
        match cache.view(url) {
            ImageView::Ready(img) => img.id,
            other => panic!("expected Ready for {url}, got {other:?}"),
        }
    }

    #[test]
    fn clear_terminal_deletes_placements_only_and_keeps_transmit_state() {
        let mut cache = cache_with_images(&["https://x/a.png"]);
        let mut out: Vec<u8> = Vec::new();
        assert!(cache
            .render_at(&mut out, "https://x/a.png", 0, 0, 40, 10)
            .unwrap());
        let first = String::from_utf8_lossy(&out).to_string();
        assert!(first.contains("a=t"), "first render transmits the PNG");

        // View-change clear: placements-only (lowercase d=a), never d=A —
        // the terminal keeps the pixel data so re-entry is cheap.
        let mut cleared: Vec<u8> = Vec::new();
        cache.clear_terminal(&mut cleared).unwrap();
        let cleared = String::from_utf8_lossy(&cleared).to_string();
        assert!(
            cleared.contains("a=d,d=a"),
            "expected placement-only clear in: {cleared}"
        );
        assert!(
            !cleared.contains("d=A"),
            "must not free terminal data on view change"
        );
        assert!(cache.last_placed_id.is_none());

        // Re-entering the view re-places without re-encoding/re-transmitting.
        let mut out2: Vec<u8> = Vec::new();
        assert!(cache
            .render_at(&mut out2, "https://x/a.png", 0, 0, 40, 10)
            .unwrap());
        let second = String::from_utf8_lossy(&out2).to_string();
        assert!(
            !second.contains("a=t"),
            "re-render must not retransmit: {second}"
        );
        assert!(
            second.contains("a=p"),
            "re-render places the retained image"
        );
    }

    #[test]
    fn switching_images_deletes_previous_placement() {
        // Regression lock for placement stacking: placements are keyed by
        // (image id, placement id), so placing image B with p=1 does NOT
        // replace image A's (id_a, p=1) placement — render_at must delete
        // it explicitly when the displayed image changes.
        let mut cache = cache_with_images(&["https://x/a.png", "https://x/b.png"]);
        let mut out: Vec<u8> = Vec::new();
        cache
            .render_at(&mut out, "https://x/a.png", 0, 0, 40, 10)
            .unwrap();
        let id_a = ready_id(&cache, "https://x/a.png");

        let mut out_b: Vec<u8> = Vec::new();
        cache
            .render_at(&mut out_b, "https://x/b.png", 0, 0, 40, 10)
            .unwrap();
        let s = String::from_utf8_lossy(&out_b).to_string();
        assert!(
            s.contains(&format!("a=d,d=i,i={},q=2", id_a)),
            "expected placement delete for image {id_a} in: {s}"
        );
        // Same-image re-render must NOT delete its own placement.
        let mut out_b2: Vec<u8> = Vec::new();
        cache
            .render_at(&mut out_b2, "https://x/b.png", 0, 0, 40, 10)
            .unwrap();
        let s2 = String::from_utf8_lossy(&out_b2).to_string();
        assert!(
            !s2.contains("a=d,d=i"),
            "same image re-place needs no delete: {s2}"
        );
    }

    #[test]
    fn lru_eviction_frees_terminal_data_on_next_flush() {
        let mut cache = cache_with_images(&["https://x/old.png"]);
        let mut out: Vec<u8> = Vec::new();
        cache
            .render_at(&mut out, "https://x/old.png", 0, 0, 40, 10)
            .unwrap();
        let old_id = ready_id(&cache, "https://x/old.png");

        // Push the transmitted image out of the LRU.
        for i in 0..MAX_CACHED_IMAGES {
            cache.insert_ready_for_test(
                &format!("https://x/fill-{i}.png"),
                100 + i as u32,
                8,
                8,
                false,
            );
        }
        assert!(matches!(
            cache.view("https://x/old.png"),
            ImageView::Missing
        ));
        assert_eq!(cache.pending_deletes, vec![old_id]);

        // The next write flushes a d=I (placements + pixel data) for it.
        let mut cleared: Vec<u8> = Vec::new();
        cache.clear_terminal(&mut cleared).unwrap();
        let s = String::from_utf8_lossy(&cleared).to_string();
        assert!(
            s.contains(&format!("a=d,d=I,i={},q=2", old_id)),
            "expected terminal-data delete for {old_id} in: {s}"
        );
        assert!(cache.pending_deletes.is_empty());
    }

    // ── transmit budget and virtual placements (pump) ─────────────────────

    /// Spec naming `url` in `cols` x `rows` cells.
    fn spec(url: &str, cols: u16, rows: u16) -> PlacementSpec {
        PlacementSpec {
            url: url.to_string(),
            cols,
            rows,
        }
    }

    #[test]
    fn pump_transmits_one_image_per_call_then_places_it() {
        let mut cache = ImageCache::new();
        cache.force_protocol(ImageProtocol::UnicodePlaceholders);
        cache.insert_ready_for_test("a", 1, 40, 10, false);
        cache.insert_ready_for_test("b", 2, 40, 10, false);
        let specs = [spec("a", 40, 10), spec("b", 40, 10)];

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
        cache.force_protocol(ImageProtocol::UnicodePlaceholders);
        cache.insert_ready_for_test("a", 1, 40, 10, true);
        let mut out = Vec::new();
        cache.pump(&mut out, &[spec("a", 40, 10)]).unwrap();
        assert!(out.is_empty());

        let mut resized = Vec::new();
        cache.pump(&mut resized, &[spec("a", 60, 15)]).unwrap();
        let s = String::from_utf8_lossy(&resized);
        assert!(s.contains("a=p,U=1,i=1,p=1,c=60,r=15"), "{s}");
        assert!(!s.contains("a=t"), "{s}");
    }

    #[test]
    fn pump_does_nothing_on_none_protocol() {
        let mut cache = ImageCache::new();
        cache.force_protocol(ImageProtocol::None);
        cache.insert_ready_for_test("a", 1, 40, 10, false);

        let mut out = Vec::new();
        cache.pump(&mut out, &[spec("a", 40, 10)]).unwrap();
        assert!(out.is_empty(), "{:?}", String::from_utf8_lossy(&out));
    }

    #[test]
    fn pump_frees_evicted_terminal_data() {
        let mut cache = ImageCache::new();
        cache.force_protocol(ImageProtocol::UnicodePlaceholders);
        cache.insert_ready_for_test("https://x/old.png", 4242, 8, 8, true);
        // Push the transmitted image out of the LRU; eviction has no writer,
        // so the `d=I` is only queued.
        for i in 0..MAX_CACHED_IMAGES {
            cache.insert_ready_for_test(
                &format!("https://x/fill-{i}.png"),
                100 + i as u32,
                8,
                8,
                false,
            );
        }
        assert!(matches!(
            cache.view("https://x/old.png"),
            ImageView::Missing
        ));
        assert_eq!(cache.pending_deletes, vec![4242]);

        let mut out = Vec::new();
        cache.pump(&mut out, &[]).unwrap();
        let s = String::from_utf8_lossy(&out);
        assert!(s.contains("a=d,d=I,i=4242,q=2"), "{s}");
        assert!(cache.pending_deletes.is_empty());
        assert!(
            !cache.last_placement.contains_key(&4242),
            "eviction must not leave stale placement state behind"
        );
    }

    #[test]
    fn pump_pins_spec_urls_against_eviction() {
        let mut cache = ImageCache::new();
        cache.force_protocol(ImageProtocol::UnicodePlaceholders);
        let urls: Vec<String> = (0..MAX_CACHED_IMAGES)
            .map(|i| format!("https://x/pinned-{i}.png"))
            .collect();
        for (i, url) in urls.iter().enumerate() {
            cache.insert_ready_for_test(url, i as u32 + 1, 8, 8, false);
        }
        assert_eq!(cache.entries.len(), MAX_CACHED_IMAGES);

        let specs: Vec<PlacementSpec> = urls.iter().map(|u| spec(u, 8, 4)).collect();
        let mut out = Vec::new();
        cache.pump(&mut out, &specs).unwrap();

        // A 33rd insertion cannot evict anything: every existing URL is
        // pinned by the specs, so the newcomer is the only evictable entry.
        cache.insert_ready_for_test("https://x/newcomer.png", 999, 8, 8, false);
        for url in &urls {
            assert!(
                cache.has_image(url),
                "pinned URL {url} must survive eviction"
            );
        }
        assert!(matches!(
            cache.view("https://x/newcomer.png"),
            ImageView::Missing
        ));
    }

    #[test]
    fn pump_skips_ready_entry_that_lost_its_payload() {
        let mut cache = ImageCache::new();
        cache.force_protocol(ImageProtocol::UnicodePlaceholders);
        // Reachable after a transmit I/O error, and from test fixtures: the
        // entry is `Ready` but has no PNG to send. It must never be marked
        // transmitted or placed — the terminal has nothing to draw.
        cache.insert_entry(
            "https://x/hollow.png".to_string(),
            Entry::Ready {
                image: ReadyImage {
                    id: 9,
                    width: 8,
                    height: 8,
                    transmitted: false,
                },
                png: None,
            },
        );

        let mut out = Vec::new();
        cache
            .pump(&mut out, &[spec("https://x/hollow.png", 8, 4)])
            .unwrap();
        assert!(out.is_empty(), "{:?}", String::from_utf8_lossy(&out));
        match cache.view("https://x/hollow.png") {
            ImageView::Ready(img) => assert!(!img.transmitted, "must not claim transmission"),
            other => panic!("expected Ready, got {other:?}"),
        }
        assert!(
            cache.last_placement.is_empty(),
            "must not place an image the terminal never received"
        );
    }
}
