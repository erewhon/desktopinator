//! Clipboard sync between Wayland (data_device) and RDP (CLIPRDR).
//!
//! - Wayland app copies → RDP client can paste
//! - RDP client copies → Wayland app can paste

use std::sync::{Arc, Mutex};

use ironrdp_cliprdr::backend::{
    CliprdrBackend, CliprdrBackendFactory, ClipboardMessage, ClipboardMessageProxy,
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

/// Shared clipboard state between the RDP backend and the compositor.
#[derive(Debug, Default)]
pub struct ClipboardState {
    /// Current Wayland clipboard text (UTF-8), ready to serve to RDP client.
    pub wayland_text: Option<String>,
    /// Current RDP clipboard text (UTF-8, converted from UTF-16LE).
    pub rdp_text: Option<String>,
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
        // If we already have Wayland clipboard text, advertise it
        let state = self.state.lock().unwrap();
        if state.wayland_text.is_some() {
            drop(state);
            self.send_msg(ClipboardMessage::SendInitiateCopy(vec![ClipboardFormat {
                id: CF_UNICODETEXT,
                name: None,
            }]));
        }
    }

    fn on_request_format_list(&mut self) {
        debug!("CLIPRDR: client requested format list");
    }

    fn on_process_negotiated_capabilities(&mut self, capabilities: ClipboardGeneralCapabilityFlags) {
        debug!(?capabilities, "CLIPRDR: negotiated capabilities");
    }

    fn on_remote_copy(&mut self, available_formats: &[ClipboardFormat]) {
        // RDP client copied something — check if text is available
        let has_text = available_formats
            .iter()
            .any(|f| f.id == CF_UNICODETEXT || f.id == ClipboardFormatId(1));

        if has_text {
            info!("CLIPRDR: RDP client has text on clipboard, requesting paste");
            self.state.lock().unwrap().rdp_owns_clipboard = true;
            // Request the text data immediately
            self.send_msg(ClipboardMessage::SendInitiatePaste(CF_UNICODETEXT));
        } else {
            debug!(
                formats = ?available_formats,
                "CLIPRDR: RDP client clipboard has no text formats"
            );
        }
    }

    fn on_format_data_request(&mut self, request: FormatDataRequest) {
        // RDP client wants to paste from our clipboard
        debug!(format = ?request.format, "CLIPRDR: RDP client requesting clipboard data");

        if request.format == CF_UNICODETEXT {
            let state = self.state.lock().unwrap();
            if let Some(text) = state.wayland_text.clone() {
                drop(state);
                // Convert UTF-8 → UTF-16LE, null-terminated
                let utf16: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
                let bytes: Vec<u8> = utf16.iter().flat_map(|c| c.to_le_bytes()).collect();

                info!(
                    text_len = text.len(),
                    wire_bytes = bytes.len(),
                    "CLIPRDR: sending text to RDP client"
                );
                let response = FormatDataResponse::new_data(bytes);
                self.send_msg(ClipboardMessage::SendFormatData(response));
            } else {
                warn!("CLIPRDR: no Wayland text available for paste");
                let response = FormatDataResponse::new_error();
                self.send_msg(ClipboardMessage::SendFormatData(response));
            }
        } else {
            debug!(format = ?request.format, "CLIPRDR: unsupported format requested");
            let response = FormatDataResponse::new_error();
            self.send_msg(ClipboardMessage::SendFormatData(response));
        }
    }

    fn on_format_data_response(&mut self, response: FormatDataResponse<'_>) {
        // RDP client sent us clipboard data (we requested via SendInitiatePaste)
        if response.is_error() {
            warn!("CLIPRDR: RDP client returned error for format data request");
            return;
        }

        let data = response.data();

        // Decode UTF-16LE → UTF-8
        if data.len() >= 2 {
            let utf16: Vec<u16> = data
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            // Strip null terminator
            let text = String::from_utf16_lossy(&utf16)
                .trim_end_matches('\0')
                .to_string();

            info!(
                text_len = text.len(),
                wire_bytes = data.len(),
                "CLIPRDR: received text from RDP client"
            );
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

impl ironrdp_server::ServerEventSender for DinatorCliprdrFactory {
    fn set_sender(&mut self, sender: mpsc::UnboundedSender<ServerEvent>) {
        self.event_tx = Some(sender);
    }
}

impl ironrdp_server::CliprdrServerFactory for DinatorCliprdrFactory {}
