use smithay::utils::{Physical, Rectangle};

/// Result of encoding a single dirty tile.
pub struct TileFrame {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    pub data: Vec<u8>,
    pub is_keyframe: bool,
}

/// A single tile in the grid with its own persistent H.264 encoder.
struct Tile {
    x: u16,
    y: u16,
    width: u16,
    height: u16,
    encoder: Box<dyn dinator_encode::Encoder>,
    dirty: bool,
    frames_since_keyframe: u32,
}

/// Grid of tiles covering an output, each with an independent H.264 encoder.
/// Only tiles that intersect damage rectangles are encoded each frame.
pub struct TileGrid {
    output_name: String,
    output_width: u16,
    output_height: u16,
    cols: u16,
    rows: u16,
    tiles: Vec<Tile>,
}

impl TileGrid {
    /// Create a new tile grid for an output.
    pub fn new(
        output_name: String,
        output_width: u16,
        output_height: u16,
        cols: u16,
        rows: u16,
        encoder_pref: &str,
    ) -> anyhow::Result<Self> {
        let base_w = output_width / cols;
        let base_h = output_height / rows;
        let mut tiles = Vec::with_capacity((cols * rows) as usize);

        for row in 0..rows {
            for col in 0..cols {
                let x = col * base_w;
                let y = row * base_h;
                // Edge tiles get the remaining pixels
                let w = if col == cols - 1 {
                    output_width - x
                } else {
                    base_w
                };
                let h = if row == rows - 1 {
                    output_height - y
                } else {
                    base_h
                };

                // Round up to even dimensions for H.264
                let ew = (w + 1) & !1;
                let eh = (h + 1) & !1;

                let encoder = create_tile_encoder(ew as u32, eh as u32, encoder_pref)
                    .map_err(|e| anyhow::anyhow!("tile ({col},{row}) encoder: {e}"))?;

                tiles.push(Tile {
                    x,
                    y,
                    width: w,
                    height: h,
                    encoder,
                    dirty: true, // first frame is always dirty
                    frames_since_keyframe: 0,
                });
            }
        }

        tracing::info!(
            output = %output_name,
            cols,
            rows,
            tile_count = tiles.len(),
            "adaptive tile grid created"
        );

        Ok(Self {
            output_name,
            output_width,
            output_height,
            cols,
            rows,
            tiles,
        })
    }

    /// Mark tiles that intersect any of the damage rectangles as dirty.
    pub fn mark_damage(&mut self, damage_rects: &[Rectangle<i32, Physical>]) {
        let mut marked = 0u32;
        for tile in &mut self.tiles {
            for rect in damage_rects {
                if rects_overlap(
                    tile.x as i32,
                    tile.y as i32,
                    tile.width as i32,
                    tile.height as i32,
                    rect.loc.x,
                    rect.loc.y,
                    rect.size.w,
                    rect.size.h,
                ) {
                    tile.dirty = true;
                    marked += 1;
                    break;
                }
            }
        }
        if marked > 0 || !damage_rects.is_empty() {
            tracing::debug!(
                damage_rects = damage_rects.len(),
                marked,
                total_tiles = self.tiles.len(),
                "mark_damage"
            );
        }
    }

    /// Encode all dirty tiles. Returns encoded frames for each dirty tile.
    /// `full_bgra` is the full output framebuffer in BGRA format.
    pub fn encode_dirty_tiles(&mut self, full_bgra: &[u8]) -> Vec<TileFrame> {
        let stride = self.output_width as usize * 4;
        let mut frames = Vec::new();

        for tile in &mut self.tiles {
            if !tile.dirty {
                continue;
            }
            tile.dirty = false;

            // Extract tile pixels from the full framebuffer
            let tw = tile.width as usize;
            let th = tile.height as usize;
            let mut tile_buf = vec![0u8; tw * th * 4];

            for row in 0..th {
                let src_y = tile.y as usize + row;
                let src_offset = src_y * stride + tile.x as usize * 4;
                let dst_offset = row * tw * 4;
                if src_offset + tw * 4 <= full_bgra.len() {
                    tile_buf[dst_offset..dst_offset + tw * 4]
                        .copy_from_slice(&full_bgra[src_offset..src_offset + tw * 4]);
                }
            }

            match tile
                .encoder
                .encode(&tile_buf, tw as u32, th as u32)
            {
                Ok(Some(encoded)) => {
                    if encoded.is_keyframe {
                        tile.frames_since_keyframe = 0;
                    } else {
                        tile.frames_since_keyframe += 1;
                    }

                    // Skip empty P-frames
                    if encoded.data.len() < 50 && !encoded.is_keyframe {
                        continue;
                    }

                    frames.push(TileFrame {
                        x: tile.x,
                        y: tile.y,
                        width: tile.width,
                        height: tile.height,
                        data: encoded.data,
                        is_keyframe: encoded.is_keyframe,
                    });
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(
                        tile_x = tile.x,
                        tile_y = tile.y,
                        error = %e,
                        "tile encode error"
                    );
                }
            }
        }

        frames
    }

    /// Force keyframe on all tile encoders.
    pub fn force_all_keyframes(&mut self) {
        for tile in &mut self.tiles {
            tile.encoder.force_keyframe();
            tile.dirty = true;
        }
    }

    /// Force keyframe on one tile (round-robin staggering).
    /// Call once per frame to spread keyframes over time.
    pub fn stagger_keyframe(&mut self, frame_count: u64) {
        let idx = (frame_count as usize) % self.tiles.len();
        if self.tiles[idx].frames_since_keyframe > 120 {
            self.tiles[idx].encoder.force_keyframe();
            self.tiles[idx].dirty = true;
        }
    }

    /// Resize the grid (recreates all encoders).
    pub fn resize(
        &mut self,
        output_width: u16,
        output_height: u16,
        encoder_pref: &str,
    ) -> anyhow::Result<()> {
        let new = Self::new(
            self.output_name.clone(),
            output_width,
            output_height,
            self.cols,
            self.rows,
            encoder_pref,
        )?;
        *self = new;
        Ok(())
    }

    pub fn output_name(&self) -> &str {
        &self.output_name
    }

    pub fn tile_count(&self) -> usize {
        self.tiles.len()
    }

    pub fn dirty_count(&self) -> usize {
        self.tiles.iter().filter(|t| t.dirty).count()
    }
}

fn rects_overlap(
    ax: i32,
    ay: i32,
    aw: i32,
    ah: i32,
    bx: i32,
    by: i32,
    bw: i32,
    bh: i32,
) -> bool {
    ax < bx + bw && ax + aw > bx && ay < by + bh && ay + ah > by
}

fn create_tile_encoder(
    width: u32,
    height: u32,
    pref: &str,
) -> anyhow::Result<Box<dyn dinator_encode::Encoder>> {
    // Try preferred encoder, fall back to openh264
    if pref != "openh264" {
        let ffmpeg_pref = match pref {
            "vaapi" => dinator_encode::FfmpegEncoderPreference::Vaapi,
            "nvenc" => dinator_encode::FfmpegEncoderPreference::Nvenc,
            "x264" => dinator_encode::FfmpegEncoderPreference::Software,
            _ => dinator_encode::FfmpegEncoderPreference::Auto,
        };
        if let Ok(enc) = dinator_encode::FfmpegEncoder::new(width, height, 2_000_000, ffmpeg_pref) {
            return Ok(Box::new(enc));
        }
    }
    let enc = dinator_encode::OpenH264Encoder::new(width, height, 500_000)?;
    Ok(Box::new(enc))
}
