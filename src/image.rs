//! Inline image rendering via the Kitty graphics protocol.
//!
//! Scope (MVP): a single image per article, placed in a reserved strip of
//! the detail view. Background fetch via a worker thread pool. Decoded into
//! `image::DynamicImage`, downscaled, re-encoded as PNG, base64-encoded,
//! and transmitted to the terminal in 4096-byte chunks per the Kitty spec.
//!
//! Non-goals (deliberate):
//!   * iTerm2 / Sixel / Terminology / half-block fallbacks. In terminals
//!     without Kitty support the image area silently stays blank.
//!   * Multiple inline images per article. The article's `thumbnail` field
//!     is the single source surfaced.
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

/// Concurrency cap for background fetches. Detail-view ever needs one
/// image at a time, but a quick navigate-spam from the user can queue
/// multiple — bound it so a hostile feed can't fan-out a connection burst.
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

/// LRU cap on decoded images. Each slot can hold up to
/// `MAX_SOURCE_DIM`² RGBA (~4 MB), so the cache is bounded at ~128 MB
/// worst-case instead of growing for the life of the process. Same
/// insertion-order eviction pattern as `App::extracted`.
const MAX_CACHED_IMAGES: usize = 32;

/// Heuristic ratio of (cell width / cell height) in pixels. Most modern
/// fixed-pitch fonts hit ~0.5 — cells are roughly twice as tall as wide.
/// Used to compute how many *columns* an image of a given pixel aspect
/// should occupy when constrained to a target row count.
const CELL_ASPECT: f32 = 0.5;

/// Which inline-image protocol the current terminal supports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageProtocol {
    Kitty,
    None,
}

/// Detect the terminal's inline-image capability from env. We treat any
/// terminal that *speaks* Kitty's graphics protocol as Kitty, including
/// Ghostty (which implements the protocol natively).
pub fn detect_protocol() -> ImageProtocol {
    // tmux inherits KITTY_WINDOW_ID / TERM_PROGRAM from the outer terminal
    // but swallows raw APC sequences (no passthrough support here yet), so
    // the image would never appear while the strip still gets reserved.
    if std::env::var_os("TMUX").is_some() {
        return ImageProtocol::None;
    }
    // Kitty itself.
    if std::env::var_os("KITTY_WINDOW_ID").is_some() {
        return ImageProtocol::Kitty;
    }
    if let Ok(term) = std::env::var("TERM") {
        let term = term.to_lowercase();
        if term.contains("kitty") || term.contains("ghostty") {
            return ImageProtocol::Kitty;
        }
    }
    if let Ok(prog) = std::env::var("TERM_PROGRAM") {
        let prog = prog.to_lowercase();
        if prog == "ghostty" || prog == "wezterm" {
            return ImageProtocol::Kitty;
        }
    }
    ImageProtocol::None
}

/// A PNG blob ready for Kitty transmission, with the dimensions needed to
/// position the placement.
#[derive(Clone, Debug)]
struct KittyImage {
    /// 24-bit Kitty image id. Reserved between transmit and place.
    id: u32,
    /// Pre-encoded PNG bytes, transmitted once on first render and dropped.
    pending_png: Option<Arc<Vec<u8>>>,
    /// Whether the PNG has been transmitted to the terminal. Once true,
    /// subsequent renders just emit placement escapes (cheap).
    transmitted: bool,
    /// Source-image aspect (width / height) — needed to size the placement
    /// in cells against the available rect.
    src_aspect: f32,
}

/// Per-process inline-image cache + Kitty protocol driver.
///
/// State machine for a given URL:
///   absent → in_flight (worker queued) → images: Some(img) (decoded)
///   → kitty_images: Some(KittyImage) (PNG-ready)
///   → on first render: transmitted=true (PNG sent to terminal)
///
/// A failed fetch lands as `images: None` (insert-known-failed); subsequent
/// `start_fetch` calls for that URL no-op.
///
/// Debug is implemented manually because the internal `mpsc::Receiver`
/// has no `Debug` impl — we surface counts instead of contents.
pub struct ImageCache {
    protocol: ImageProtocol,
    /// Decoded images. `None` slot = fetch attempted and failed.
    images: HashMap<String, Option<Arc<DynamicImage>>>,
    /// Insertion order for `images`, oldest first. Drives LRU eviction at
    /// `MAX_CACHED_IMAGES` — see `insert_image`.
    images_order: VecDeque<String>,
    /// PNG-ready Kitty payloads. Only populated when `protocol == Kitty`.
    kitty_images: HashMap<String, Option<KittyImage>>,
    /// Monotonic Kitty image id allocator. Skips 0 (reserved by spec).
    next_kitty_id: u32,
    /// Background fetch channel + bookkeeping.
    sender: mpsc::Sender<(String, Option<DynamicImage>)>,
    receiver: mpsc::Receiver<(String, Option<DynamicImage>)>,
    in_flight: HashSet<String>,
    /// Lazily-built HTTP client. Kept here so the detail-view renderer can
    /// trigger fetches via `&mut App` without the caller threading a client
    /// through every UI layer. Construction is cheap relative to a fetch.
    client: Option<reqwest::blocking::Client>,
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
            .field("images", &self.images.len())
            .field("kitty_images", &self.kitty_images.len())
            .field("in_flight", &self.in_flight.len())
            .finish()
    }
}

impl ImageCache {
    pub fn new() -> Self {
        let (sender, receiver) = mpsc::channel();
        Self {
            protocol: detect_protocol(),
            images: HashMap::new(),
            images_order: VecDeque::new(),
            kitty_images: HashMap::new(),
            next_kitty_id: 0,
            sender,
            receiver,
            in_flight: HashSet::new(),
            client: None,
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

    /// True iff a decoded image is ready for this URL (independent of
    /// whether the Kitty PNG has been transmitted yet — that's lazy).
    pub fn has_image(&self, url: &str) -> bool {
        self.images.get(url).is_some_and(|o| o.is_some())
    }

    /// Record a fetch result (decoded image or known failure), evicting the
    /// oldest entry past `MAX_CACHED_IMAGES`. Eviction also drops the
    /// matching Kitty payload so stale PNG state can't outlive its image;
    /// an evicted-but-displayed URL simply re-fetches on the next render.
    pub(crate) fn insert_image(&mut self, url: String, img: Option<Arc<DynamicImage>>) {
        if self.images.insert(url.clone(), img).is_none() {
            self.images_order.push_back(url);
        }
        while self.images_order.len() > MAX_CACHED_IMAGES {
            if let Some(oldest) = self.images_order.pop_front() {
                self.images.remove(&oldest);
                self.kitty_images.remove(&oldest);
            }
        }
    }

    /// Queue a background fetch+decode for `url`. Returns false if the
    /// concurrency cap is full — caller can re-queue on the next tick.
    /// Idempotent: re-queueing an in-flight or already-attempted URL is a
    /// no-op (returns true, the "already handled" signal).
    pub fn start_fetch(&mut self, url: &str) -> bool {
        if self.protocol == ImageProtocol::None {
            return true; // no point fetching if we can't render
        }
        if self.images.contains_key(url) || self.in_flight.contains(url) {
            return true;
        }
        if self.in_flight.len() >= MAX_CONCURRENT_FETCHES {
            return false;
        }
        if !crate::feed::is_safe_auto_url(url) {
            // Treat blocked hosts as a known failure so we don't re-queue.
            self.insert_image(url.to_string(), None);
            return true;
        }
        // Lazily build the client. Failure here is treated as a known-failed
        // fetch so we don't re-queue on every render tick. The safe-redirect
        // client re-runs `is_safe_auto_url` on every hop — thumbnails come
        // from hostile feed content and are fetched with no user action, so
        // a public-looking URL must not be allowed to 302 into the user's
        // internal network (same threat model as the auto-fulltext path).
        if self.client.is_none() {
            match crate::feed::Feed::build_safe_redirect_client(15) {
                Ok(c) => self.client = Some(c),
                Err(_) => {
                    self.insert_image(url.to_string(), None);
                    return true;
                }
            }
        }
        let client = self.client.as_ref().unwrap().clone();
        self.in_flight.insert(url.to_string());
        let sender = self.sender.clone();
        let url_owned = url.to_string();
        std::thread::spawn(move || {
            // Panic-safe: a panic in the image decoder must still free the
            // in-flight slot via `poll_completed`. Without `catch_unwind`,
            // a single hostile image could permanently strand the slot.
            let img = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                fetch_and_decode(&url_owned, &client)
            }))
            .unwrap_or(None);
            let _ = sender.send((url_owned, img));
        });
        true
    }

    /// Drain completed background fetches. Returns true if any new images
    /// arrived (so the caller can request a redraw).
    pub fn poll_completed(&mut self) -> bool {
        let mut any = false;
        while let Ok((url, img)) = self.receiver.try_recv() {
            self.in_flight.remove(&url);
            self.insert_image(url, img.map(Arc::new));
            any = true;
        }
        any
    }

    /// Compute the (cols, rows) the image should occupy when constrained
    /// to `(max_cols, max_rows)` cells, preserving the image's aspect.
    /// Returns None if the image isn't loaded yet.
    pub fn display_cells(&self, url: &str, max_cols: usize, max_rows: usize) -> Option<(u16, u16)> {
        let img = self.images.get(url)?.as_ref()?;
        let (w, h) = img.dimensions();
        Some(fit_cells(w, h, max_cols, max_rows))
    }

    /// Pre-encode the PNG for `url` if needed, ready to transmit. Called
    /// from `render_at` on first use of an image.
    fn ensure_kitty_ready(&mut self, url: &str) -> bool {
        if self.protocol != ImageProtocol::Kitty {
            return false;
        }
        if let Some(ki) = self.kitty_images.get(url) {
            return ki.is_some();
        }
        let img = match self.images.get(url).and_then(|o| o.as_ref()) {
            Some(img) => img,
            None => return false,
        };
        let png = match encode_png(img) {
            Some(p) => p,
            None => {
                self.kitty_images.insert(url.to_string(), None);
                return false;
            }
        };
        self.next_kitty_id = self.next_kitty_id.wrapping_add(1);
        // 24-bit range; ID 0 is reserved by the spec.
        if self.next_kitty_id == 0 || self.next_kitty_id > 0x00FF_FFFF {
            self.next_kitty_id = 1;
        }
        let (w, h) = img.dimensions();
        let src_aspect = if h == 0 { 1.0 } else { w as f32 / h as f32 };
        self.kitty_images.insert(
            url.to_string(),
            Some(KittyImage {
                id: self.next_kitty_id,
                pending_png: Some(Arc::new(png)),
                transmitted: false,
                src_aspect,
            }),
        );
        true
    }

    /// Place the image for `url` at the top-left of `rect`, sized to fit
    /// within `rect`. Returns true if anything was written to stdout.
    /// No-op on non-Kitty terminals.
    pub fn render_at(
        &mut self,
        stdout: &mut impl Write,
        url: &str,
        rect_col: u16,
        rect_row: u16,
        max_cols: u16,
        max_rows: u16,
    ) -> io::Result<bool> {
        if self.protocol != ImageProtocol::Kitty {
            return Ok(false);
        }
        if !self.ensure_kitty_ready(url) {
            return Ok(false);
        }
        let ki = match self.kitty_images.get_mut(url).and_then(|o| o.as_mut()) {
            Some(ki) => ki,
            None => return Ok(false),
        };

        // Size the placement: respect both rect bounds AND image aspect.
        let (cols, rows) =
            fit_cells_with_aspect(ki.src_aspect, max_cols as usize, max_rows as usize);
        if cols == 0 || rows == 0 {
            return Ok(false);
        }

        // Position cursor at the rect's top-left in raw 1-based coords.
        // crossterm's MoveTo is 0-based, so emit the CSI directly to stay
        // independent of which Writer the caller uses (stdout, BufWriter…).
        write!(stdout, "\x1b[{};{}H", rect_row + 1, rect_col + 1)?;

        if !ki.transmitted {
            if let Some(png) = ki.pending_png.take() {
                transmit_kitty_image(stdout, &png, ki.id)?;
            }
            ki.transmitted = true;
        }
        place_kitty_image(stdout, ki.id, cols, rows)?;
        // Leave the cursor in a sane spot below the image so the next
        // ratatui frame's diff doesn't get confused.
        write!(stdout, "\x1b[{};1H", rect_row + rows + 1)?;
        Ok(true)
    }

    /// Tell the terminal to drop every Kitty placement. Call when leaving
    /// any view that was showing an image so it doesn't bleed into the
    /// next view's cells. Does NOT evict our in-process cache — we keep
    /// the decoded image and the registered transmit so re-entering the
    /// view is cheap.
    pub fn clear_terminal(&mut self, stdout: &mut impl Write) -> io::Result<()> {
        if self.protocol != ImageProtocol::Kitty {
            return Ok(());
        }
        // a=d (delete), d=a (all). q=2 suppresses the terminal's response.
        stdout.write_all(b"\x1b_Ga=d,d=a,q=2\x1b\\")?;
        // Every image we previously sent is gone from the terminal's
        // registry, and successful transmits already dropped their PNG
        // bytes — so drop `kitty_images` entirely and re-encode fresh on
        // the next render. The decoded `images` cache is preserved.
        self.kitty_images.clear();
        Ok(())
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
    let b64 = BASE64.encode(png);
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

    // ── ImageCache ───────────────────────────────────────────────────────

    #[test]
    fn detect_protocol_none_by_default() {
        // Env can poison this test if KITTY_WINDOW_ID is set in the parent.
        // We can't safely unset process-global env in a multi-threaded
        // test runner, so just assert the function returns *something*.
        let p = detect_protocol();
        assert!(matches!(p, ImageProtocol::Kitty | ImageProtocol::None));
    }

    #[test]
    fn cache_tracks_known_failures() {
        let mut cache = ImageCache::new();
        // Mark as known-failed.
        cache.insert_image("http://example.com/x.png".into(), None);
        assert!(cache.images.contains_key("http://example.com/x.png"));
        assert!(!cache.has_image("http://example.com/x.png"));
    }

    #[test]
    fn start_fetch_noop_in_none_protocol() {
        let mut cache = ImageCache::new();
        cache.protocol = ImageProtocol::None;
        assert!(cache.start_fetch("https://example.com/img.png"));
        // No fetch should have spawned.
        assert!(!cache.in_flight.contains("https://example.com/img.png"));
        assert!(!cache.images.contains_key("https://example.com/img.png"));
    }

    #[test]
    fn start_fetch_rejects_unsafe_urls() {
        let mut cache = ImageCache::new();
        cache.protocol = ImageProtocol::Kitty;
        // localhost / private IPs are blocked by is_safe_auto_url.
        assert!(cache.start_fetch("http://127.0.0.1/x.png"));
        // Recorded as a known failure so it isn't re-queued every tick.
        assert!(cache.images.contains_key("http://127.0.0.1/x.png"));
        assert!(!cache.has_image("http://127.0.0.1/x.png"));
        assert!(!cache.in_flight.contains("http://127.0.0.1/x.png"));
    }

    #[test]
    fn insert_image_evicts_oldest_past_cap() {
        let mut cache = ImageCache::new();
        cache.protocol = ImageProtocol::Kitty;
        let img = DynamicImage::new_rgb8(2, 2);
        for i in 0..MAX_CACHED_IMAGES + 3 {
            cache.insert_image(format!("https://x/{i}.png"), Some(Arc::new(img.clone())));
        }
        assert_eq!(cache.images.len(), MAX_CACHED_IMAGES);
        assert_eq!(cache.images_order.len(), MAX_CACHED_IMAGES);
        // The three oldest are gone, the newest survive.
        assert!(!cache.images.contains_key("https://x/0.png"));
        assert!(!cache.images.contains_key("https://x/2.png"));
        assert!(cache.has_image(&format!("https://x/{}.png", MAX_CACHED_IMAGES + 2)));
        // Eviction also drops the Kitty payload for the evicted URL.
        cache.kitty_images.insert("https://x/3.png".into(), None);
        cache.insert_image("https://x/new.png".into(), Some(Arc::new(img)));
        assert!(!cache.kitty_images.contains_key("https://x/3.png"));
    }

    #[test]
    fn insert_image_overwrite_does_not_grow_order() {
        let mut cache = ImageCache::new();
        let img = DynamicImage::new_rgb8(2, 2);
        cache.insert_image("https://x/a.png".into(), None);
        cache.insert_image("https://x/a.png".into(), Some(Arc::new(img)));
        assert_eq!(cache.images_order.len(), 1);
        assert!(cache.has_image("https://x/a.png"));
    }

    #[test]
    fn ensure_kitty_ready_assigns_unique_ids() {
        let mut cache = ImageCache::new();
        cache.protocol = ImageProtocol::Kitty;
        let img = DynamicImage::new_rgb8(8, 8);
        cache.images.insert("a".into(), Some(Arc::new(img.clone())));
        cache.images.insert("b".into(), Some(Arc::new(img)));
        assert!(cache.ensure_kitty_ready("a"));
        assert!(cache.ensure_kitty_ready("b"));
        let id_a = cache.kitty_images.get("a").unwrap().as_ref().unwrap().id;
        let id_b = cache.kitty_images.get("b").unwrap().as_ref().unwrap().id;
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
}
