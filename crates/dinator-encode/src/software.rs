use anyhow::Context;
use openh264::encoder::{Encoder as H264Encoder, EncoderConfig};
use openh264::formats::YUVSlices;
use tracing::info;

use crate::{bgra_to_i420, EncodedFrame};

pub struct OpenH264Encoder {
    encoder: H264Encoder,
    width: u32,
    height: u32,
    force_kf: bool,
    yuv_buf: Vec<u8>,
    bitrate_bps: u32,
}

impl OpenH264Encoder {
    pub fn new(width: u32, height: u32, bitrate_bps: u32) -> anyhow::Result<Self> {
        let config = EncoderConfig::new().set_bitrate_bps(bitrate_bps);
        // openh264 0.6: encoder auto-initializes on first encode with dimensions from YUVSource
        let encoder = H264Encoder::with_api_config(openh264::OpenH264API::from_source(), config)
            .context("failed to create openh264 encoder")?;

        let yuv_size = (width * height * 3 / 2) as usize;
        info!(width, height, bitrate_bps, "openh264 encoder created");

        Ok(Self {
            encoder,
            width,
            height,
            force_kf: false,
            yuv_buf: vec![0u8; yuv_size],
            bitrate_bps,
        })
    }
}

impl crate::Encoder for OpenH264Encoder {
    fn encode(
        &mut self,
        bgra: &[u8],
        width: u32,
        height: u32,
    ) -> anyhow::Result<Option<EncodedFrame>> {
        if width != self.width || height != self.height {
            self.resize(width, height)?;
        }

        // Convert BGRA to I420
        bgra_to_i420(bgra, width, height, &mut self.yuv_buf);

        let w = width as usize;
        let h = height as usize;
        let y_size = w * h;
        let uv_size = (w / 2) * (h / 2);
        let yuv = YUVSlices::new(
            (
                &self.yuv_buf[..y_size],
                &self.yuv_buf[y_size..y_size + uv_size],
                &self.yuv_buf[y_size + uv_size..y_size + 2 * uv_size],
            ),
            (w, h),
            (w, w / 2, w / 2),
        );

        if self.force_kf {
            self.force_kf = false;
            self.encoder.force_intra_frame();
        }

        let bitstream = self
            .encoder
            .encode(&yuv)
            .context("openh264 encode failed")?;

        let mut data = Vec::new();
        bitstream.write_vec(&mut data);

        if data.is_empty() {
            return Ok(None);
        }

        let is_keyframe = has_idr_nal(&data);
        Ok(Some(EncodedFrame { data, is_keyframe }))
    }

    fn force_keyframe(&mut self) {
        self.force_kf = true;
    }

    fn resize(&mut self, width: u32, height: u32) -> anyhow::Result<()> {
        info!(width, height, "openh264 encoder resize");
        // openh264 auto-reinitializes when dimensions change, but we recreate
        // to ensure clean state
        let config = EncoderConfig::new().set_bitrate_bps(self.bitrate_bps);
        self.encoder = H264Encoder::with_api_config(openh264::OpenH264API::from_source(), config)
            .context("failed to recreate openh264 encoder")?;
        self.width = width;
        self.height = height;
        self.yuv_buf.resize((width * height * 3 / 2) as usize, 0);
        Ok(())
    }

    fn name(&self) -> &str {
        "openh264"
    }
}

/// Check if an Annex B bitstream contains an IDR NAL unit (type 5).
fn has_idr_nal(data: &[u8]) -> bool {
    let mut i = 0;
    while i + 3 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 {
            let nal_start = if data[i + 2] == 1 {
                i + 3
            } else if i + 3 < data.len() && data[i + 2] == 0 && data[i + 3] == 1 {
                i + 4
            } else {
                i += 1;
                continue;
            };
            if nal_start < data.len() && (data[nal_start] & 0x1F) == 5 {
                return true;
            }
            i = nal_start;
        } else {
            i += 1;
        }
    }
    false
}
