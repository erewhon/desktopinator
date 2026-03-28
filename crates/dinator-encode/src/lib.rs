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

/// Pack the AVC444v2 chroma residual stream (stream 2) from full-resolution U444/V444 planes.
///
/// Implements the MS-RDPEGFX 3.3.8.3.3 "YUV420p Stream Combination for YUV444v2 mode":
///
/// Stream 2 Y plane (full resolution, width x height):
///   Left half:  U444 at odd columns [2x+1, y] for all rows
///   Right half: V444 at odd columns [2x+1, y] for all rows
///
/// Stream 2 U plane (quarter resolution, width/2 x height/2):
///   Left quarter:  U444[4k,   2y+1] (odd rows, every 4th col offset 0)
///   Right quarter: V444[4k,   2y+1] (odd rows, every 4th col offset 0)
///
/// Stream 2 V plane (quarter resolution, width/2 x height/2):
///   Left quarter:  U444[4k+2, 2y+1] (odd rows, every 4th col offset 2)
///   Right quarter: V444[4k+2, 2y+1] (odd rows, every 4th col offset 2)
///
/// `chroma_yuv420` must be at least width*height*3/2 bytes.
pub fn pack_avc444v2_chroma(
    u444: &[u8],
    v444: &[u8],
    width: u32,
    height: u32,
    chroma_yuv420: &mut [u8],
) {
    let w = width as usize;
    let h = height as usize;
    let half_w = w / 2;
    let half_h = h / 2;
    let y_size = w * h;
    let uv_size = half_w * half_h;

    let (y_out, rest) = chroma_yuv420.split_at_mut(y_size);
    let (u_out, v_out) = rest.split_at_mut(uv_size);

    // Y plane: left half = U444 odd columns, right half = V444 odd columns
    for row in 0..h {
        for col in 0..half_w {
            let src_col = col * 2 + 1; // odd column in full-res
            let src_idx = row * w + src_col.min(w - 1);
            // Left half: U odd-col
            y_out[row * w + col] = if src_idx < u444.len() { u444[src_idx] } else { 128 };
            // Right half: V odd-col
            y_out[row * w + half_w + col] = if src_idx < v444.len() { v444[src_idx] } else { 128 };
        }
    }

    // U plane: left = U444[4k, odd_y], right = V444[4k, odd_y]
    let quarter_w = half_w / 2;
    for row in 0..half_h {
        let src_row = row * 2 + 1; // odd rows
        for col in 0..quarter_w {
            let src_col = col * 4; // every 4th column, offset 0
            let src_idx = src_row * w + src_col.min(w - 1);
            let dst_idx = row * half_w + col;
            u_out[dst_idx] = if src_idx < u444.len() { u444[src_idx] } else { 128 };
            u_out[dst_idx + quarter_w] = if src_idx < v444.len() { v444[src_idx] } else { 128 };
        }
    }

    // V plane: left = U444[4k+2, odd_y], right = V444[4k+2, odd_y]
    for row in 0..half_h {
        let src_row = row * 2 + 1;
        for col in 0..quarter_w {
            let src_col = col * 4 + 2; // every 4th column, offset 2
            let src_idx = src_row * w + src_col.min(w - 1);
            let dst_idx = row * half_w + col;
            v_out[dst_idx] = if src_idx < u444.len() { u444[src_idx] } else { 128 };
            v_out[dst_idx + quarter_w] = if src_idx < v444.len() { v444[src_idx] } else { 128 };
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

/// Downsample full-resolution YUV444 planes to I420 (YUV420 planar).
/// This is stream 1 of AVC444 — standard 4:2:0 subsample with 2x2 averaging.
/// `i420` must be at least width*height*3/2 bytes.
pub fn yuv444_to_i420(
    y444: &[u8],
    u444: &[u8],
    v444: &[u8],
    width: u32,
    height: u32,
    i420: &mut [u8],
) {
    let w = width as usize;
    let h = height as usize;
    let y_size = w * h;
    let uv_size = (w / 2) * (h / 2);

    let (y_out, rest) = i420.split_at_mut(y_size);
    let (u_out, v_out) = rest.split_at_mut(uv_size);

    // Y plane: copy directly (full resolution)
    y_out[..y_size.min(y444.len())].copy_from_slice(&y444[..y_size.min(y444.len())]);

    // U and V planes: average 2x2 blocks
    let half_w = w / 2;
    for row in 0..(h / 2) {
        for col in 0..half_w {
            let r0 = row * 2;
            let r1 = (r0 + 1).min(h - 1);
            let c0 = col * 2;
            let c1 = (c0 + 1).min(w - 1);

            let u_avg = (u444[r0 * w + c0] as u16
                + u444[r0 * w + c1] as u16
                + u444[r1 * w + c0] as u16
                + u444[r1 * w + c1] as u16)
                / 4;
            let v_avg = (v444[r0 * w + c0] as u16
                + v444[r0 * w + c1] as u16
                + v444[r1 * w + c0] as u16
                + v444[r1 * w + c1] as u16)
                / 4;

            u_out[row * half_w + col] = u_avg as u8;
            v_out[row * half_w + col] = v_avg as u8;
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
