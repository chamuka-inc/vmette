//! Pure **RFB (VNC) protocol codec** for the daemon's live desktop view.
//!
//! A live viewer reuses the machinery the agent already has: the desktop is
//! captured with `vmette::Action::Screenshot` and driven with the same
//! computer-use [`Action`] vocabulary. This module is the translation layer
//! between an RFB client (any VNC viewer) and that vocabulary:
//!
//! * **server → client** — framebuffer pixels: serialize a [`PixelFormat`],
//!   build the handshake/`ServerInit` bytes, diff two captures into changed
//!   rectangles, and Raw-encode a `FramebufferUpdate` in the client's
//!   negotiated pixel format.
//! * **client → server** — input: map an RFB `PointerEvent`/`KeyEvent` onto
//!   the [`Action`]s the guest agent understands (move/click/drag/scroll, and
//!   key chords / typed text), tracking the small amount of state RFB's
//!   stateful button-mask + modifier model requires.
//!
//! It is **pure** — no sockets, no VZ, no objc2 — plain byte/pixel math over
//! buffers, unit-tested against synthetic sequences. Its only consumer is the
//! daemon's [`view`](crate) bridge, which owns the TCP socket and the
//! `SessionClient`. The pixel-rectangle type it reports ([`Rect`]) is the
//! shared wire type from `vmette-proto`, exactly as `settle` does.
//!
//! Only what a live view needs is implemented: RFB 3.8, security type `None`,
//! and the `Raw` encoding (mandatory in every viewer). That keeps the codec
//! small; a loopback view at a handful of frames per second has no need for the
//! compressing encodings, and a true-color pixel format covers every modern
//! client.

use vmette_proto::agent::{Action, ScrollDirection};
use vmette_proto::Rect;

/// The RFB protocol version this server speaks (3.8).
pub const PROTOCOL_VERSION: &[u8; 12] = b"RFB 003.008\n";
/// Security type `None` — no authentication.
pub const SECURITY_NONE: u8 = 1;
/// Security type `VNC Authentication` (DES challenge/response). macOS Screen
/// Sharing refuses to connect over plain `None`, so the view offers this and
/// runs the challenge dance. The view binds to loopback only and is ephemeral,
/// so the response is not verified — the loopback boundary is the real gate and
/// any password the viewer types is accepted (see `view.rs`).
pub const SECURITY_VNC_AUTH: u8 = 2;
/// The fixed length of a VNC Authentication challenge/response, in bytes.
pub const VNC_AUTH_CHALLENGE_LEN: usize = 16;

/// RFB client→server message type bytes (the ones we handle).
pub mod client_msg {
    pub const SET_PIXEL_FORMAT: u8 = 0;
    pub const SET_ENCODINGS: u8 = 2;
    pub const FRAMEBUFFER_UPDATE_REQUEST: u8 = 3;
    pub const KEY_EVENT: u8 = 4;
    pub const POINTER_EVENT: u8 = 5;
    pub const CLIENT_CUT_TEXT: u8 = 6;
}

/// The RFB `PIXEL_FORMAT` structure (16 bytes on the wire). A live view only
/// ever serves true-color; the fields a client may renegotiate via
/// `SetPixelFormat` (depth, endianness, channel maxes/shifts) are honored when
/// encoding pixels so any modern viewer renders correctly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PixelFormat {
    pub bits_per_pixel: u8,
    pub depth: u8,
    pub big_endian: bool,
    pub true_color: bool,
    pub red_max: u16,
    pub green_max: u16,
    pub blue_max: u16,
    pub red_shift: u8,
    pub green_shift: u8,
    pub blue_shift: u8,
}

impl PixelFormat {
    /// The format advertised in `ServerInit`: 32 bpp, depth 24, little-endian,
    /// true-color, RGB at shifts 16/8/0 — i.e. each pixel is the little-endian
    /// bytes `[B, G, R, 0]`. Most viewers accept it as-is; the rest send a
    /// `SetPixelFormat` we then honor.
    pub fn server_default() -> Self {
        Self {
            bits_per_pixel: 32,
            depth: 24,
            big_endian: false,
            true_color: true,
            red_max: 255,
            green_max: 255,
            blue_max: 255,
            red_shift: 16,
            green_shift: 8,
            blue_shift: 0,
        }
    }

    /// Bytes per pixel in this format.
    pub fn bytes_per_pixel(&self) -> usize {
        (self.bits_per_pixel / 8) as usize
    }

    /// Whether this is a format [`encode_pixel`](Self::encode_pixel) can serve
    /// without panicking or producing a malformed frame: true-color at 8/16/32
    /// bpp, with each channel max non-zero and each shift within the pixel
    /// width. A client `SetPixelFormat` that fails this must be **rejected** (the
    /// previous valid format kept) rather than fed to the encoder — otherwise a
    /// `bits_per_pixel > 32` slices the 4-byte pixel out of range and a shift
    /// `>= bits_per_pixel` overflows the `u32` shift. The view enforces this in
    /// its reader before storing the format.
    pub fn supported(&self) -> bool {
        matches!(self.bits_per_pixel, 8 | 16 | 32)
            && self.true_color
            && self.red_max > 0
            && self.green_max > 0
            && self.blue_max > 0
            && (self.red_shift as u32) < self.bits_per_pixel as u32
            && (self.green_shift as u32) < self.bits_per_pixel as u32
            && (self.blue_shift as u32) < self.bits_per_pixel as u32
    }

    /// Serialize the 16-byte `PIXEL_FORMAT` (3 trailing padding bytes included).
    pub fn encode(&self) -> [u8; 16] {
        [
            self.bits_per_pixel,
            self.depth,
            self.big_endian as u8,
            self.true_color as u8,
            (self.red_max >> 8) as u8,
            self.red_max as u8,
            (self.green_max >> 8) as u8,
            self.green_max as u8,
            (self.blue_max >> 8) as u8,
            self.blue_max as u8,
            self.red_shift,
            self.green_shift,
            self.blue_shift,
            0,
            0,
            0,
        ]
    }

    /// Parse a 16-byte `PIXEL_FORMAT` (the body of a `SetPixelFormat` message).
    pub fn parse(b: &[u8; 16]) -> Self {
        Self {
            bits_per_pixel: b[0],
            depth: b[1],
            big_endian: b[2] != 0,
            true_color: b[3] != 0,
            red_max: u16::from_be_bytes([b[4], b[5]]),
            green_max: u16::from_be_bytes([b[6], b[7]]),
            blue_max: u16::from_be_bytes([b[8], b[9]]),
            red_shift: b[10],
            green_shift: b[11],
            blue_shift: b[12],
        }
    }

    /// Pack one RGB triple into this format's pixel bytes (one
    /// `bytes_per_pixel()`-byte run), scaling each channel to its max and
    /// placing it at its shift, honoring byte order.
    ///
    /// Requires [`supported`](Self::supported) (8/16/32 bpp, shifts in range) —
    /// the per-pixel hot path performs no validation, so the caller must reject
    /// unsupported client formats first. The server's own advertised format is
    /// supported by construction.
    pub fn encode_pixel(&self, rgb: [u8; 3], out: &mut Vec<u8>) {
        let scale = |c: u8, max: u16| -> u32 {
            // max == 255 (the common 8-bit-per-channel case, including the
            // server default) is the identity — skip the per-pixel multiply and
            // divide. Otherwise round to nearest.
            if max == 255 {
                c as u32
            } else {
                (c as u32 * max as u32 + 127) / 255
            }
        };
        let value = (scale(rgb[0], self.red_max) << self.red_shift)
            | (scale(rgb[1], self.green_max) << self.green_shift)
            | (scale(rgb[2], self.blue_max) << self.blue_shift);
        let bpp = self.bytes_per_pixel();
        let bytes = value.to_le_bytes(); // little-endian source; reorder below.
        if self.big_endian {
            // Most-significant of the `bpp` bytes first.
            for i in (0..bpp).rev() {
                out.push(bytes[i]);
            }
        } else {
            out.extend_from_slice(&bytes[..bpp]);
        }
    }
}

/// How the security *type* is negotiated, which differs by the RFB minor
/// version the **client** selects after we offer 3.8. Some clients (notably
/// macOS Screen Sharing) pin the connection down to 3.3, which negotiates the
/// type differently from 3.7+.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeNegotiation {
    /// RFB 3.3: the **server** dictates a single `U32` security type; the
    /// client does not echo a choice back.
    Dictate,
    /// RFB 3.7+: the server offers a `[count, types…]` list and the client
    /// echoes back the one byte it picked.
    List,
}

/// The RFB minor version a client selected, parsed from its 12-byte
/// `ProtocolVersion` reply (`"RFB 003.00X\n"`). Defaults to 3 (the most
/// conservative handshake) if the field is malformed.
pub fn client_minor_version(client_version: &[u8; 12]) -> u32 {
    std::str::from_utf8(&client_version[8..11])
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(3)
}

/// How to negotiate the security type, given the client's selected
/// `ProtocolVersion`. (The chosen type is always VNC Authentication, whose
/// challenge/response + `SecurityResult` follow identically for every version.)
pub fn type_negotiation(client_version: &[u8; 12]) -> TypeNegotiation {
    if client_minor_version(client_version) < 7 {
        TypeNegotiation::Dictate
    } else {
        TypeNegotiation::List
    }
}

/// Build the `ServerInit` message: framebuffer size, the server pixel format,
/// and the desktop name.
pub fn server_init(width: u16, height: u16, pf: &PixelFormat, name: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(24 + name.len());
    v.extend_from_slice(&width.to_be_bytes());
    v.extend_from_slice(&height.to_be_bytes());
    v.extend_from_slice(&pf.encode());
    v.extend_from_slice(&(name.len() as u32).to_be_bytes());
    v.extend_from_slice(name.as_bytes());
    v
}

/// Read the RGB triple at `(x, y)` from a tightly-packed `channels`-per-pixel
/// buffer (the first three channels; alpha ignored).
#[inline]
fn rgb_at(px: &[u8], width: u32, channels: usize, x: u32, y: u32) -> [u8; 3] {
    let i = ((y * width + x) as usize) * channels;
    [px[i], px[i + 1], px[i + 2]]
}

/// Diff two equally-sized frames at `tile`-pixel granularity and return the
/// changed regions, coalescing horizontally-adjacent changed tiles within each
/// tile-row into one rectangle (so a changed row is a few wide rects, not many
/// small ones). RGB is compared exactly — the agent's PNG capture is lossless,
/// so an unchanged screen yields byte-identical pixels and an empty result.
pub fn changed_rects(
    prev: &[u8],
    cur: &[u8],
    width: u32,
    height: u32,
    channels: u8,
    tile: u32,
) -> Vec<Rect> {
    let ch = channels as usize;
    let mut rects = Vec::new();
    let mut ty = 0;
    while ty < height {
        let th = tile.min(height - ty);
        let mut tx = 0;
        // `run_start` marks the left edge of the current run of changed tiles.
        let mut run_start: Option<u32> = None;
        while tx < width {
            let tw = tile.min(width - tx);
            let changed = tile_changed(prev, cur, width, ch, tx, ty, tw, th);
            match (changed, run_start) {
                (true, None) => run_start = Some(tx),
                (false, Some(start)) => {
                    rects.push(Rect {
                        x: start,
                        y: ty,
                        w: tx - start,
                        h: th,
                    });
                    run_start = None;
                }
                _ => {}
            }
            tx += tile;
        }
        if let Some(start) = run_start {
            rects.push(Rect {
                x: start,
                y: ty,
                w: width - start,
                h: th,
            });
        }
        ty += tile;
    }
    rects
}

/// True if any RGB pixel in the `tw`×`th` tile at `(tx, ty)` differs between the
/// two frames. Early-exits on the first difference.
#[allow(clippy::too_many_arguments)]
fn tile_changed(
    a: &[u8],
    b: &[u8],
    width: u32,
    ch: usize,
    tx: u32,
    ty: u32,
    tw: u32,
    th: u32,
) -> bool {
    for y in ty..ty + th {
        for x in tx..tx + tw {
            if rgb_at(a, width, ch, x, y) != rgb_at(b, width, ch, x, y) {
                return true;
            }
        }
    }
    false
}

/// Build a `FramebufferUpdate` message carrying `rects`, each Raw-encoded from
/// the `width`-wide, `channels`-per-pixel source buffer in `pf`'s pixel format.
/// Rectangles must lie within the buffer (the caller derives them from
/// [`changed_rects`] or the full frame).
pub fn framebuffer_update(
    rects: &[Rect],
    pf: &PixelFormat,
    px: &[u8],
    width: u32,
    channels: u8,
) -> Vec<u8> {
    let ch = channels as usize;
    let bpp = pf.bytes_per_pixel();
    let pixels: usize = rects.iter().map(|r| (r.w * r.h) as usize).sum();
    let mut out = Vec::with_capacity(4 + rects.len() * 12 + pixels * bpp);
    out.push(0); // message-type: FramebufferUpdate
    out.push(0); // padding
    out.extend_from_slice(&(rects.len() as u16).to_be_bytes());
    for r in rects {
        out.extend_from_slice(&(r.x as u16).to_be_bytes());
        out.extend_from_slice(&(r.y as u16).to_be_bytes());
        out.extend_from_slice(&(r.w as u16).to_be_bytes());
        out.extend_from_slice(&(r.h as u16).to_be_bytes());
        out.extend_from_slice(&0i32.to_be_bytes()); // encoding-type: Raw
        for y in r.y..r.y + r.h {
            for x in r.x..r.x + r.w {
                pf.encode_pixel(rgb_at(px, width, ch, x, y), &mut out);
            }
        }
    }
    out
}

// ---- client → server: pointer ------------------------------------------

/// RFB pointer button-mask bits.
mod button {
    pub const LEFT: u8 = 1 << 0;
    pub const MIDDLE: u8 = 1 << 1;
    pub const RIGHT: u8 = 1 << 2;
    pub const WHEEL_UP: u8 = 1 << 3;
    pub const WHEEL_DOWN: u8 = 1 << 4;
    pub const WHEEL_LEFT: u8 = 1 << 5;
    pub const WHEEL_RIGHT: u8 = 1 << 6;
}

/// Drag detected only once the pointer leaves a small dead zone while the left
/// button is held — below this a press-move-release is just a click with hand
/// jitter, and should not become a drag.
const DRAG_THRESHOLD_PX: i32 = 3;

/// The little state RFB's stateful pointer model needs: the previous
/// button-mask (to find press/release edges), the last reported position (to
/// suppress redundant moves), and — while the left button is held — where it
/// went down (the drag anchor).
#[derive(Default)]
pub struct PointerState {
    last_mask: u8,
    last_pos: Option<(i32, i32)>,
    left_down_at: Option<(i32, i32)>,
}

/// Translate one RFB `PointerEvent` into computer-use [`Action`]s.
///
/// * A bare move (no button held) tracks the cursor with `MouseMove`.
/// * Left button: move to the press point on the down edge, then on release
///   emit a `LeftClickDrag` if the pointer travelled past [`DRAG_THRESHOLD_PX`],
///   else a `LeftClick`. Intermediate moves are suppressed while held so the
///   drag's "from" point stays the anchor the agent expects.
/// * Middle/right release emits the corresponding click at the current point.
/// * A wheel-bit press emits one `Scroll` click in that direction.
pub fn pointer_actions(st: &mut PointerState, mask: u8, x: i32, y: i32) -> Vec<Action> {
    let prev = st.last_mask;
    st.last_mask = mask;
    let moved = st.last_pos != Some((x, y));
    st.last_pos = Some((x, y));
    let pressed = |b: u8| mask & b != 0 && prev & b == 0;
    let released = |b: u8| mask & b == 0 && prev & b != 0;
    let left_held = mask & button::LEFT != 0;
    let mut out = Vec::new();

    // Wheel: one Scroll per press edge (the matching release carries no click).
    // Scroll already carries its (x, y), so it stands in for a hover move.
    let mut scrolled = false;
    for (bit, dir) in [
        (button::WHEEL_UP, ScrollDirection::Up),
        (button::WHEEL_DOWN, ScrollDirection::Down),
        (button::WHEEL_LEFT, ScrollDirection::Left),
        (button::WHEEL_RIGHT, ScrollDirection::Right),
    ] {
        if pressed(bit) {
            out.push(Action::Scroll {
                x,
                y,
                direction: dir,
                amount: 1,
            });
            scrolled = true;
        }
    }

    if pressed(button::LEFT) {
        // Anchor the (possible) drag at the press point; put the agent pointer
        // there only if it isn't already (clicks fire at the current position).
        st.left_down_at = Some((x, y));
        if moved {
            out.push(Action::MouseMove { x, y });
        }
    } else if released(button::LEFT) {
        let (ax, ay) = st.left_down_at.take().unwrap_or((x, y));
        if (x - ax).abs() > DRAG_THRESHOLD_PX || (y - ay).abs() > DRAG_THRESHOLD_PX {
            // Press-move-release from the anchor to here.
            out.push(Action::LeftClickDrag { x, y });
        } else {
            out.push(Action::LeftClick);
        }
    } else if moved && !left_held && !scrolled {
        // Hover (or pre-click positioning): track the cursor live. Suppressed
        // while the left button is held so a drag's anchor stays put.
        out.push(Action::MouseMove { x, y });
    }

    if released(button::MIDDLE) {
        out.push(Action::MiddleClick);
    }
    if released(button::RIGHT) {
        out.push(Action::RightClick);
    }
    out
}

// ---- client → server: keyboard -----------------------------------------

// X11 modifier keysyms (left/right pairs) we track to assemble chords.
const XK_SHIFT_L: u32 = 0xffe1;
const XK_SHIFT_R: u32 = 0xffe2;
const XK_CONTROL_L: u32 = 0xffe3;
const XK_CONTROL_R: u32 = 0xffe4;
const XK_META_L: u32 = 0xffe7;
const XK_META_R: u32 = 0xffe8;
const XK_ALT_L: u32 = 0xffe9;
const XK_ALT_R: u32 = 0xffea;
const XK_SUPER_L: u32 = 0xffeb;
const XK_SUPER_R: u32 = 0xffec;

/// Held-modifier state for assembling key chords across RFB `KeyEvent`s.
#[derive(Default)]
pub struct KeyState {
    ctrl: bool,
    alt: bool,
    shift: bool,
    sup: bool,
}

/// Translate one RFB `KeyEvent` into a computer-use [`Action`], or `None` when
/// it only updated modifier state (or was a key-up, which carries no action —
/// chords and typed text fire on the down edge).
///
/// With a command modifier held (ctrl/alt/super) the keysym becomes a chord
/// like `"ctrl+c"` / `"alt+Tab"`; a bare printable key types its character
/// (the client already folded Shift into the keysym, so `shift+a` arrives as
/// `A` and `shift+1` as `!`); a bare non-printable key (Return, arrows, …)
/// becomes a single-name `Key`, with `shift+` prefixed when Shift is held.
pub fn key_action(st: &mut KeyState, down: bool, keysym: u32) -> Option<Action> {
    // Modifier keys only toggle state.
    match keysym {
        XK_SHIFT_L | XK_SHIFT_R => {
            st.shift = down;
            return None;
        }
        XK_CONTROL_L | XK_CONTROL_R => {
            st.ctrl = down;
            return None;
        }
        XK_ALT_L | XK_ALT_R => {
            st.alt = down;
            return None;
        }
        XK_META_L | XK_META_R | XK_SUPER_L | XK_SUPER_R => {
            st.sup = down;
            return None;
        }
        _ => {}
    }
    if !down {
        return None;
    }

    let special = special_keyname(keysym);
    let printable = special.is_none().then(|| printable_char(keysym)).flatten();

    let mut mods: Vec<&str> = Vec::new();
    if st.ctrl {
        mods.push("ctrl");
    }
    if st.alt {
        mods.push("alt");
    }
    if st.sup {
        mods.push("super");
    }
    // Shift is only passed for special keys (Tab, arrows, …); for a printable
    // key the client already encoded it into the character, so adding it would
    // double-apply.
    if st.shift && special.is_some() {
        mods.push("shift");
    }

    let token: String = match (special, printable) {
        (Some(name), _) => name.to_string(),
        (None, Some(c)) => c.to_string(),
        (None, None) => return None, // unmappable keysym — drop it.
    };

    if mods.is_empty() {
        match special {
            Some(name) => Some(Action::Key {
                keys: name.to_string(),
            }),
            None => Some(Action::Type { text: token }),
        }
    } else {
        mods.push(&token);
        Some(Action::Key {
            keys: mods.join("+"),
        })
    }
}

/// The X keysym-database name for a non-printable key, as the guest agent's
/// chord parser expects (it feeds these straight to `XStringToKeysym`). Covers
/// the keys a live viewer actually sends; an unlisted keysym falls through to
/// the printable path or is dropped.
fn special_keyname(keysym: u32) -> Option<&'static str> {
    Some(match keysym {
        0xff08 => "BackSpace",
        0xff09 => "Tab",
        0xff0d => "Return",
        0xff1b => "Escape",
        0xff63 => "Insert",
        0xffff => "Delete",
        0xff50 => "Home",
        0xff51 => "Left",
        0xff52 => "Up",
        0xff53 => "Right",
        0xff54 => "Down",
        0xff55 => "Prior", // Page Up
        0xff56 => "Next",  // Page Down
        0xff57 => "End",
        0xff8d => "Return", // KP_Enter
        0xffbe => "F1",
        0xffbf => "F2",
        0xffc0 => "F3",
        0xffc1 => "F4",
        0xffc2 => "F5",
        0xffc3 => "F6",
        0xffc4 => "F7",
        0xffc5 => "F8",
        0xffc6 => "F9",
        0xffc7 => "F10",
        0xffc8 => "F11",
        0xffc9 => "F12",
        _ => return None,
    })
}

/// The character a printable keysym denotes: Latin-1 keysyms (`0x20..=0xff`)
/// are their own codepoint; the Unicode range (`0x01000000 | cp`) carries the
/// codepoint in its low bits. Anything else is not printable.
fn printable_char(keysym: u32) -> Option<char> {
    let cp = if (0x20..=0xff).contains(&keysym) {
        keysym
    } else if (0x0100_0000..=0x0110_ffff).contains(&keysym) {
        keysym & 0x00ff_ffff
    } else {
        return None;
    };
    char::from_u32(cp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_negotiation_follows_client_version() {
        // macOS Screen Sharing pins to 3.3 → server dictates the type.
        assert_eq!(type_negotiation(b"RFB 003.003\n"), TypeNegotiation::Dictate);
        // 3.7 / 3.8 / Apple 3.889 → the client picks from a list.
        assert_eq!(type_negotiation(b"RFB 003.007\n"), TypeNegotiation::List);
        assert_eq!(type_negotiation(b"RFB 003.008\n"), TypeNegotiation::List);
        assert_eq!(type_negotiation(b"RFB 003.889\n"), TypeNegotiation::List);
    }

    #[test]
    fn client_minor_version_parses_and_defaults() {
        assert_eq!(client_minor_version(b"RFB 003.003\n"), 3);
        assert_eq!(client_minor_version(b"RFB 003.008\n"), 8);
        // Malformed minor field falls back to the conservative 3.
        assert_eq!(client_minor_version(b"RFB 003.xyz\n"), 3);
    }

    #[test]
    fn pixel_format_round_trips() {
        let pf = PixelFormat::server_default();
        assert_eq!(PixelFormat::parse(&pf.encode()), pf);
    }

    #[test]
    fn supported_accepts_real_formats_rejects_hostile_ones() {
        // The server's own format and a normal 16bpp 5-6-5 client format pass.
        assert!(PixelFormat::server_default().supported());
        assert!(PixelFormat {
            bits_per_pixel: 16,
            depth: 16,
            big_endian: false,
            true_color: true,
            red_max: 31,
            green_max: 63,
            blue_max: 31,
            red_shift: 11,
            green_shift: 5,
            blue_shift: 0,
        }
        .supported());

        let bad = |f: &dyn Fn(&mut PixelFormat)| {
            let mut pf = PixelFormat::server_default();
            f(&mut pf);
            assert!(!pf.supported());
        };
        // bits_per_pixel > 32 → would slice the 4-byte pixel out of range.
        bad(&|pf| pf.bits_per_pixel = 64);
        // bits_per_pixel not a positive multiple of 8 → bpp 0 / malformed frame.
        bad(&|pf| pf.bits_per_pixel = 0);
        bad(&|pf| pf.bits_per_pixel = 24);
        // shift >= bits_per_pixel → u32 shift overflow in encode_pixel.
        bad(&|pf| pf.red_shift = 200);
        bad(&|pf| pf.blue_shift = 32);
        // non-true-color is not something the encoder serves.
        bad(&|pf| pf.true_color = false);
        // a zero channel max would divide-scale to nothing meaningful.
        bad(&|pf| pf.green_max = 0);
    }

    #[test]
    fn default_format_encodes_pixel_as_bgrx_little_endian() {
        let pf = PixelFormat::server_default();
        let mut out = Vec::new();
        pf.encode_pixel([0x11, 0x22, 0x33], &mut out); // R,G,B
                                                       // value = 0x112233; little-endian 4 bytes = [33,22,11,00] = B,G,R,0.
        assert_eq!(out, vec![0x33, 0x22, 0x11, 0x00]);
    }

    #[test]
    fn big_endian_format_reverses_pixel_bytes() {
        let mut pf = PixelFormat::server_default();
        pf.big_endian = true;
        let mut out = Vec::new();
        pf.encode_pixel([0x11, 0x22, 0x33], &mut out);
        assert_eq!(out, vec![0x00, 0x11, 0x22, 0x33]);
    }

    #[test]
    fn sixteen_bpp_565_packs_two_bytes() {
        // A common 16bpp 5-6-5 format.
        let pf = PixelFormat {
            bits_per_pixel: 16,
            depth: 16,
            big_endian: false,
            true_color: true,
            red_max: 31,
            green_max: 63,
            blue_max: 31,
            red_shift: 11,
            green_shift: 5,
            blue_shift: 0,
        };
        let mut out = Vec::new();
        pf.encode_pixel([255, 255, 255], &mut out); // white → all bits set.
        assert_eq!(out.len(), 2);
        assert_eq!(u16::from_le_bytes([out[0], out[1]]), 0xffff);
        out.clear();
        pf.encode_pixel([255, 0, 0], &mut out); // pure red → top 5 bits.
        assert_eq!(u16::from_le_bytes([out[0], out[1]]), 0xf800);
    }

    #[test]
    fn server_init_has_size_format_and_name() {
        let pf = PixelFormat::server_default();
        let msg = server_init(1280, 800, &pf, "vmette");
        assert_eq!(u16::from_be_bytes([msg[0], msg[1]]), 1280);
        assert_eq!(u16::from_be_bytes([msg[2], msg[3]]), 800);
        // 4 (size) + 16 (format) + 4 (name-len) + 6 (name).
        assert_eq!(msg.len(), 4 + 16 + 4 + 6);
        let name_len = u32::from_be_bytes([msg[20], msg[21], msg[22], msg[23]]);
        assert_eq!(name_len, 6);
        assert_eq!(&msg[24..], b"vmette");
    }

    fn solid(w: u32, h: u32, rgb: [u8; 3]) -> Vec<u8> {
        let mut v = Vec::with_capacity((w * h * 3) as usize);
        for _ in 0..w * h {
            v.extend_from_slice(&rgb);
        }
        v
    }

    #[test]
    fn changed_rects_empty_for_identical_frames() {
        let a = solid(64, 64, [10, 10, 10]);
        assert!(changed_rects(&a, &a, 64, 64, 3, 32).is_empty());
    }

    #[test]
    fn changed_rects_covers_a_single_changed_tile() {
        let prev = solid(64, 64, [10, 10, 10]);
        let mut cur = prev.clone();
        // Flip one pixel inside the bottom-right 32×32 tile.
        let (px, py) = (40u32, 40u32);
        let i = ((py * 64 + px) * 3) as usize;
        cur[i] = 200;
        let rects = changed_rects(&prev, &cur, 64, 64, 3, 32);
        assert_eq!(rects.len(), 1);
        let r = rects[0];
        assert!(r.x <= px && r.y <= py && r.x + r.w > px && r.y + r.h > py);
    }

    #[test]
    fn changed_rects_coalesces_a_changed_row() {
        let prev = solid(96, 32, [0, 0, 0]);
        let cur = solid(96, 32, [255, 255, 255]); // whole frame changed
                                                  // One tile-row (h=32), three tile-cols coalesced into one rect.
        let rects = changed_rects(&prev, &cur, 96, 32, 3, 32);
        assert_eq!(rects.len(), 1);
        assert_eq!((rects[0].x, rects[0].w, rects[0].h), (0, 96, 32));
    }

    #[test]
    fn framebuffer_update_header_and_pixels() {
        let pf = PixelFormat::server_default();
        let px = solid(2, 1, [0x11, 0x22, 0x33]);
        let rects = vec![Rect {
            x: 0,
            y: 0,
            w: 2,
            h: 1,
        }];
        let msg = framebuffer_update(&rects, &pf, &px, 2, 3);
        assert_eq!(msg[0], 0); // FramebufferUpdate
        assert_eq!(u16::from_be_bytes([msg[2], msg[3]]), 1); // one rectangle
                                                             // rect header: x,y,w,h, encoding
        assert_eq!(u16::from_be_bytes([msg[4], msg[5]]), 0);
        assert_eq!(u16::from_be_bytes([msg[8], msg[9]]), 2); // w
        assert_eq!(u16::from_be_bytes([msg[10], msg[11]]), 1); // h
        assert_eq!(i32::from_be_bytes([msg[12], msg[13], msg[14], msg[15]]), 0);
        // two BGRX pixels follow.
        assert_eq!(
            &msg[16..],
            &[0x33, 0x22, 0x11, 0x00, 0x33, 0x22, 0x11, 0x00]
        );
    }

    #[test]
    fn pointer_hover_moves_cursor() {
        let mut st = PointerState::default();
        assert_eq!(
            pointer_actions(&mut st, 0, 100, 50),
            vec![Action::MouseMove { x: 100, y: 50 }]
        );
    }

    #[test]
    fn pointer_press_release_in_place_is_a_click() {
        let mut st = PointerState::default();
        let down = pointer_actions(&mut st, button::LEFT, 30, 40);
        assert_eq!(down, vec![Action::MouseMove { x: 30, y: 40 }]);
        let up = pointer_actions(&mut st, 0, 31, 41); // within dead zone
        assert_eq!(up, vec![Action::LeftClick]);
    }

    #[test]
    fn pointer_press_move_release_is_a_drag() {
        let mut st = PointerState::default();
        pointer_actions(&mut st, button::LEFT, 30, 40);
        // Move while held — suppressed (no MouseMove emitted).
        assert!(pointer_actions(&mut st, button::LEFT, 80, 90).is_empty());
        let up = pointer_actions(&mut st, 0, 120, 140);
        assert_eq!(up, vec![Action::LeftClickDrag { x: 120, y: 140 }]);
    }

    #[test]
    fn pointer_right_and_middle_click_on_release() {
        let mut st = PointerState::default();
        pointer_actions(&mut st, button::RIGHT, 10, 10);
        assert_eq!(
            pointer_actions(&mut st, 0, 10, 10),
            vec![Action::RightClick]
        );
        pointer_actions(&mut st, button::MIDDLE, 10, 10);
        assert_eq!(
            pointer_actions(&mut st, 0, 10, 10),
            vec![Action::MiddleClick]
        );
    }

    #[test]
    fn pointer_wheel_press_emits_one_scroll() {
        let mut st = PointerState::default();
        let down = pointer_actions(&mut st, button::WHEEL_DOWN, 5, 6);
        assert_eq!(
            down,
            vec![Action::Scroll {
                x: 5,
                y: 6,
                direction: ScrollDirection::Down,
                amount: 1,
            }]
        );
        // Releasing the wheel bit (same position) emits nothing — no scroll,
        // and no redundant move since the pointer hasn't shifted.
        let up = pointer_actions(&mut st, 0, 5, 6);
        assert!(up.is_empty());
    }

    #[test]
    fn key_printable_types_the_character() {
        let mut st = KeyState::default();
        assert_eq!(
            key_action(&mut st, true, 0x61), // 'a'
            Some(Action::Type { text: "a".into() })
        );
        // key-up emits nothing.
        assert_eq!(key_action(&mut st, false, 0x61), None);
    }

    #[test]
    fn key_shifted_printable_uses_client_folded_char() {
        let mut st = KeyState::default();
        // Client sends Shift down, then the already-shifted keysym 'A'.
        assert_eq!(key_action(&mut st, true, XK_SHIFT_L), None);
        assert_eq!(
            key_action(&mut st, true, 0x41), // 'A'
            Some(Action::Type { text: "A".into() })
        );
    }

    #[test]
    fn key_ctrl_combo_becomes_a_chord() {
        let mut st = KeyState::default();
        assert_eq!(key_action(&mut st, true, XK_CONTROL_L), None);
        assert_eq!(
            key_action(&mut st, true, 0x63), // 'c'
            Some(Action::Key {
                keys: "ctrl+c".into()
            })
        );
    }

    #[test]
    fn key_special_is_a_named_key() {
        let mut st = KeyState::default();
        assert_eq!(
            key_action(&mut st, true, 0xff0d), // Return
            Some(Action::Key {
                keys: "Return".into()
            })
        );
    }

    #[test]
    fn key_shift_tab_keeps_shift_for_special_keys() {
        let mut st = KeyState::default();
        key_action(&mut st, true, XK_SHIFT_L);
        assert_eq!(
            key_action(&mut st, true, 0xff09), // Tab
            Some(Action::Key {
                keys: "shift+Tab".into()
            })
        );
    }

    #[test]
    fn key_unicode_range_decodes_codepoint() {
        let mut st = KeyState::default();
        // U+00E9 é via the Unicode keysym range.
        assert_eq!(
            key_action(&mut st, true, 0x0100_00e9),
            Some(Action::Type { text: "é".into() })
        );
    }
}
