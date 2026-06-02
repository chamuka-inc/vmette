//! Library face of `vmette-daemon`.
//!
//! The daemon ships as the `vmetted` binary ([`main.rs`](../main.rs)); this lib
//! target exposes the pieces of it that are pure and worth exercising on their
//! own — the [`settle`] perception module and the [`rfb`] VNC codec for the
//! live desktop view. Keeping them here (library modules the binary consumes
//! via `vmette_daemon::*`) lets them be unit-tested and benchmarked in
//! isolation, without standing up a VM, while they still live in the daemon
//! crate — their only consumer.

pub mod rfb;
pub mod settle;

use anyhow::{bail, Context, Result};

use settle::Frame;

/// Decode a screenshot PNG (the agent emits 8-bit RGB) into a [`Frame`] — the
/// shared decode for both the settle detector (registry) and the live view
/// (`view`). Accepts RGB or RGBA at 8-bit depth; anything else is an error
/// rather than a silent misread, since the agent's output is known.
pub fn decode_png(bytes: &[u8]) -> Result<Frame> {
    let decoder = png::Decoder::new(bytes);
    let mut reader = decoder.read_info().context("reading PNG header")?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).context("decoding PNG frame")?;
    if info.bit_depth != png::BitDepth::Eight {
        bail!("unsupported PNG bit depth {:?}", info.bit_depth);
    }
    let channels = match info.color_type {
        png::ColorType::Rgb => 3u8,
        png::ColorType::Rgba => 4u8,
        other => bail!("unsupported PNG color type {other:?}"),
    };
    buf.truncate(info.buffer_size());
    Ok(Frame::new(info.width, info.height, channels, buf))
}
