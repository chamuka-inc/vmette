//! Tile-based **pixel-settle detection** for the desktop perception layer.
//!
//! An agent drives the Agent-workload desktop purely through screenshots
//! ([`crate::desktop::Action::Screenshot`]). After it acts, we want to hand it
//! the screen once it has *settled* (stopped changing) — but a playing video or
//! a blinking caret means the framebuffer never globally quiesces, so a naive
//! "is this frame identical to the last?" check either fires too early or never
//! fires. This module decides settle from the pixels alone and, crucially,
//! reports the regions that are *still* moving so the model can reason about
//! them instead of being blocked by them.
//!
//! Like [`crate::desktop`], this module is **pure** — no VZ, no objc2, no I/O.
//! It is plain pixel math over decoded frames, unit-testable against synthetic
//! sequences. The host-side orchestration (poll the agent, decode each PNG,
//! feed it here, stop when settled) lives a layer up in the daemon's session
//! registry; this is only the algorithm.
//!
//! ## Design (distilled from prior art)
//!
//! * **x11vnc** finds change at **32×32 tile** granularity and throttles only
//!   when few tiles move, so a small animating region never marks the whole
//!   screen busy. We adopt the tile grid and per-tile change tracking.
//! * **TurboVNC** hands over a region once it has been *still for a timeout* —
//!   region-local settling, not whole-frame. We track per-tile stable-run
//!   length and settle each tile independently.
//! * **Playwright** declares stability after **N consecutive unchanged frames**,
//!   and **pixelmatch** tolerates antialiasing/cursor noise rather than
//!   demanding exact equality. We use a per-pixel delta threshold plus a
//!   per-tile changed-pixel tolerance so a blinking caret can't livelock us.
//! * **Visual-regression** tools mask persistently-changing regions. We derive
//!   that mask automatically: tiles that keep changing across a sliding window
//!   are classified as *churning*, excluded from the settle test, and reported
//!   back as "still moving here" rectangles (the agent's video/animation hint).

use std::collections::VecDeque;

/// A decoded framebuffer. `pixels.len() == width * height * channels`.
/// `channels` is 3 (RGB) or 4 (RGBA); only the first three (RGB) are compared,
/// so a constant or absent alpha never reads as motion.
#[derive(Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub channels: u8,
    pub pixels: Vec<u8>,
}

impl Frame {
    /// Wrap a decoded buffer. `channels` must be 3 or 4 and
    /// `pixels.len()` must equal `width * height * channels`.
    pub fn new(width: u32, height: u32, channels: u8, pixels: Vec<u8>) -> Self {
        debug_assert!(channels == 3 || channels == 4);
        debug_assert_eq!(pixels.len(), (width * height * channels as u32) as usize);
        Self {
            width,
            height,
            channels,
            pixels,
        }
    }

    /// A `width`×`height` RGBA frame filled with a solid color (tests/fixtures).
    pub fn solid(width: u32, height: u32, rgba: [u8; 4]) -> Self {
        let mut pixels = Vec::with_capacity((width * height * 4) as usize);
        for _ in 0..width * height {
            pixels.extend_from_slice(&rgba);
        }
        Self {
            width,
            height,
            channels: 4,
            pixels,
        }
    }

    /// Paint a filled rectangle (clipped to bounds). Writes `channels` bytes
    /// per pixel from `rgba` (alpha dropped for 3-channel frames).
    pub fn fill_rect(&mut self, rect: Rect, rgba: [u8; 4]) {
        let ch = self.channels as usize;
        let x2 = (rect.x + rect.w).min(self.width);
        let y2 = (rect.y + rect.h).min(self.height);
        for y in rect.y.min(self.height)..y2 {
            for x in rect.x.min(self.width)..x2 {
                let i = ((y * self.width + x) as usize) * ch;
                self.pixels[i..i + ch].copy_from_slice(&rgba[..ch]);
            }
        }
    }

    /// The RGB triple at `(x, y)` (the first three channels).
    #[inline]
    fn rgb(&self, x: u32, y: u32) -> [u8; 3] {
        let i = ((y * self.width + x) as usize) * self.channels as usize;
        [self.pixels[i], self.pixels[i + 1], self.pixels[i + 2]]
    }
}

/// A rectangle in pixel coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// Tunables for [`SettleDetector`]. Defaults mirror the prior-art numbers
/// (32px tiles, ~2–3 stable frames) and are the validated spike starting point.
#[derive(Clone, Copy, Debug)]
pub struct SettleConfig {
    /// Tile edge length in pixels (x11vnc uses 32).
    pub tile: u32,
    /// Per-channel absolute delta above which a pixel counts as changed.
    /// Tolerates render dither / antialiasing (pixelmatch's idea).
    pub pixel_threshold: u8,
    /// Changed-pixel count above which a *tile* counts as changed. A small
    /// floor so a few-pixel cursor or AA fringe does not trip the tile.
    pub tile_pixel_tolerance: u32,
    /// Consecutive unchanged polls a tile needs to count as settled
    /// (Playwright requires two; we default a touch higher).
    pub settle_frames: u32,
    /// Sliding window (in polls) over which churn is measured.
    pub churn_window: u32,
    /// Changes within the window at which a tile is deemed *churning*
    /// (persistent animation: video/spinner/blink).
    pub churn_threshold: u32,
    /// If the churning fraction of tiles exceeds this, the screen is animating
    /// *globally* (still loading / mid-transition) — never report settled.
    pub max_churn_fraction: f32,
}

impl Default for SettleConfig {
    fn default() -> Self {
        Self {
            tile: 32,
            pixel_threshold: 24,
            tile_pixel_tolerance: 12,
            settle_frames: 3,
            churn_window: 10,
            churn_threshold: 5,
            max_churn_fraction: 0.5,
        }
    }
}

/// The detector's verdict for the most recent frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettleState {
    /// Need at least two frames before any verdict.
    Warmup,
    /// The screen is still changing broadly; do not capture yet.
    Unsettled,
    /// Stable: every non-churning tile held still for `settle_frames`. The
    /// agent gets this frame. `moving` lists rectangles still animating
    /// (video/cursor) — metadata the model can act on without being blocked.
    Settled { moving: Vec<Rect> },
}

#[derive(Clone)]
struct TileState {
    /// Consecutive polls this tile has been unchanged.
    stable: u32,
    /// Rolling change flags over the last `churn_window` polls.
    window: VecDeque<bool>,
    /// Count of `true` in `window` (kept in sync to avoid rescans).
    window_changes: u32,
}

/// Stateful settle detector for a fixed frame size. Feed frames with
/// [`SettleDetector::push`]; read the rolling damage box with
/// [`SettleDetector::last_damage`].
pub struct SettleDetector {
    cfg: SettleConfig,
    width: u32,
    height: u32,
    cols: u32,
    rows: u32,
    tiles: Vec<TileState>,
    prev: Option<Frame>,
    last_damage: Option<Rect>,
}

impl SettleDetector {
    /// Build a detector for a fixed frame size.
    pub fn new(width: u32, height: u32, cfg: SettleConfig) -> Self {
        let cols = width.div_ceil(cfg.tile);
        let rows = height.div_ceil(cfg.tile);
        let tiles = vec![
            TileState {
                stable: 0,
                window: VecDeque::with_capacity(cfg.churn_window as usize),
                window_changes: 0,
            };
            (cols * rows) as usize
        ];
        Self {
            cfg,
            width,
            height,
            cols,
            rows,
            tiles,
            prev: None,
            last_damage: None,
        }
    }

    /// Damage bounding box (union of tiles that changed) for the most recent
    /// [`push`](Self::push). Powers a `what_changed` primitive. `None` on
    /// warmup or when nothing changed.
    pub fn last_damage(&self) -> Option<Rect> {
        self.last_damage
    }

    /// Feed the next captured frame and get the current settle verdict. Takes
    /// the frame by value and retains it as the next comparison baseline, so
    /// there is no per-frame copy (the previous frame is moved into place).
    pub fn push(&mut self, frame: Frame) -> SettleState {
        // The tile grid is sized for the construction dimensions. A frame of any
        // other size cannot be diffed against that grid (it would index past the
        // buffer), so reject it without disturbing the baseline: report
        // Unsettled and let the caller keep polling or time out. The frame comes
        // from a decoded guest PNG, so this must degrade rather than panic.
        if frame.width != self.width || frame.height != self.height {
            self.last_damage = None;
            return SettleState::Unsettled;
        }
        let prev = match self.prev.take() {
            None => {
                self.prev = Some(frame);
                return SettleState::Warmup;
            }
            Some(p) => p,
        };
        let (fw, fh) = (frame.width, frame.height);

        let mut damage: Option<Rect> = None;
        for ty in 0..self.rows {
            for tx in 0..self.cols {
                let r = tile_rect(self.cfg.tile, tx, ty, fw, fh);
                let changed = tile_changed(
                    &prev,
                    &frame,
                    r,
                    self.cfg.pixel_threshold,
                    self.cfg.tile_pixel_tolerance,
                );
                let idx = (ty * self.cols + tx) as usize;
                let t = &mut self.tiles[idx];

                if t.window.len() as u32 == self.cfg.churn_window
                    && t.window.pop_front() == Some(true)
                {
                    t.window_changes -= 1;
                }
                t.window.push_back(changed);
                if changed {
                    t.window_changes += 1;
                    t.stable = 0;
                    damage = Some(union(damage, r));
                } else {
                    t.stable += 1;
                }
            }
        }
        self.last_damage = damage;
        self.prev = Some(frame);

        self.verdict(fw, fh)
    }

    /// A tile is *churning* if it changed often across the window AND has not
    /// yet held a clean stable run — i.e. it is actively animating right now,
    /// not merely a region that finished changing a moment ago.
    fn is_churning(&self, t: &TileState) -> bool {
        t.window_changes >= self.cfg.churn_threshold && t.stable < self.cfg.settle_frames
    }

    fn verdict(&self, fw: u32, fh: u32) -> SettleState {
        let total = (self.cols * self.rows) as f32;
        let churning: Vec<bool> = self.tiles.iter().map(|t| self.is_churning(t)).collect();
        let churn_count = churning.iter().filter(|&&c| c).count() as f32;

        // Whole-screen animation (loading / transition): not a video-in-a-box.
        if churn_count / total > self.cfg.max_churn_fraction {
            return SettleState::Unsettled;
        }

        // Every non-churning tile must have held still long enough.
        let all_quiet = self
            .tiles
            .iter()
            .zip(churning.iter())
            .all(|(t, &c)| c || t.stable >= self.cfg.settle_frames);
        if !all_quiet {
            return SettleState::Unsettled;
        }

        SettleState::Settled {
            moving: self.churn_rects(&churning, fw, fh),
        }
    }

    /// Merge churning tiles into bounding boxes via 4-connected components on
    /// the tile grid — so "video in one corner, spinner elsewhere" yields two
    /// rects, not one giant box spanning the gap.
    fn churn_rects(&self, churning: &[bool], fw: u32, fh: u32) -> Vec<Rect> {
        let mut seen = vec![false; churning.len()];
        let mut rects = Vec::new();
        for start in 0..churning.len() {
            if !churning[start] || seen[start] {
                continue;
            }
            let mut stack = vec![start];
            seen[start] = true;
            let (mut min_tx, mut min_ty, mut max_tx, mut max_ty) =
                (self.cols, self.rows, 0u32, 0u32);
            while let Some(i) = stack.pop() {
                let tx = (i as u32) % self.cols;
                let ty = (i as u32) / self.cols;
                min_tx = min_tx.min(tx);
                min_ty = min_ty.min(ty);
                max_tx = max_tx.max(tx);
                max_ty = max_ty.max(ty);
                for (dx, dy) in [(-1i32, 0i32), (1, 0), (0, -1), (0, 1)] {
                    let nx = tx as i32 + dx;
                    let ny = ty as i32 + dy;
                    if nx < 0 || ny < 0 || nx as u32 >= self.cols || ny as u32 >= self.rows {
                        continue;
                    }
                    let ni = (ny as u32 * self.cols + nx as u32) as usize;
                    if churning[ni] && !seen[ni] {
                        seen[ni] = true;
                        stack.push(ni);
                    }
                }
            }
            let top_left = tile_rect(self.cfg.tile, min_tx, min_ty, fw, fh);
            let bottom_right = tile_rect(self.cfg.tile, max_tx, max_ty, fw, fh);
            rects.push(Rect {
                x: top_left.x,
                y: top_left.y,
                w: bottom_right.x + bottom_right.w - top_left.x,
                h: bottom_right.y + bottom_right.h - top_left.y,
            });
        }
        rects
    }
}

/// Pixel rectangle covered by tile `(tx, ty)`, clipped to the frame.
fn tile_rect(tile: u32, tx: u32, ty: u32, fw: u32, fh: u32) -> Rect {
    let x = tx * tile;
    let y = ty * tile;
    Rect {
        x,
        y,
        w: tile.min(fw - x),
        h: tile.min(fh - y),
    }
}

/// True if a tile differs between two frames beyond the tolerances. Compares
/// RGB only and early-exits once `tolerance` changed pixels are seen.
fn tile_changed(a: &Frame, b: &Frame, r: Rect, pixel_threshold: u8, tolerance: u32) -> bool {
    let mut changed_px = 0u32;
    for y in r.y..r.y + r.h {
        for x in r.x..r.x + r.w {
            let pa = a.rgb(x, y);
            let pb = b.rgb(x, y);
            let delta = pa
                .iter()
                .zip(pb.iter())
                .map(|(&u, &v)| u.abs_diff(v))
                .max()
                .unwrap_or(0);
            if delta > pixel_threshold {
                changed_px += 1;
                if changed_px > tolerance {
                    return true;
                }
            }
        }
    }
    false
}

fn union(acc: Option<Rect>, r: Rect) -> Rect {
    match acc {
        None => r,
        Some(a) => {
            let x = a.x.min(r.x);
            let y = a.y.min(r.y);
            let x2 = (a.x + a.w).max(r.x + r.w);
            let y2 = (a.y + a.h).max(r.y + r.h);
            Rect {
                x,
                y,
                w: x2 - x,
                h: y2 - y,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: u32 = 640;
    const H: u32 = 480;
    const BG: [u8; 4] = [20, 20, 20, 255];

    /// Tiny deterministic PRNG (xorshift32) so synthetic "video noise" frames
    /// are reproducible without pulling in `rand`.
    struct Lcg(u32);
    impl Lcg {
        fn next_u8(&mut self) -> u8 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            self.0 = x;
            (x & 0xff) as u8
        }
    }

    /// Fill a rect with deterministic per-pixel noise (a stand-in for video).
    fn noise_rect(frame: &mut Frame, rect: Rect, rng: &mut Lcg) {
        for y in rect.y..rect.y + rect.h {
            for x in rect.x..rect.x + rect.w {
                let i = ((y * frame.width + x) * 4) as usize;
                frame.pixels[i] = rng.next_u8();
                frame.pixels[i + 1] = rng.next_u8();
                frame.pixels[i + 2] = rng.next_u8();
                frame.pixels[i + 3] = 255;
            }
        }
    }

    fn det() -> SettleDetector {
        SettleDetector::new(W, H, SettleConfig::default())
    }

    /// A page that paints in progressively, then holds still, is reported
    /// Unsettled while painting and Settled (with nothing moving) once stable.
    #[test]
    fn loading_then_static_settles_clean() {
        let mut d = det();
        let cfg = SettleConfig::default();

        assert_eq!(d.push(Frame::solid(W, H, BG)), SettleState::Warmup);

        for k in 1..=4 {
            let mut f = Frame::solid(W, H, BG);
            f.fill_rect(
                Rect {
                    x: 0,
                    y: 0,
                    w: W,
                    h: k * 80,
                },
                [200, 200, 200, 255],
            );
            assert_eq!(d.push(f), SettleState::Unsettled, "still painting at k={k}");
        }

        let final_img = {
            let mut f = Frame::solid(W, H, BG);
            f.fill_rect(
                Rect {
                    x: 0,
                    y: 0,
                    w: W,
                    h: 320,
                },
                [200, 200, 200, 255],
            );
            f
        };
        let mut last = SettleState::Unsettled;
        for _ in 0..cfg.churn_window + cfg.settle_frames {
            last = d.push(final_img.clone());
        }
        assert_eq!(
            last,
            SettleState::Settled { moving: vec![] },
            "static screen must settle with nothing moving"
        );
    }

    /// A small video region churning every frame must NOT block settle: the
    /// rest of the screen settles and the video is reported as a moving rect.
    #[test]
    fn video_region_excluded_and_reported() {
        let mut d = det();
        let video = Rect {
            x: 64,
            y: 64,
            w: 128,
            h: 96,
        };
        let mut rng = Lcg(0x1234_5678);

        let mut state = SettleState::Warmup;
        for _ in 0..30 {
            let mut f = Frame::solid(W, H, BG);
            noise_rect(&mut f, video, &mut rng);
            state = d.push(f);
        }

        match state {
            SettleState::Settled { moving } => {
                assert_eq!(moving.len(), 1, "exactly one moving region");
                let m = moving[0];
                assert!(
                    m.x <= video.x
                        && m.y <= video.y
                        && m.x + m.w >= video.x + video.w
                        && m.y + m.h >= video.y + video.h,
                    "moving rect {m:?} must cover video {video:?}"
                );
                assert!(m.w < W && m.h < H, "moving rect must stay local");
            }
            other => panic!("expected Settled with a moving region, got {other:?}"),
        }
    }

    /// A 3×3 blinking cursor (under the per-tile pixel tolerance) is ignored
    /// entirely: the screen settles with NOTHING reported as moving.
    #[test]
    fn tiny_blinking_cursor_is_tolerated() {
        let mut d = det();
        let cursor = Rect {
            x: 300,
            y: 200,
            w: 3,
            h: 3,
        };
        let mut state = SettleState::Warmup;
        for i in 0..20 {
            let mut f = Frame::solid(W, H, BG);
            if i % 2 == 0 {
                f.fill_rect(cursor, [255, 255, 255, 255]);
            }
            state = d.push(f);
        }
        assert_eq!(
            state,
            SettleState::Settled { moving: vec![] },
            "a few-pixel caret blink must not register as motion"
        );
    }

    /// Whole-screen animation (every tile churning) is never reported settled;
    /// once it stops, the screen settles.
    #[test]
    fn global_animation_blocks_until_it_stops() {
        let mut d = det();
        let full = Rect {
            x: 0,
            y: 0,
            w: W,
            h: H,
        };
        let mut rng = Lcg(0xc0ff_ee01);

        let mut prime = Frame::solid(W, H, BG);
        noise_rect(&mut prime, full, &mut rng);
        assert_eq!(d.push(prime), SettleState::Warmup);

        for _ in 0..20 {
            let mut f = Frame::solid(W, H, BG);
            noise_rect(&mut f, full, &mut rng);
            assert_eq!(
                d.push(f),
                SettleState::Unsettled,
                "global churn must not settle"
            );
        }

        let cfg = SettleConfig::default();
        let mut last = SettleState::Unsettled;
        for _ in 0..cfg.churn_window + cfg.settle_frames {
            last = d.push(Frame::solid(W, H, [77, 88, 99, 255]));
        }
        assert_eq!(
            last,
            SettleState::Settled { moving: vec![] },
            "once motion stops the screen settles"
        );
    }

    /// `what_changed`: the damage bbox tracks the region that moved.
    #[test]
    fn damage_bbox_tracks_change() {
        let mut d = det();
        d.push(Frame::solid(W, H, BG));

        let mut f = Frame::solid(W, H, BG);
        let moved = Rect {
            x: 200,
            y: 100,
            w: 64,
            h: 64,
        };
        f.fill_rect(moved, [10, 240, 10, 255]);
        d.push(f);

        let dmg = d.last_damage().expect("a change should produce damage");
        assert!(
            dmg.x <= moved.x
                && dmg.y <= moved.y
                && dmg.x + dmg.w >= moved.x + moved.w
                && dmg.y + dmg.h >= moved.y + moved.h,
            "damage {dmg:?} must cover the moved rect {moved:?}"
        );

        let mut same = Frame::solid(W, H, BG);
        same.fill_rect(moved, [10, 240, 10, 255]);
        d.push(same);
        assert_eq!(d.last_damage(), None, "no change ⇒ no damage");
    }

    /// A frame whose size doesn't match the detector (e.g. an unexpected guest
    /// capture) must degrade to Unsettled, never panic, and must not corrupt the
    /// baseline so correctly-sized frames still settle afterward.
    #[test]
    fn mismatched_frame_size_does_not_panic() {
        let mut d = det();
        assert_eq!(d.push(Frame::solid(W, H, BG)), SettleState::Warmup);

        // Smaller than the grid — the dangerous direction (would underflow the
        // tile clip and read out of bounds without the guard).
        assert_eq!(
            d.push(Frame::solid(W / 2, H / 2, BG)),
            SettleState::Unsettled
        );
        // Larger than the grid, also rejected.
        assert_eq!(d.push(Frame::solid(W * 2, H, BG)), SettleState::Unsettled);
        assert_eq!(d.last_damage(), None);

        // The baseline survived the bad frames: a static correct-size stream
        // still settles.
        let mut last = SettleState::Unsettled;
        let cfg = SettleConfig::default();
        for _ in 0..cfg.churn_window + cfg.settle_frames {
            last = d.push(Frame::solid(W, H, BG));
        }
        assert_eq!(last, SettleState::Settled { moving: vec![] });
    }

    /// A 3-channel (RGB) frame diffs identically to the 4-channel path: the
    /// decoded-PNG shape the daemon feeds in is handled without alpha.
    #[test]
    fn rgb_frames_diff_without_alpha() {
        let mut d = SettleDetector::new(64, 64, SettleConfig::default());
        let solid_rgb = |c: [u8; 3]| {
            let mut px = Vec::with_capacity(64 * 64 * 3);
            for _ in 0..64 * 64 {
                px.extend_from_slice(&c);
            }
            Frame::new(64, 64, 3, px)
        };
        assert_eq!(d.push(solid_rgb([10, 10, 10])), SettleState::Warmup);
        // A wholesale color change is motion.
        assert_eq!(d.push(solid_rgb([200, 30, 30])), SettleState::Unsettled);
    }
}
