//! PNG helpers for the emulator's `png` output (8-bit RGB).

use anyhow::{bail, Result};

/// `ue2_core::render::BACKDROP` (0x101010): hidden overlays and transparent pixels.
const BACKDROP: [u8; 3] = [0x10, 0x10, 0x10];

pub fn decode_rgb(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>)> {
    let mut reader = png::Decoder::new(std::io::Cursor::new(bytes)).read_info()?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf)?;
    if info.color_type != png::ColorType::Rgb || info.bit_depth != png::BitDepth::Eight {
        bail!("unexpected PNG format {:?}/{:?}", info.color_type, info.bit_depth);
    }
    buf.truncate(info.buffer_size());
    Ok((info.width, info.height, buf))
}

pub fn encode_rgb(w: u32, h: u32, rgb: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header()?;
        writer.write_image_data(rgb)?;
        writer.finish()?;
    }
    Ok(out)
}

/// Nearest-neighbour upscale by `k`.
pub fn scale(w: u32, h: u32, rgb: &[u8], k: u32) -> (u32, u32, Vec<u8>) {
    let (sw, sh) = (w * k, h * k);
    let mut out = Vec::with_capacity((sw * sh * 3) as usize);
    for y in 0..sh {
        let row = (y / k) * w;
        for x in 0..sw {
            let i = ((row + x / k) * 3) as usize;
            out.extend_from_slice(&rgb[i..i + 3]);
        }
    }
    (sw, sh, out)
}

/// The renderer paints a hidden overlay entirely in the backdrop colour.
pub fn overlay_visible(rgb: &[u8]) -> bool {
    rgb.chunks_exact(3).any(|p| p != BACKDROP)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_scale_and_visibility() {
        let rgb = [0x10, 0x10, 0x10, 0xFF, 0, 0];
        let png = encode_rgb(2, 1, &rgb).unwrap();
        let (w, h, back) = decode_rgb(&png).unwrap();
        assert_eq!((w, h, back.as_slice()), (2, 1, &rgb[..]));
        let (sw, sh, big) = scale(2, 1, &rgb, 2);
        assert_eq!((sw, sh), (4, 2));
        assert_eq!(&big[..12], &[0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0xFF, 0, 0, 0xFF, 0, 0]);
        assert!(overlay_visible(&rgb));
        assert!(!overlay_visible(&[0x10; 12]));
    }
}
