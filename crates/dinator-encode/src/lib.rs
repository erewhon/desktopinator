#[cfg(feature = "software")]
mod software;

#[cfg(feature = "ffmpeg")]
mod ffmpeg;

#[cfg(feature = "software")]
pub use software::OpenH264Encoder;

#[cfg(feature = "ffmpeg")]
pub use self::ffmpeg::{FfmpegEncoder, FfmpegEncoderPreference};

/// Encoded output from a single frame.
pub struct EncodedFrame {
    /// H.264 NAL unit data (Annex B format).
    pub data: Vec<u8>,
    /// Whether this frame is a keyframe (IDR).
    pub is_keyframe: bool,
}

/// Trait for video encoders that accept BGRA frames and produce H.264.
pub trait Encoder: Send {
    /// Encode a single frame. `bgra` is width*height*4 bytes in BGRA format.
    /// Returns encoded H.264 NAL units, or None if the encoder buffered the frame.
    fn encode(
        &mut self,
        bgra: &[u8],
        width: u32,
        height: u32,
    ) -> anyhow::Result<Option<EncodedFrame>>;

    /// Encode a frame from raw I420 (YUV420 planar) data.
    /// `i420` is width*height*3/2 bytes: Y plane, then U plane, then V plane.
    /// Default implementation falls back to `encode()` (ignoring that it's not BGRA).
    fn encode_i420(
        &mut self,
        _i420: &[u8],
        _width: u32,
        _height: u32,
    ) -> anyhow::Result<Option<EncodedFrame>> {
        // Default: not supported
        anyhow::bail!("encode_i420 not supported by this encoder")
    }

    /// Force a keyframe on the next encode call.
    fn force_keyframe(&mut self);

    /// Handle a resolution change. Returns Err if the encoder can't resize.
    fn resize(&mut self, width: u32, height: u32) -> anyhow::Result<()>;

    /// Encoder name for logging.
    fn name(&self) -> &str;
}

/// Pack the AVC444 chroma residual stream (stream 2) from full-resolution U444/V444 planes.
///
/// Takes the full-resolution U and V planes (width*height each) and packs them
/// into a YUV420-shaped buffer suitable for encoding with a standard H.264 encoder.
///
/// The packing follows the MS-RDPEGFX AVC444 B4-B7 scheme:
/// - "Y" plane of stream 2: odd-column U and V values, interleaved in 8-row blocks
/// - "U" plane: odd-row, even-column U values (quarter res)
/// - "V" plane: odd-row, even-column V values (quarter res)
///
/// `chroma_yuv420` must be at least width*height*3/2 bytes.
pub fn pack_avc444_chroma(
    u444: &[u8],
    v444: &[u8],
    width: u32,
    height: u32,
    chroma_yuv420: &mut [u8],
) {
    let w = width as usize;
    let h = height as usize;
    let y_size = w * h;
    let uv_size = (w / 2) * (h / 2);

    let (y_out, rest) = chroma_yuv420.split_at_mut(y_size);
    let (u_out, v_out) = rest.split_at_mut(uv_size);

    // "Y" plane of stream 2: pack odd-column chroma values
    // For each row, take U/V values at odd columns
    // Interleave in 16-line groups: lines 0-7 = U odd-col, lines 8-15 = V odd-col
    for row in 0..h {
        let block_group = row / 16;
        let line_in_group = row % 16;
        let is_v = line_in_group >= 8;
        let src_row = block_group * 16 + if is_v { line_in_group - 8 } else { line_in_group };
        if src_row >= h {
            continue;
        }

        for col in 0..w {
            let src_col = col * 2 + 1; // odd columns
            if src_col >= w {
                y_out[row * w + col] = 128;
                continue;
            }
            let src_idx = src_row * w + src_col;
            y_out[row * w + col] = if is_v {
                if src_idx < v444.len() { v444[src_idx] } else { 128 }
            } else {
                if src_idx < u444.len() { u444[src_idx] } else { 128 }
            };
        }
    }

    // "U" plane: odd-row, even-column U values (quarter resolution)
    for row in 0..(h / 2) {
        for col in 0..(w / 2) {
            let src_row = row * 2 + 1; // odd rows
            let src_col = col * 2;     // even columns
            let src_idx = src_row * w + src_col;
            u_out[row * (w / 2) + col] = if src_idx < u444.len() { u444[src_idx] } else { 128 };
        }
    }

    // "V" plane: odd-row, even-column V values (quarter resolution)
    for row in 0..(h / 2) {
        for col in 0..(w / 2) {
            let src_row = row * 2 + 1;
            let src_col = col * 2;
            let src_idx = src_row * w + src_col;
            v_out[row * (w / 2) + col] = if src_idx < v444.len() { v444[src_idx] } else { 128 };
        }
    }
}

/// Convert BGRA pixels to full-resolution YUV444 planes.
/// `y444` must be width*height, `u444` and `v444` must be width*height each.
pub fn bgra_to_yuv444(bgra: &[u8], width: u32, height: u32, y444: &mut [u8], u444: &mut [u8], v444: &mut [u8]) {
    let w = width as usize;
    let h = height as usize;
    for row in 0..h {
        for col in 0..w {
            let px = (row * w + col) * 4;
            let b = bgra[px] as i32;
            let g = bgra[px + 1] as i32;
            let r = bgra[px + 2] as i32;
            let idx = row * w + col;
            y444[idx] = ((66 * r + 129 * g + 25 * b + 128) >> 8).wrapping_add(16).clamp(0, 255) as u8;
            u444[idx] = ((-38 * r - 74 * g + 112 * b + 128) >> 8).wrapping_add(128).clamp(0, 255) as u8;
            v444[idx] = ((112 * r - 94 * g - 18 * b + 128) >> 8).wrapping_add(128).clamp(0, 255) as u8;
        }
    }
}

/// Convert BGRA pixels to I420 (YUV420 planar) in-place into the provided buffer.
/// `yuv` must be at least width*height*3/2 bytes.
pub(crate) fn bgra_to_i420(bgra: &[u8], width: u32, height: u32, yuv: &mut [u8]) {
    let w = width as usize;
    let h = height as usize;
    let y_size = w * h;
    let uv_size = (w / 2) * (h / 2);

    let (y_plane, rest) = yuv.split_at_mut(y_size);
    let (u_plane, v_plane) = rest.split_at_mut(uv_size);

    // Y plane: full resolution
    for row in 0..h {
        for col in 0..w {
            let px = (row * w + col) * 4;
            let b = bgra[px] as i32;
            let g = bgra[px + 1] as i32;
            let r = bgra[px + 2] as i32;
            y_plane[row * w + col] = ((66 * r + 129 * g + 25 * b + 128) >> 8)
                .wrapping_add(16)
                .clamp(0, 255) as u8;
        }
    }

    // U and V planes: half resolution, average 2x2 blocks
    for row in (0..h).step_by(2) {
        for col in (0..w).step_by(2) {
            let mut r_sum = 0i32;
            let mut g_sum = 0i32;
            let mut b_sum = 0i32;
            for dy in 0..2 {
                for dx in 0..2 {
                    let y = (row + dy).min(h - 1);
                    let x = (col + dx).min(w - 1);
                    let px = (y * w + x) * 4;
                    b_sum += bgra[px] as i32;
                    g_sum += bgra[px + 1] as i32;
                    r_sum += bgra[px + 2] as i32;
                }
            }
            let r = r_sum / 4;
            let g = g_sum / 4;
            let b = b_sum / 4;
            let uv_idx = (row / 2) * (w / 2) + col / 2;
            u_plane[uv_idx] = ((-38 * r - 74 * g + 112 * b + 128) >> 8)
                .wrapping_add(128)
                .clamp(0, 255) as u8;
            v_plane[uv_idx] = ((112 * r - 94 * g - 18 * b + 128) >> 8)
                .wrapping_add(128)
                .clamp(0, 255) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bgra_to_i420_black() {
        let w = 4u32;
        let h = 4u32;
        let bgra = vec![0u8; (w * h * 4) as usize];
        let mut yuv = vec![0u8; (w * h * 3 / 2) as usize];
        bgra_to_i420(&bgra, w, h, &mut yuv);
        // Black pixel: Y=16, U=128, V=128 (BT.601)
        assert_eq!(yuv[0], 16); // Y
        let y_size = (w * h) as usize;
        assert_eq!(yuv[y_size], 128); // U
        let uv_size = (w / 2 * h / 2) as usize;
        assert_eq!(yuv[y_size + uv_size], 128); // V
    }

    #[cfg(feature = "software")]
    #[test]
    fn encode_one_frame() {
        let w = 64u32;
        let h = 64u32;
        let bgra = vec![128u8; (w * h * 4) as usize];
        let mut enc = OpenH264Encoder::new(w, h, 500_000).unwrap();
        let result = enc.encode(&bgra, w, h).unwrap();
        assert!(result.is_some(), "first frame should produce output");
        let frame = result.unwrap();
        assert!(!frame.data.is_empty());
        assert!(frame.is_keyframe, "first frame should be a keyframe");
    }

    #[cfg(feature = "ffmpeg")]
    #[test]
    fn ffmpeg_libx264_encode() {
        let w = 64u32;
        let h = 64u32;
        let bgra = vec![128u8; (w * h * 4) as usize];
        let mut enc = FfmpegEncoder::new(w, h, 500_000, FfmpegEncoderPreference::Software).unwrap();
        // libx264 may need a few frames before producing output
        let mut got_output = false;
        for _ in 0..5 {
            if let Ok(Some(frame)) = enc.encode(&bgra, w, h) {
                assert!(!frame.data.is_empty());
                got_output = true;
                break;
            }
        }
        assert!(got_output, "libx264 should produce output within 5 frames");
    }
}
