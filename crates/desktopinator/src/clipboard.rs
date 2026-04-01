//! Clipboard sync between Wayland (data_device) and RDP (CLIPRDR).
//!
//! - Wayland app copies → RDP client can paste
//! - RDP client copies → Wayland app can paste

use std::sync::{Arc, Mutex};

use ironrdp_cliprdr::backend::{
    ClipboardMessage, ClipboardMessageProxy, CliprdrBackend, CliprdrBackendFactory,
};
use ironrdp_cliprdr::pdu::{
    ClipboardFormat, ClipboardFormatId, ClipboardGeneralCapabilityFlags, FileContentsRequest,
    FileContentsResponse, FormatDataRequest, FormatDataResponse, LockDataId,
};
use ironrdp_core::impl_as_any;
use ironrdp_server::ServerEvent;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Standard RDP clipboard format IDs.
const CF_UNICODETEXT: ClipboardFormatId = ClipboardFormatId(13);
const CF_DIB: ClipboardFormatId = ClipboardFormatId(8);

/// Shared clipboard state between the RDP backend and the compositor.
#[derive(Debug, Default)]
pub struct ClipboardState {
    /// Current Wayland clipboard text (UTF-8), ready to serve to RDP client.
    pub wayland_text: Option<String>,
    /// Current RDP clipboard text (UTF-8, converted from UTF-16LE).
    pub rdp_text: Option<String>,
    /// Current Wayland clipboard image (PNG bytes).
    pub wayland_image: Option<Vec<u8>>,
    /// Current RDP clipboard image (PNG bytes, converted from CF_DIB).
    pub rdp_image: Option<Vec<u8>>,
    /// Whether the RDP client currently owns the clipboard.
    pub rdp_owns_clipboard: bool,
}

/// CLIPRDR backend that bridges RDP clipboard ↔ shared state.
#[derive(Debug)]
pub struct DinatorCliprdrBackend {
    state: Arc<Mutex<ClipboardState>>,
    proxy: Option<Box<dyn ClipboardMessageProxy>>,
}

impl_as_any!(DinatorCliprdrBackend);

impl DinatorCliprdrBackend {
    pub fn new(state: Arc<Mutex<ClipboardState>>) -> Self {
        Self { state, proxy: None }
    }

    fn send_msg(&self, msg: ClipboardMessage) {
        if let Some(ref proxy) = self.proxy {
            proxy.send_clipboard_message(msg);
        }
    }
}

impl CliprdrBackend for DinatorCliprdrBackend {
    fn temporary_directory(&self) -> &str {
        "/tmp"
    }

    fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
        ClipboardGeneralCapabilityFlags::USE_LONG_FORMAT_NAMES
    }

    fn on_ready(&mut self) {
        info!("CLIPRDR: clipboard channel ready");
        // Advertise available formats
        let state = self.state.lock().unwrap();
        let mut formats = Vec::new();
        if state.wayland_text.is_some() {
            formats.push(ClipboardFormat { id: CF_UNICODETEXT, name: None });
        }
        if state.wayland_image.is_some() {
            formats.push(ClipboardFormat { id: CF_DIB, name: None });
        }
        drop(state);
        if !formats.is_empty() {
            self.send_msg(ClipboardMessage::SendInitiateCopy(formats));
        }
    }

    fn on_request_format_list(&mut self) {
        debug!("CLIPRDR: client requested format list");
    }

    fn on_process_negotiated_capabilities(
        &mut self,
        capabilities: ClipboardGeneralCapabilityFlags,
    ) {
        debug!(?capabilities, "CLIPRDR: negotiated capabilities");
    }

    fn on_remote_copy(&mut self, available_formats: &[ClipboardFormat]) {
        let has_text = available_formats
            .iter()
            .any(|f| f.id == CF_UNICODETEXT || f.id == ClipboardFormatId(1));
        let has_image = available_formats
            .iter()
            .any(|f| f.id == CF_DIB || f.id == ClipboardFormatId(17)); // CF_DIB or CF_DIBV5

        self.state.lock().unwrap().rdp_owns_clipboard = true;

        // Request text first, then image
        if has_text {
            info!("CLIPRDR: RDP client has text, requesting");
            self.send_msg(ClipboardMessage::SendInitiatePaste(CF_UNICODETEXT));
        }
        if has_image {
            info!("CLIPRDR: RDP client has image (CF_DIB), requesting");
            self.send_msg(ClipboardMessage::SendInitiatePaste(CF_DIB));
        }
        if !has_text && !has_image {
            debug!(formats = ?available_formats, "CLIPRDR: no supported formats");
        }
    }

    fn on_format_data_request(&mut self, request: FormatDataRequest) {
        // RDP client wants to paste from our clipboard
        debug!(format = ?request.format, "CLIPRDR: RDP client requesting clipboard data");

        if request.format == CF_UNICODETEXT {
            let state = self.state.lock().unwrap();
            if let Some(text) = state.wayland_text.clone() {
                drop(state);
                let utf16: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
                let bytes: Vec<u8> = utf16.iter().flat_map(|c| c.to_le_bytes()).collect();
                info!(text_len = text.len(), "CLIPRDR: sending text to RDP client");
                self.send_msg(ClipboardMessage::SendFormatData(FormatDataResponse::new_data(bytes)));
            } else {
                self.send_msg(ClipboardMessage::SendFormatData(FormatDataResponse::new_error()));
            }
        } else if request.format == CF_DIB {
            let state = self.state.lock().unwrap();
            if let Some(png_data) = state.wayland_image.clone() {
                drop(state);
                match png_to_dib(&png_data) {
                    Ok(dib) => {
                        info!(dib_bytes = dib.len(), "CLIPRDR: sending image (CF_DIB) to RDP client");
                        self.send_msg(ClipboardMessage::SendFormatData(FormatDataResponse::new_data(dib)));
                    }
                    Err(e) => {
                        warn!(error = %e, "CLIPRDR: failed to convert PNG to DIB");
                        self.send_msg(ClipboardMessage::SendFormatData(FormatDataResponse::new_error()));
                    }
                }
            } else {
                self.send_msg(ClipboardMessage::SendFormatData(FormatDataResponse::new_error()));
            }
        } else {
            debug!(format = ?request.format, "CLIPRDR: unsupported format requested");
            self.send_msg(ClipboardMessage::SendFormatData(FormatDataResponse::new_error()));
        }
    }

    fn on_format_data_response(&mut self, response: FormatDataResponse<'_>) {
        if response.is_error() {
            warn!("CLIPRDR: RDP client returned error for format data request");
            return;
        }

        let data = response.data();

        // Try to detect if this is text (UTF-16LE) or image (DIB)
        // DIB starts with BITMAPINFOHEADER (first 4 bytes = header size, typically 40 or 124)
        let is_dib = data.len() >= 4 && {
            let header_size = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
            header_size == 40 || header_size == 56 || header_size == 108 || header_size == 124
        };

        if is_dib {
            info!(dib_bytes = data.len(), "CLIPRDR: received image (CF_DIB) from RDP client");
            match dib_to_png(data) {
                Ok(png) => {
                    let mut state = self.state.lock().unwrap();
                    state.rdp_image = Some(png);
                    state.rdp_owns_clipboard = true;
                }
                Err(e) => {
                    warn!(error = %e, "CLIPRDR: failed to convert DIB to PNG");
                }
            }
        } else if data.len() >= 2 {
            // Assume UTF-16LE text
            let utf16: Vec<u16> = data
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            let text = String::from_utf16_lossy(&utf16)
                .trim_end_matches('\0')
                .to_string();
            info!(text_len = text.len(), "CLIPRDR: received text from RDP client");
            let mut state = self.state.lock().unwrap();
            state.rdp_text = Some(text);
            state.rdp_owns_clipboard = true;
        }
    }

    fn on_file_contents_request(&mut self, _request: FileContentsRequest) {
        debug!("CLIPRDR: file contents request (not supported)");
    }

    fn on_file_contents_response(&mut self, _response: FileContentsResponse<'_>) {
        debug!("CLIPRDR: file contents response (not supported)");
    }

    fn on_lock(&mut self, _data_id: LockDataId) {}
    fn on_unlock(&mut self, _data_id: LockDataId) {}
}

/// Factory that creates CLIPRDR backends per connection.
#[derive(Debug)]
pub struct DinatorCliprdrFactory {
    state: Arc<Mutex<ClipboardState>>,
    event_tx: Option<mpsc::UnboundedSender<ServerEvent>>,
}

impl DinatorCliprdrFactory {
    pub fn new(state: Arc<Mutex<ClipboardState>>) -> Self {
        Self {
            state,
            event_tx: None,
        }
    }
}

impl CliprdrBackendFactory for DinatorCliprdrFactory {
    fn build_cliprdr_backend(&self) -> Box<dyn CliprdrBackend> {
        Box::new(DinatorCliprdrBackend::new(self.state.clone()))
    }
}

/// Convert PNG image data to CF_DIB (BITMAPINFOHEADER + BGR pixel rows, bottom-up).
fn png_to_dib(png_data: &[u8]) -> anyhow::Result<Vec<u8>> {
    use image::io::Reader as ImageReader;
    use std::io::Cursor;

    let img = ImageReader::new(Cursor::new(png_data))
        .with_guessed_format()?
        .decode()?;
    let rgba = img.to_rgba8();
    let w = rgba.width();
    let h = rgba.height();

    // BITMAPINFOHEADER (40 bytes)
    let header_size: u32 = 40;
    let bpp: u16 = 32; // BGRA
    let row_size = (w * 4) as usize; // no padding needed for 32-bit
    let image_size = row_size * h as usize;

    let mut dib = Vec::with_capacity(header_size as usize + image_size);

    // BITMAPINFOHEADER
    dib.extend_from_slice(&header_size.to_le_bytes());     // biSize
    dib.extend_from_slice(&(w as i32).to_le_bytes());      // biWidth
    dib.extend_from_slice(&(h as i32).to_le_bytes());      // biHeight (positive = bottom-up)
    dib.extend_from_slice(&1u16.to_le_bytes());             // biPlanes
    dib.extend_from_slice(&bpp.to_le_bytes());              // biBitCount
    dib.extend_from_slice(&0u32.to_le_bytes());             // biCompression = BI_RGB
    dib.extend_from_slice(&(image_size as u32).to_le_bytes()); // biSizeImage
    dib.extend_from_slice(&0u32.to_le_bytes());             // biXPelsPerMeter
    dib.extend_from_slice(&0u32.to_le_bytes());             // biYPelsPerMeter
    dib.extend_from_slice(&0u32.to_le_bytes());             // biClrUsed
    dib.extend_from_slice(&0u32.to_le_bytes());             // biClrImportant

    // Pixel data: bottom-up, BGRA
    for y in (0..h).rev() {
        for x in 0..w {
            let p = rgba.get_pixel(x, y);
            dib.push(p[2]); // B
            dib.push(p[1]); // G
            dib.push(p[0]); // R
            dib.push(p[3]); // A
        }
    }

    Ok(dib)
}

/// Convert CF_DIB (BITMAPINFOHEADER + pixel data) to PNG.
fn dib_to_png(dib_data: &[u8]) -> anyhow::Result<Vec<u8>> {
    use image::{ImageBuffer, RgbaImage};
    use std::io::Cursor;

    if dib_data.len() < 40 {
        anyhow::bail!("DIB data too short");
    }

    let header_size = u32::from_le_bytes([dib_data[0], dib_data[1], dib_data[2], dib_data[3]]);
    let w = i32::from_le_bytes([dib_data[4], dib_data[5], dib_data[6], dib_data[7]]) as u32;
    let h_signed = i32::from_le_bytes([dib_data[8], dib_data[9], dib_data[10], dib_data[11]]);
    let bottom_up = h_signed > 0;
    let h = h_signed.unsigned_abs();
    let bpp = u16::from_le_bytes([dib_data[14], dib_data[15]]);

    if bpp != 32 && bpp != 24 {
        anyhow::bail!("unsupported DIB bpp: {bpp}");
    }

    let pixel_offset = header_size as usize;
    let pixel_data = &dib_data[pixel_offset..];
    let bytes_per_pixel = (bpp / 8) as usize;
    let row_size = ((w as usize * bytes_per_pixel + 3) / 4) * 4; // padded to 4 bytes

    let mut img: RgbaImage = ImageBuffer::new(w, h);

    for y in 0..h {
        let src_y = if bottom_up { h - 1 - y } else { y };
        let row_start = src_y as usize * row_size;

        for x in 0..w {
            let px_start = row_start + x as usize * bytes_per_pixel;
            if px_start + bytes_per_pixel > pixel_data.len() {
                break;
            }
            let b = pixel_data[px_start];
            let g = pixel_data[px_start + 1];
            let r = pixel_data[px_start + 2];
            let a = if bpp == 32 { pixel_data[px_start + 3] } else { 255 };
            img.put_pixel(x, y, image::Rgba([r, g, b, a]));
        }
    }

    let mut png_buf = Cursor::new(Vec::new());
    img.write_to(&mut png_buf, image::ImageFormat::Png)?;
    Ok(png_buf.into_inner())
}

impl ironrdp_server::ServerEventSender for DinatorCliprdrFactory {
    fn set_sender(&mut self, sender: mpsc::UnboundedSender<ServerEvent>) {
        self.event_tx = Some(sender);
    }
}

impl ironrdp_server::CliprdrServerFactory for DinatorCliprdrFactory {}
