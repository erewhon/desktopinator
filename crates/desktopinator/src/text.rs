use fontdue::{Font, FontSettings};
use std::sync::OnceLock;

static FONT: OnceLock<Font> = OnceLock::new();

const FONT_BYTES: &[u8] = include_bytes!("../assets/Inter.ttf");

fn font() -> &'static Font {
    FONT.get_or_init(|| {
        Font::from_bytes(FONT_BYTES, FontSettings::default()).expect("failed to load embedded font")
    })
}

/// Render text into an ARGB8888 pixel buffer.
/// Returns the buffer as Vec<u8> with dimensions (width, height).
pub fn render_text(
    text: &str,
    font_size: f32,
    color: [u8; 3],
    max_width: i32,
    height: i32,
) -> (Vec<u8>, i32, i32) {
    let font = font();
    let px_size = font_size;

    // Rasterize each glyph and compute total width
    let mut glyphs: Vec<(fontdue::Metrics, Vec<u8>)> = Vec::new();
    let mut total_width: i32 = 0;

    for ch in text.chars() {
        let (metrics, bitmap) = font.rasterize(ch, px_size);
        total_width += metrics.advance_width as i32;
        glyphs.push((metrics, bitmap));
    }

    let buf_w = total_width.min(max_width).max(1);
    let buf_h = height.max(1);
    let mut pixels = vec![0u8; (buf_w * buf_h * 4) as usize];

    // Baseline: place text vertically centered
    let metrics = font.horizontal_line_metrics(px_size);
    let baseline = if let Some(m) = metrics {
        let ascent = m.ascent;
        let descent = m.descent;
        let text_h = ascent - descent;
        ((buf_h as f32 - text_h) / 2.0 + ascent) as i32
    } else {
        buf_h * 3 / 4
    };

    let mut cursor_x: i32 = 0;

    for (metrics, bitmap) in &glyphs {
        let gx = cursor_x + metrics.xmin;
        let gy = baseline - metrics.height as i32 - metrics.ymin;

        for row in 0..metrics.height {
            for col in 0..metrics.width {
                let px = gx + col as i32;
                let py = gy + row as i32;
                if px < 0 || px >= buf_w || py < 0 || py >= buf_h {
                    continue;
                }
                let alpha = bitmap[row * metrics.width + col];
                if alpha == 0 {
                    continue;
                }
                let offset = ((py * buf_w + px) * 4) as usize;
                // ARGB8888: [B, G, R, A] in little-endian memory
                let existing_a = pixels[offset + 3] as u16;
                let new_a = alpha as u16;
                let out_a = new_a + existing_a * (255 - new_a) / 255;
                if out_a > 0 {
                    pixels[offset] = ((color[2] as u16 * new_a + pixels[offset] as u16 * existing_a * (255 - new_a) / 255) / out_a) as u8;
                    pixels[offset + 1] = ((color[1] as u16 * new_a + pixels[offset + 1] as u16 * existing_a * (255 - new_a) / 255) / out_a) as u8;
                    pixels[offset + 2] = ((color[0] as u16 * new_a + pixels[offset + 2] as u16 * existing_a * (255 - new_a) / 255) / out_a) as u8;
                    pixels[offset + 3] = out_a as u8;
                }
            }
        }

        cursor_x += metrics.advance_width as i32;
        if cursor_x >= max_width {
            break;
        }
    }

    (pixels, buf_w, buf_h)
}
