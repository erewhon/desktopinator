use anyhow::{bail, Context};
use tracing::info;

use crate::EncodedFrame;

/// Which hardware encoder to attempt, falling back through the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FfmpegEncoderPreference {
    /// Try VAAPI first, then NVENC, then libx264.
    Auto,
    /// Force VAAPI (h264_vaapi).
    Vaapi,
    /// Force NVENC (h264_nvenc).
    Nvenc,
    /// Force software (libx264).
    Software,
}

pub struct FfmpegEncoder {
    encoder: ffmpeg_next::codec::encoder::Video,
    converter: ffmpeg_next::software::scaling::Context,
    width: u32,
    height: u32,
    frame_index: i64,
    force_kf: bool,
    encoder_name: String,
    bitrate_bps: u32,
}

// The ffmpeg types contain raw pointers that aren't Send, but we only
// use the encoder from a single thread (the compositor render loop).
unsafe impl Send for FfmpegEncoder {}

impl FfmpegEncoder {
    pub fn new(
        width: u32,
        height: u32,
        bitrate_bps: u32,
        preference: FfmpegEncoderPreference,
    ) -> anyhow::Result<Self> {
        ffmpeg_next::init().context("failed to init ffmpeg")?;

        let encoder_names = match preference {
            FfmpegEncoderPreference::Auto => vec!["h264_vaapi", "h264_nvenc", "libx264"],
            FfmpegEncoderPreference::Vaapi => vec!["h264_vaapi"],
            FfmpegEncoderPreference::Nvenc => vec!["h264_nvenc"],
            FfmpegEncoderPreference::Software => vec!["libx264"],
        };

        let mut last_err = None;
        for name in &encoder_names {
            match try_create_encoder(name, width, height, bitrate_bps) {
                Ok((encoder, converter)) => {
                    info!(
                        encoder = name,
                        width, height, bitrate_bps, "FFmpeg encoder created"
                    );
                    return Ok(Self {
                        encoder,
                        converter,
                        width,
                        height,
                        frame_index: 0,
                        force_kf: false,
                        encoder_name: name.to_string(),
                        bitrate_bps,
                    });
                }
                Err(e) => {
                    info!(encoder = name, error = %e, "encoder not available, trying next");
                    last_err = Some(e);
                }
            }
        }

        bail!(
            "no H.264 encoder available (tried {:?}): {}",
            encoder_names,
            last_err.map(|e| e.to_string()).unwrap_or_default()
        )
    }
}

fn try_create_encoder(
    name: &str,
    width: u32,
    height: u32,
    bitrate_bps: u32,
) -> anyhow::Result<(
    ffmpeg_next::codec::encoder::Video,
    ffmpeg_next::software::scaling::Context,
)> {
    use ffmpeg_next::codec;
    use ffmpeg_next::format::Pixel;

    // H.264 with YUV420P requires even dimensions — round up if needed
    let enc_width = (width + 1) & !1;
    let enc_height = (height + 1) & !1;

    let codec = codec::encoder::find_by_name(name)
        .with_context(|| format!("encoder '{name}' not found"))?;

    let ctx = codec::context::Context::new_with_codec(codec);
    let mut encoder_ctx = ctx.encoder().video()?;

    encoder_ctx.set_width(enc_width);
    encoder_ctx.set_height(enc_height);
    encoder_ctx.set_time_base(ffmpeg_next::Rational::new(1, 60));
    // No periodic keyframes — they overwhelm the RDP DVC channel (100-300KB bursts).
    // Keyframes are only produced on-demand via force_keyframe() (connect, resize).
    encoder_ctx.set_gop(i32::MAX as u32);
    encoder_ctx.set_max_b_frames(0); // low latency

    // Set pixel format based on encoder
    let target_format = if name == "h264_vaapi" {
        Pixel::NV12
    } else {
        Pixel::YUV420P
    };
    encoder_ctx.set_format(target_format);

    // Set encoder-specific options
    let mut opts = ffmpeg_next::Dictionary::new();
    match name {
        "libx264" => {
            // "Capped CRF" mode: CRF 23 for good quality with smaller frames,
            // VBV rate control caps frame sizes during complex scenes (animations).
            // Higher CRF (23 vs 20) produces noticeably smaller P-frames during
            // motion, reducing dropped frames and pixelation.
            opts.set("crf", "23");
            encoder_ctx.set_max_bit_rate(10_000_000); // 10Mbps max burst
            // VBV buffer size controls max single-frame burst.
            // 4Mbits = 500KB max frame, matches MAX_GFX_FRAME_BYTES.
            unsafe {
                (*encoder_ctx.as_mut_ptr()).rc_buffer_size = 4_000_000;
            }
            opts.set("preset", "ultrafast");
            opts.set("tune", "zerolatency");
            opts.set("forced-idr", "1"); // force_keyframe() produces IDR (not just I-frame)
        }
        "h264_nvenc" => {
            encoder_ctx.set_bit_rate(bitrate_bps as usize);
            opts.set("preset", "p1"); // fastest
            opts.set("tune", "ull"); // ultra low latency
            opts.set("zerolatency", "1");
        }
        "h264_vaapi" => {
            encoder_ctx.set_bit_rate(bitrate_bps as usize);
            opts.set("low_power", "1");
        }
        _ => {
            encoder_ctx.set_bit_rate(bitrate_bps as usize);
        }
    }

    let encoder = encoder_ctx.open_with(opts)?;

    // Create scaler: BGRA (original size) → target pixel format (even size for encoder)
    let converter = ffmpeg_next::software::scaling::Context::get(
        Pixel::BGRA,
        width,
        height,
        target_format,
        enc_width,
        enc_height,
        ffmpeg_next::software::scaling::Flags::FAST_BILINEAR,
    )?;

    Ok((encoder, converter))
}

impl crate::Encoder for FfmpegEncoder {
    fn encode(
        &mut self,
        bgra: &[u8],
        width: u32,
        height: u32,
    ) -> anyhow::Result<Option<EncodedFrame>> {
        use ffmpeg_next::format::Pixel;

        if width != self.width || height != self.height {
            self.resize(width, height)?;
        }

        // Create input frame from BGRA data
        let mut input_frame = ffmpeg_next::frame::Video::new(Pixel::BGRA, width, height);
        let stride = input_frame.stride(0);
        let data = input_frame.data_mut(0);

        // Copy BGRA data row by row (stride may differ from width*4)
        let src_stride = width as usize * 4;
        for row in 0..height as usize {
            let src_start = row * src_stride;
            let dst_start = row * stride;
            let len = src_stride.min(stride);
            data[dst_start..dst_start + len].copy_from_slice(&bgra[src_start..src_start + len]);
        }

        // Convert to encoder's pixel format
        let mut yuv_frame = ffmpeg_next::frame::Video::empty();
        self.converter.run(&input_frame, &mut yuv_frame)?;

        yuv_frame.set_pts(Some(self.frame_index));
        self.frame_index += 1;

        if self.force_kf {
            self.force_kf = false;
            yuv_frame.set_kind(ffmpeg_next::picture::Type::I);
        }

        // Send frame to encoder
        self.encoder.send_frame(&yuv_frame)?;

        // Receive encoded packet
        let mut packet = ffmpeg_next::Packet::empty();
        match self.encoder.receive_packet(&mut packet) {
            Ok(()) => {
                let data = packet.data().unwrap_or(&[]).to_vec();
                let is_keyframe = packet.is_key();
                Ok(Some(EncodedFrame { data, is_keyframe }))
            }
            Err(ffmpeg_next::Error::Other { errno })
                if errno == ffmpeg_next::util::error::EAGAIN =>
            {
                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    }

    fn encode_i420(
        &mut self,
        i420: &[u8],
        width: u32,
        height: u32,
    ) -> anyhow::Result<Option<EncodedFrame>> {
        use ffmpeg_next::format::Pixel;

        if width != self.width || height != self.height {
            self.resize(width, height)?;
        }

        let enc_width = self.encoder.width();
        let enc_height = self.encoder.height();

        // Create YUV420P frame directly — no color conversion needed
        let mut yuv_frame = ffmpeg_next::frame::Video::new(Pixel::YUV420P, enc_width, enc_height);

        let w = width as usize;
        let h = height as usize;
        let y_size = w * h;
        let uv_size = (w / 2) * (h / 2);

        // Copy Y plane
        let y_stride = yuv_frame.stride(0);
        let y_data = yuv_frame.data_mut(0);
        for row in 0..h.min(enc_height as usize) {
            let src = row * w;
            let dst = row * y_stride;
            let len = w.min(y_stride);
            if src + len <= i420.len() {
                y_data[dst..dst + len].copy_from_slice(&i420[src..src + len]);
            }
        }

        // Copy U plane
        let u_stride = yuv_frame.stride(1);
        let u_data = yuv_frame.data_mut(1);
        let uh = h / 2;
        let uw = w / 2;
        for row in 0..uh.min(enc_height as usize / 2) {
            let src = y_size + row * uw;
            let dst = row * u_stride;
            let len = uw.min(u_stride);
            if src + len <= i420.len() {
                u_data[dst..dst + len].copy_from_slice(&i420[src..src + len]);
            }
        }

        // Copy V plane
        let v_stride = yuv_frame.stride(2);
        let v_data = yuv_frame.data_mut(2);
        for row in 0..uh.min(enc_height as usize / 2) {
            let src = y_size + uv_size + row * uw;
            let dst = row * v_stride;
            let len = uw.min(v_stride);
            if src + len <= i420.len() {
                v_data[dst..dst + len].copy_from_slice(&i420[src..src + len]);
            }
        }

        yuv_frame.set_pts(Some(self.frame_index));
        self.frame_index += 1;

        if self.force_kf {
            self.force_kf = false;
            yuv_frame.set_kind(ffmpeg_next::picture::Type::I);
        }

        self.encoder.send_frame(&yuv_frame)?;

        let mut packet = ffmpeg_next::Packet::empty();
        match self.encoder.receive_packet(&mut packet) {
            Ok(()) => {
                let data = packet.data().unwrap_or(&[]).to_vec();
                let is_keyframe = packet.is_key();
                Ok(Some(EncodedFrame { data, is_keyframe }))
            }
            Err(ffmpeg_next::Error::Other { errno })
                if errno == ffmpeg_next::util::error::EAGAIN =>
            {
                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    }

    fn force_keyframe(&mut self) {
        self.force_kf = true;
    }

    fn resize(&mut self, width: u32, height: u32) -> anyhow::Result<()> {
        info!(
            width,
            height,
            encoder = %self.encoder_name,
            "FFmpeg encoder resize — recreating"
        );
        let (encoder, converter) =
            try_create_encoder(&self.encoder_name, width, height, self.bitrate_bps)?;
        self.encoder = encoder;
        self.converter = converter;
        self.width = width;
        self.height = height;
        self.frame_index = 0;
        self.force_kf = true; // first frame after resize must be a keyframe
        Ok(())
    }

    fn name(&self) -> &str {
        &self.encoder_name
    }
}
