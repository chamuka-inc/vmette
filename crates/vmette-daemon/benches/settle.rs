//! Cost of the settle detector at the production desktop size (1280×800).
//!
//! Two regimes:
//!   * **static**  — worst case for the diff scan: no pixel crosses the
//!     threshold, so every tile is scanned to completion (no early-exit).
//!   * **video**   — realistic: a noisy region early-exits its tiles fast,
//!     the rest is static.
//!
//! `push()` takes the frame by value and retains it as the next baseline; the
//! per-frame ~4 MB memcpy that a caller pays to hand over a fresh frame is
//! reported separately (`clone`) since a production double-buffer removes it.
//!
//! This drives only the detector's public API (`Frame::new` + `SettleDetector`),
//! so it needs no test-only fixtures. Run: `cargo bench -p vmette-daemon`.

use std::time::Instant;

use vmette_daemon::settle::{Frame, SettleConfig, SettleDetector};

const W: u32 = 1280;
const H: u32 = 800;
const BG: [u8; 4] = [18, 18, 22, 255];
const ITERS: u32 = 1000;

/// Tiny deterministic PRNG (xorshift32) so the synthetic "video noise" is
/// reproducible without pulling in `rand`.
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

/// A `W`×`H` RGBA frame filled with a solid color.
fn solid(rgba: [u8; 4]) -> Frame {
    let mut pixels = Vec::with_capacity((W * H * 4) as usize);
    for _ in 0..W * H {
        pixels.extend_from_slice(&rgba);
    }
    Frame::new(W, H, 4, pixels)
}

/// Paint per-pixel noise into a sub-rectangle of an RGBA buffer (video stand-in).
fn noise_rect(pixels: &mut [u8], x0: u32, y0: u32, w: u32, h: u32, rng: &mut Lcg) {
    for y in y0..y0 + h {
        for x in x0..x0 + w {
            let i = ((y * W + x) * 4) as usize;
            pixels[i] = rng.next_u8();
            pixels[i + 1] = rng.next_u8();
            pixels[i + 2] = rng.next_u8();
            pixels[i + 3] = 255;
        }
    }
}

fn bench(name: &str, mut make_frame: impl FnMut() -> Frame) {
    let mut d = SettleDetector::new(W, H, SettleConfig::default());
    // Warm the detector + caches.
    for _ in 0..10 {
        d.push(make_frame());
    }
    let start = Instant::now();
    for _ in 0..ITERS {
        std::hint::black_box(d.push(make_frame()));
    }
    let elapsed = start.elapsed();
    let per_frame_us = elapsed.as_micros() as f64 / ITERS as f64;
    println!(
        "{name:<8}  {per_frame_us:>7.1} µs/frame   ~{:>6.0} frames/s   ({ITERS} frames in {elapsed:?})",
        1_000_000.0 / per_frame_us,
    );
}

fn clone_cost() {
    let f = solid(BG);
    let start = Instant::now();
    for _ in 0..ITERS {
        std::hint::black_box(f.clone());
    }
    let per = start.elapsed().as_micros() as f64 / ITERS as f64;
    println!(
        "clone     {per:>7.1} µs/frame   (the retain-previous memcpy, ~{} KB)",
        W * H * 4 / 1024
    );
}

fn main() {
    println!(
        "frame {W}×{H} = {} px, {} KB RGBA\n",
        W * H,
        W * H * 4 / 1024
    );

    // static: a fresh copy each push (push consumes the frame), so the diff
    // scans every tile to completion.
    let static_frame = solid(BG);
    bench("static", || static_frame.clone());

    // video: a small noisy region that early-exits its tiles, rest static.
    let mut rng = Lcg(0xa5a5_1234);
    bench("video", || {
        let mut f = solid(BG);
        noise_rect(&mut f.pixels, 96, 96, 256, 192, &mut rng);
        f
    });

    clone_cost();
}
