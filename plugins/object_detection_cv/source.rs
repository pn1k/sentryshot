// SPDX-License-Identifier: GPL-2.0-or-later

use sentryshot_convert::{Frame, PixelFormat};
use thiserror::Error;

#[derive(Debug, Error)]
pub(crate) enum GrayError {
    #[error("unsupported pixel format: {0}")]
    Unsupported(PixelFormat),

    #[error("frame is too small for scale: {0}x{1} / {2}")]
    TooSmall(u16, u16, u8),

    #[error("invalid linesize {0} for width {1}")]
    BadLinesize(usize, u16),
}

/// Builds a lookup table mapping a box-filter sum to its averaged byte, so
/// the per-pixel division by `factor^2` disappears from the hot loop.
///
/// Only needed for `scale > 1`.
pub(crate) fn build_div_lut(factor: u8) -> Vec<u8> {
    let factor = usize::from(factor).max(1);
    let inv = u32::try_from(factor * factor).unwrap_or(u32::MAX);
    (0..=255 * inv)
        .map(|s| u8::try_from(s / inv).unwrap_or(255))
        .collect()
}

/// Extracts the luma (Y) plane as a compact `GRAY8` buffer, optionally
/// downscaled by an integer factor using a box filter.
///
/// `lut` must come from [`build_div_lut`] with the same `scale`; it is unused
/// when `scale` is 1.
///
/// Returns the analyzed `(width, height)`.
pub(crate) fn frame_to_gray(
    frame: &Frame,
    scale: u8,
    lut: &[u8],
    out: &mut Vec<u8>,
) -> Result<(u16, u16), GrayError> {
    let width = frame.width().get();
    let height = frame.height().get();
    let w = usize::from(width);
    let h = usize::from(height);
    let stride = frame.linesize()[0];

    let plane = match frame.pix_fmt() {
        // The Y plane of YUV420P/YUVJ420P and the sole plane of GRAY8 are luma.
        PixelFormat::YUV420P | PixelFormat::YUVJ420P | PixelFormat::GRAY8 => {
            frame.data()[0].as_slice()
        }
        other => return Err(GrayError::Unsupported(other)),
    };

    if stride < w || plane.len() < stride * h {
        return Err(GrayError::BadLinesize(stride, width));
    }

    let factor = usize::from(scale).max(1);
    if factor == 1 {
        // No downscale: write the luma plane directly, skipping the scratch
        // buffer and a second full-frame copy.
        out.clear();
        if stride == w {
            // Tight plane: one copy instead of a row-by-row loop.
            out.extend_from_slice(&plane[..w * h]);
        } else {
            for y in 0..h {
                let start = y * stride;
                out.extend_from_slice(&plane[start..start + w]);
            }
        }
        return Ok((width, height));
    }

    let aw = w / factor;
    let ah = h / factor;
    if aw == 0 || ah == 0 {
        return Err(GrayError::TooSmall(width, height, scale));
    }

    // Box filter straight from the (padded) plane, no intermediate copy.
    out.clear();
    out.resize(aw * ah, 0);
    for ay in 0..ah {
        let y0 = ay * factor;
        let orow = ay * aw;
        for ax in 0..aw {
            let x0 = ax * factor;
            let mut sum: u32 = 0;
            for dy in 0..factor {
                let row = (y0 + dy) * stride + x0;
                for dx in 0..factor {
                    sum += u32::from(plane[row + dx]);
                }
            }
            out[orow + ax] = lut[usize::try_from(sum).unwrap_or(0)];
        }
    }

    Ok((
        u16::try_from(aw).unwrap_or(u16::MAX),
        u16::try_from(ah).unwrap_or(u16::MAX),
    ))
}

#[allow(clippy::unwrap_used, clippy::unseparated_literal_suffix)]
#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU16;

    fn gray_frame(width: u16, height: u16, pixels: &[u8]) -> Frame {
        Frame::from_raw(
            pixels,
            PixelFormat::GRAY8,
            NonZeroU16::new(width).unwrap(),
            NonZeroU16::new(height).unwrap(),
            1,
        )
        .unwrap()
    }

    #[test]
    fn test_full_res_copy() {
        let frame = gray_frame(2, 2, &[1, 2, 3, 4]);
        let mut out = Vec::new();
        let (w, h) = frame_to_gray(&frame, 1, &[], &mut out).unwrap();
        assert_eq!((2, 2), (w, h));
        assert_eq!(vec![1, 2, 3, 4], out);
    }

    #[test]
    fn test_downscale_box() {
        // 4x4 all 10 except the bottom-right 2x2 block which is 0.
        let mut pixels = vec![10u8; 16];
        pixels[10] = 0;
        pixels[11] = 0;
        pixels[14] = 0;
        pixels[15] = 0;
        let frame = gray_frame(4, 4, &pixels);
        let lut = build_div_lut(2);
        let mut out = Vec::new();
        let (w, h) = frame_to_gray(&frame, 2, &lut, &mut out).unwrap();
        assert_eq!((2, 2), (w, h));
        assert_eq!(vec![10, 10, 10, 0], out);
    }

    #[test]
    fn test_downscale_box_padded_stride() {
        // 4x4 frame on an 8-byte stride must read the real pixels, not padding.
        let mut pixels = vec![255u8; 8 * 4];
        for y in 0..4 {
            for x in 0..4 {
                pixels[y * 8 + x] = 10;
            }
        }
        let frame = Frame::from_raw(
            &pixels,
            PixelFormat::GRAY8,
            NonZeroU16::new(4).unwrap(),
            NonZeroU16::new(4).unwrap(),
            8,
        )
        .unwrap();
        let lut = build_div_lut(2);
        let mut out = Vec::new();
        let (w, h) = frame_to_gray(&frame, 2, &lut, &mut out).unwrap();
        assert_eq!((2, 2), (w, h));
        assert_eq!(vec![10, 10, 10, 10], out);
    }

    #[test]
    fn test_unsupported() {
        let mut frame = gray_frame(2, 2, &[0, 0, 0, 0]);
        frame.set_pix_fmt(PixelFormat::YUV422P);
        let mut out = Vec::new();
        assert!(frame_to_gray(&frame, 1, &[], &mut out).is_err());
    }
}
