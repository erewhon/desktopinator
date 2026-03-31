use fontdue::{Font, FontSettings};
use std::sync::OnceLock;

static FONT: OnceLock<Font> = OnceLock::new();

const FONT_BYTES: &[u8] = include_bytes!("../assets/Inter.ttf");

fn font() -> &'static Font {
    FONT.get_or_init(|| {
        Font::from_bytes(FONT_BYTES, FontSettings {
            scale: 40.0,
            ..FontSettings::default()
        })
        .expect("failed to load embedded font")
    })
}

/// Render text into an ARGB8888 pixel buffer.
/// Renders at 2x resolution internally then downscales for smoother anti-aliasing.
/// Returns the buffer as Vec<u8> with dimensions (width, height).
pub fn render_text(
    text: &str,
    font_size: f32,
    color: [u8; 3],
    max_width: i32,
    height: i32,
) -> (Vec<u8>, i32, i32) {
    let font = font();

    // Render at 2x for supersampled anti-aliasing
    let scale = 2;
    let px_size = font_size * scale as f32;
    let hi_max_w = max_width * scale;
    let hi_h = height * scale;

    // Rasterize each glyph at 2x
    let mut glyphs: Vec<(fontdue::Metrics, Vec<u8>)> = Vec::new();
    let mut total_width: i32 = 0;

    for ch in text.chars() {
        let (metrics, bitmap) = font.rasterize(ch, px_size);
        total_width += metrics.advance_width as i32;
        glyphs.push((metrics, bitmap));
    }

    let hi_w = total_width.min(hi_max_w).max(1);
    let mut hi_alpha = vec![0u8; (hi_w * hi_h) as usize];

    // Baseline at 2x
    let metrics = font.horizontal_line_metrics(px_size);
    let baseline = if let Some(m) = metrics {
        let text_h = m.ascent - m.descent;
        ((hi_h as f32 - text_h) / 2.0 + m.ascent) as i32
    } else {
        hi_h * 3 / 4
    };

    let mut cursor_x: i32 = 0;

    for (metrics, bitmap) in &glyphs {
        let gx = cursor_x + metrics.xmin;
        let gy = baseline - metrics.height as i32 - metrics.ymin;

        for row in 0..metrics.height {
            for col in 0..metrics.width {
                let px = gx + col as i32;
                let py = gy + row as i32;
                if px < 0 || px >= hi_w || py < 0 || py >= hi_h {
                    continue;
                }
                let a = bitmap[row * metrics.width + col];
                if a > 0 {
                    let idx = (py * hi_w + px) as usize;
                    // Max blend for overlapping glyphs
                    hi_alpha[idx] = hi_alpha[idx].max(a);
                }
            }
        }

        cursor_x += metrics.advance_width as i32;
        if cursor_x >= hi_max_w {
            break;
        }
    }

    // Downsample 2x → 1x with box filter (average 2x2 blocks)
    let out_w = (hi_w / scale).max(1);
    let out_h = (hi_h / scale).max(1);
    let mut pixels = vec![0u8; (out_w * out_h * 4) as usize];

    for y in 0..out_h {
        for x in 0..out_w {
            let sx = x * scale;
            let sy = y * scale;

            // Average 2x2 block of alpha values
            let mut sum = 0u32;
            for dy in 0..scale {
                for dx in 0..scale {
                    let hx = (sx + dx).min(hi_w - 1);
                    let hy = (sy + dy).min(hi_h - 1);
                    sum += hi_alpha[(hy * hi_w + hx) as usize] as u32;
                }
            }
            let alpha = (sum / (scale * scale) as u32) as u8;

            if alpha > 0 {
                let offset = ((y * out_w + x) * 4) as usize;
                // ARGB8888 little-endian: [B, G, R, A]
                // Pre-multiplied alpha for clean compositing
                let a = alpha as u16;
                pixels[offset] = (color[2] as u16 * a / 255) as u8;
                pixels[offset + 1] = (color[1] as u16 * a / 255) as u8;
                pixels[offset + 2] = (color[0] as u16 * a / 255) as u8;
                pixels[offset + 3] = alpha;
            }
        }
    }

    (pixels, out_w, out_h)
}
