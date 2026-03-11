//! Custom DisplayControl DVC handler (MS-RDPEDISP) with multi-monitor support.
//!
//! The built-in ironrdp-displaycontrol handler only advertises max 1 monitor.
//! This handler advertises up to 16 monitors and forwards layout changes to the
//! compositor event loop via shared state.

use std::sync::{Arc, Mutex};

use ironrdp_core::{decode, impl_as_any};
use ironrdp_displaycontrol::pdu::{DisplayControlCapabilities, DisplayControlPdu};
use ironrdp_dvc::{DvcMessage, DvcProcessor, DvcServerProcessor};
use ironrdp_pdu::{decode_err, PduResult};
use tracing::{debug, info};

const CHANNEL_NAME: &str = "Microsoft::Windows::RDS::DisplayControl";

/// A monitor entry received from the RDP client's DisplayControl channel.
#[derive(Debug, Clone)]
pub struct MonitorEntry {
    pub is_primary: bool,
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
}

/// Shared state for passing DisplayControl monitor layout from the RDP async
/// context to the compositor's synchronous event loop.
#[derive(Debug, Default)]
pub struct DisplayControlState {
    /// Pending monitor layout from the client, consumed by the render loop.
    pub pending_layout: Option<Vec<MonitorEntry>>,
}

/// Custom DVC handler for DisplayControl that supports multi-monitor.
pub struct DisplayControlDvc {
    state: Arc<Mutex<DisplayControlState>>,
}

impl DisplayControlDvc {
    pub fn new(state: Arc<Mutex<DisplayControlState>>) -> Self {
        Self { state }
    }
}

impl_as_any!(DisplayControlDvc);

impl DvcProcessor for DisplayControlDvc {
    fn channel_name(&self) -> &str {
        CHANNEL_NAME
    }

    fn start(&mut self, _channel_id: u32) -> PduResult<Vec<DvcMessage>> {
        // Advertise support for up to 16 monitors, 3840x2400 max area factor
        let pdu: DisplayControlPdu = DisplayControlCapabilities::new(16, 3840, 2400)
            .map_err(|e| decode_err!(e))?
            .into();

        info!("DisplayControl: advertising max 16 monitors");
        Ok(vec![Box::new(pdu)])
    }

    fn process(&mut self, _channel_id: u32, payload: &[u8]) -> PduResult<Vec<DvcMessage>> {
        match decode(payload).map_err(|e| decode_err!(e))? {
            DisplayControlPdu::MonitorLayout(layout) => {
                let monitors: Vec<MonitorEntry> = layout
                    .monitors()
                    .iter()
                    .map(|m| {
                        let (w, h) = m.dimensions();
                        let (left, top) = m.position().unwrap_or((0, 0));
                        MonitorEntry {
                            is_primary: m.is_primary(),
                            left,
                            top,
                            width: w,
                            height: h,
                        }
                    })
                    .collect();

                info!(
                    count = monitors.len(),
                    ?monitors,
                    "DisplayControl: client requested monitor layout"
                );

                if let Ok(mut state) = self.state.lock() {
                    state.pending_layout = Some(monitors);
                }
            }
            DisplayControlPdu::Caps(caps) => {
                debug!(?caps, "DisplayControl: received client caps");
            }
        }
        Ok(Vec::new())
    }
}

impl DvcServerProcessor for DisplayControlDvc {}
