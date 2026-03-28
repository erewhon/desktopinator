//! RDPEGFX (Graphics Pipeline) DVC handler for H.264 streaming over RDP.
//!
//! Implements the GFX dynamic virtual channel to send H.264-encoded frames
//! to RDP clients using the AVC420 codec.

use std::sync::{Arc, Mutex};

use ironrdp_core::{encode_vec, impl_as_any};
use ironrdp_dvc::{DvcEncode, DvcMessage, DvcProcessor, DvcServerProcessor};
use ironrdp_pdu::gcc::{Monitor, MonitorFlags};
use ironrdp_pdu::geometry::InclusiveRectangle;
use ironrdp_pdu::rdp::vc::dvc::gfx::{
    self, Avc420BitmapStream, Avc444BitmapStream, CapabilitySet, Codec1Type, CreateSurfacePdu,
    EndFramePdu, Encoding, MapSurfaceToOutputPdu, PixelFormat, QuantQuality, ResetGraphicsPdu,
    StartFramePdu, Timestamp, WireToSurface1Pdu,
};
use ironrdp_pdu::PduResult;
use tracing::{debug, info, warn};

// GFX ServerPdu implements Encode but not DvcEncode — wrap it.
struct GfxPdu(Vec<u8>);

impl ironrdp_core::Encode for GfxPdu {
    fn encode(&self, dst: &mut ironrdp_core::WriteCursor<'_>) -> ironrdp_core::EncodeResult<()> {
        ironrdp_core::ensure_size!(in: dst, size: self.0.len());
        dst.write_slice(&self.0);
        Ok(())
    }

    fn name(&self) -> &'static str {
        "GfxPdu"
    }

    fn size(&self) -> usize {
        self.0.len()
    }
}

impl DvcEncode for GfxPdu {}

/// GFX channel name per MS-RDPEGFX spec.
const GFX_CHANNEL_NAME: &str = "Microsoft::Windows::RDS::Graphics";

/// Per-output surface info for GFX multi-monitor.
#[derive(Debug, Clone)]
pub struct GfxOutputInfo {
    pub name: String,
    pub surface_id: u16,
    pub x: u32,
    pub y: u32,
    pub width: u16,
    pub height: u16,
}

/// Queued GFX data to send via ServerEvent::Dvc (avoids DVC process() return path issues).
#[derive(Debug)]
pub struct GfxPendingResponse {
    pub channel_id: u32,
    pub data: Vec<u8>,
}

/// Shared state between the GFX handler (RDP async context) and the compositor (calloop).
#[derive(Debug)]
pub struct GfxSharedState {
    /// DVC channel ID, set when the channel opens.
    pub channel_id: Option<u32>,
    /// Whether the GFX channel is fully negotiated and ready for frames.
    pub ready: bool,
    /// Whether the client supports AVC420.
    pub avc_supported: bool,
    /// Whether the client supports AVC444 (GFX cap V10.4+ without AVC_DISABLED).
    pub avc444_supported: bool,
    /// Per-output surface info. Updated by the render loop when outputs change.
    pub outputs: Vec<GfxOutputInfo>,
    /// Composite dimensions (bounding box of all outputs).
    pub composite_width: u16,
    pub composite_height: u16,
    /// Next frame ID to use.
    pub next_frame_id: u32,
    /// Last frame ID acknowledged by the client.
    pub last_acked_frame_id: Option<u32>,
    /// Pending GFX response to send via ServerEvent::Dvc (set by handler, consumed by RDP task).
    pub pending_response: Option<GfxPendingResponse>,
}

impl GfxSharedState {
    pub fn new(width: u16, height: u16) -> Self {
        Self {
            channel_id: None,
            ready: false,
            avc_supported: false,
            avc444_supported: false,
            outputs: vec![GfxOutputInfo {
                name: String::new(),
                surface_id: 0,
                x: 0,
                y: 0,
                width,
                height,
            }],
            composite_width: width,
            composite_height: height,
            next_frame_id: 0,
            last_acked_frame_id: None,
            pending_response: None,
        }
    }

    /// Look up surface_id for an output by name.
    pub fn surface_id_for_output(&self, name: &str) -> Option<u16> {
        self.outputs
            .iter()
            .find(|o| o.name == name)
            .map(|o| o.surface_id)
    }
}

/// DVC processor for the RDPEGFX channel.
pub struct GfxHandler {
    state: Arc<Mutex<GfxSharedState>>,
}

impl_as_any!(GfxHandler);

impl GfxHandler {
    pub fn new(state: Arc<Mutex<GfxSharedState>>) -> Self {
        Self { state }
    }
}

impl DvcProcessor for GfxHandler {
    fn channel_name(&self) -> &str {
        GFX_CHANNEL_NAME
    }

    fn start(&mut self, channel_id: u32) -> PduResult<Vec<DvcMessage>> {
        info!(channel_id, "GFX channel opened");
        let mut state = self.state.lock().unwrap();
        // Reset state for new connection
        state.channel_id = Some(channel_id);
        state.ready = false;
        state.avc_supported = false;
        state.avc444_supported = false;
        state.next_frame_id = 0;
        state.last_acked_frame_id = None;
        state.pending_response = None;
        // Don't send anything yet — wait for client's CapabilitiesAdvertise
        Ok(Vec::new())
    }

    fn close(&mut self, channel_id: u32) {
        info!(channel_id, "GFX channel closed");
        let mut state = self.state.lock().unwrap();
        state.channel_id = None;
        state.ready = false;
    }

    fn process(&mut self, _channel_id: u32, payload: &[u8]) -> PduResult<Vec<DvcMessage>> {
        // Client GFX PDUs may or may not be ZGFX-wrapped.
        // Detect ZGFX wrapping (0xE0 = Single, 0xE1 = Multipart) and unwrap if needed.
        let gfx_data = if !payload.is_empty() && (payload[0] == 0xE0 || payload[0] == 0xE1) {
            // ZGFX-wrapped: unwrap the single segment (skip descriptor byte + flags byte)
            if payload[0] == 0xE0 && payload.len() >= 2 {
                debug!("GFX: unwrapping ZGFX single segment from client PDU");
                &payload[2..]
            } else {
                // Multipart — not expected from client, log and process raw
                warn!("GFX: unexpected ZGFX multipart from client, processing raw");
                payload
            }
        } else {
            payload
        };

        // GFX PDUs have an 8-byte header: cmdId(u16) + flags(u16) + pduLength(u32)
        if gfx_data.len() < 8 {
            warn!(len = gfx_data.len(), "GFX PDU too short");
            return Ok(Vec::new());
        }
        let cmd_id = u16::from_le_bytes([gfx_data[0], gfx_data[1]]);

        match cmd_id {
            // CYCLEALL_CAPSADVERTISE = 0x0012
            0x0012 => {
                debug!("GFX: received CapabilitiesAdvertise");
                self.handle_capabilities_advertise(gfx_data)
            }
            // CYCLEALL_FRAME_ACKNOWLEDGE = 0x000D
            0x000D => {
                if gfx_data.len() >= 16 {
                    let frame_id =
                        u32::from_le_bytes([gfx_data[8], gfx_data[9], gfx_data[10], gfx_data[11]]);
                    let total_decoded = u32::from_le_bytes([
                        gfx_data[12],
                        gfx_data[13],
                        gfx_data[14],
                        gfx_data[15],
                    ]);
                    debug!(frame_id, total_decoded, "GFX: frame acknowledged");
                    let mut state = self.state.lock().unwrap();
                    state.last_acked_frame_id = Some(frame_id);
                }
                Ok(Vec::new())
            }
            other => {
                debug!(cmd_id = other, "GFX: unhandled client PDU");
                Ok(Vec::new())
            }
        }
    }
}

impl GfxHandler {
    fn handle_capabilities_advertise(&mut self, payload: &[u8]) -> PduResult<Vec<DvcMessage>> {
        // Parse capabilities manually to handle unknown versions gracefully.
        // ironrdp-pdu's CapabilitiesAdvertisePdu::decode() hard-fails on unknown
        // capability versions, but RDP clients (e.g. Windows App on Mac) often
        // advertise newer versions we don't recognize.
        let advertised = parse_capabilities_advertise(&payload[8..]);

        // Select the best capability set from what the client advertised.
        // Preference: V8.1 with AVC420 > V10.x > V8 > fallback to V8.1
        let (selected_cap, avc_supported, avc444_supported) = select_capability(&advertised);
        info!(?selected_cap, avc_supported, avc444_supported, "GFX: selected capability");

        let state = self.state.lock().unwrap();
        let outputs = state.outputs.clone();
        let composite_width = state.composite_width;
        let composite_height = state.composite_height;
        drop(state);

        // FreeRDP two-phase approach per MS-RDPEGFX 3.2.5.2:
        // Phase 1 (synchronous): Send CapabilitiesConfirm immediately via process() return
        // Phase 2 (deferred): Send ResetGraphics + per-output CreateSurface + MapSurfaceToOutput
        //                     via ServerEvent::Dvc on the next render tick

        // Phase 1: CapabilitiesConfirm (ZGFX-wrapped per MS-RDPEGFX 2.2.2)
        let caps_confirm =
            gfx::ServerPdu::CapabilitiesConfirm(gfx::CapabilitiesConfirmPdu(selected_cap.clone()));
        let caps_raw = encode_vec(&caps_confirm)
            .map_err(|e| ironrdp_pdu::pdu_other_err!("failed to encode GFX PDU", source: e))?;
        let caps_encoded = wrap_zgfx_uncompressed(&caps_raw);
        debug!(
            raw_len = caps_raw.len(),
            zgfx_len = caps_encoded.len(),
            "GFX: encoded CapabilitiesConfirm"
        );

        // Phase 2: Build per-output monitor list and surfaces
        let monitors: Vec<Monitor> = outputs
            .iter()
            .enumerate()
            .map(|(i, o)| Monitor {
                left: o.x as i32,
                top: o.y as i32,
                right: (o.x as i32) + (o.width as i32) - 1,
                bottom: (o.y as i32) + (o.height as i32) - 1,
                flags: if i == 0 {
                    MonitorFlags::PRIMARY
                } else {
                    MonitorFlags::empty()
                },
            })
            .collect();

        let reset_graphics = gfx::ServerPdu::ResetGraphics(ResetGraphicsPdu {
            width: composite_width as u32,
            height: composite_height as u32,
            monitors,
        });

        let mut deferred_raw = Vec::new();
        let encoded = encode_vec(&reset_graphics)
            .map_err(|e| ironrdp_pdu::pdu_other_err!("failed to encode GFX PDU", source: e))?;
        deferred_raw.extend_from_slice(&encoded);

        // Create one surface per output and map it to its position
        for o in &outputs {
            let create = gfx::ServerPdu::CreateSurface(CreateSurfacePdu {
                surface_id: o.surface_id,
                width: o.width,
                height: o.height,
                pixel_format: PixelFormat::XRgb,
            });
            let map = gfx::ServerPdu::MapSurfaceToOutput(MapSurfaceToOutputPdu {
                surface_id: o.surface_id,
                output_origin_x: o.x,
                output_origin_y: o.y,
            });
            for pdu in [create, map] {
                let encoded = encode_vec(&pdu).map_err(
                    |e| ironrdp_pdu::pdu_other_err!("failed to encode GFX PDU", source: e),
                )?;
                deferred_raw.extend_from_slice(&encoded);
            }
        }

        let deferred = wrap_zgfx_uncompressed(&deferred_raw);
        debug!(
            raw_len = deferred_raw.len(),
            zgfx_len = deferred.len(),
            outputs = outputs.len(),
            "GFX: queued deferred PDUs (ResetGraphics + {} surfaces)",
            outputs.len()
        );

        let mut state = self.state.lock().unwrap();
        state.pending_response = Some(GfxPendingResponse {
            channel_id: state.channel_id.unwrap_or(2),
            data: deferred,
        });
        state.avc_supported = avc_supported;
        state.avc444_supported = avc444_supported;
        info!(
            composite_w = composite_width,
            composite_h = composite_height,
            outputs = outputs.len(),
            avc_supported,
            avc444_supported,
            "GFX: caps confirmed, multi-surface setup deferred"
        );

        // Return CapabilitiesConfirm via DVC process() return path
        Ok(vec![Box::new(GfxPdu(caps_encoded))])
    }
}

/// Parse CapabilitiesAdvertise manually, skipping unknown capability versions.
///
/// The ironrdp-pdu decoder hard-fails on unknown versions, but RDP clients
/// commonly advertise newer versions. Each capability set has:
///   version: u32, dataLength: u32, data: [u8; dataLength]
fn parse_capabilities_advertise(data: &[u8]) -> Vec<CapabilitySet> {
    if data.len() < 2 {
        warn!("GFX: CapabilitiesAdvertise too short");
        return Vec::new();
    }

    let count = u16::from_le_bytes([data[0], data[1]]) as usize;
    let mut offset = 2;
    let mut caps = Vec::new();

    // Known capability version constants (from MS-RDPEGFX)
    const V8: u32 = 0x8_0004;
    const V8_1: u32 = 0x8_0105;
    const V10: u32 = 0xa_0002;
    const V10_1: u32 = 0xa_0100;
    const V10_2: u32 = 0xa_0200;
    const V10_3: u32 = 0xa_0301;
    const V10_4: u32 = 0xa_0400;
    const V10_5: u32 = 0xa_0502;
    const V10_6: u32 = 0xa_0600;
    const V10_6_ERR: u32 = 0xa_0601;
    const V10_7: u32 = 0xa_0701;

    for i in 0..count {
        if offset + 8 > data.len() {
            warn!(cap_index = i, "GFX: CapabilitiesAdvertise truncated");
            break;
        }
        let version = u32::from_le_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]);
        let data_length = u32::from_le_bytes([
            data[offset + 4],
            data[offset + 5],
            data[offset + 6],
            data[offset + 7],
        ]) as usize;
        offset += 8;

        if offset + data_length > data.len() {
            warn!(
                cap_index = i,
                version = format!("0x{version:08x}"),
                data_length,
                "GFX: capability data truncated"
            );
            break;
        }

        let cap_data = &data[offset..offset + data_length];
        offset += data_length;

        // Read flags (u32) from capability data, default to 0
        let flags_u32 = if cap_data.len() >= 4 {
            u32::from_le_bytes([cap_data[0], cap_data[1], cap_data[2], cap_data[3]])
        } else {
            0
        };

        let cap = match version {
            V8 => CapabilitySet::V8 {
                flags: gfx::CapabilitiesV8Flags::from_bits_truncate(flags_u32),
            },
            V8_1 => CapabilitySet::V8_1 {
                flags: gfx::CapabilitiesV81Flags::from_bits_truncate(flags_u32),
            },
            V10 => CapabilitySet::V10 {
                flags: gfx::CapabilitiesV10Flags::from_bits_truncate(flags_u32),
            },
            V10_2 => CapabilitySet::V10_2 {
                flags: gfx::CapabilitiesV10Flags::from_bits_truncate(flags_u32),
            },
            V10_3 => CapabilitySet::V10_3 {
                flags: gfx::CapabilitiesV103Flags::from_bits_truncate(flags_u32),
            },
            V10_4 => CapabilitySet::V10_4 {
                flags: gfx::CapabilitiesV104Flags::from_bits_truncate(flags_u32),
            },
            V10_5 => CapabilitySet::V10_5 {
                flags: gfx::CapabilitiesV104Flags::from_bits_truncate(flags_u32),
            },
            V10_6 | V10_6_ERR => CapabilitySet::V10_6 {
                flags: gfx::CapabilitiesV104Flags::from_bits_truncate(flags_u32),
            },
            V10_7 => CapabilitySet::V10_7 {
                flags: gfx::CapabilitiesV107Flags::from_bits_truncate(flags_u32),
            },
            V10_1 => {
                // V10_1 has 16 bytes of data but no flags we use
                debug!(
                    version = format!("0x{version:08x}"),
                    "GFX: parsed V10_1 capability"
                );
                CapabilitySet::V10_1
            }
            _ => {
                info!(
                    version = format!("0x{version:08x}"),
                    data_length, "GFX: skipping unknown capability version"
                );
                continue;
            }
        };

        debug!(?cap, "GFX: parsed client capability");
        caps.push(cap);
    }

    info!(
        total = count,
        parsed = caps.len(),
        "GFX: parsed client capabilities"
    );
    caps
}

/// Select the best capability set from the client's advertised list.
/// Returns (selected_cap, avc_supported, avc444_supported).
///
/// Per FreeRDP's approach, prefer the HIGHEST version the client advertises.
/// V10.4+ support AVC420/AVC444 implicitly (unless AVC_DISABLED flag is set).
fn select_capability(advertised: &[CapabilitySet]) -> (CapabilitySet, bool, bool) {
    // Assign a priority to each version (higher = better)
    fn version_priority(cap: &CapabilitySet) -> u32 {
        match cap {
            CapabilitySet::V8 { .. } => 1,
            CapabilitySet::V8_1 { .. } => 2,
            CapabilitySet::V10 { .. } => 10,
            CapabilitySet::V10_1 => 11,
            CapabilitySet::V10_2 { .. } => 12,
            CapabilitySet::V10_3 { .. } => 13,
            CapabilitySet::V10_4 { .. } => 14,
            CapabilitySet::V10_5 { .. } => 15,
            CapabilitySet::V10_6 { .. } | CapabilitySet::V10_6Err { .. } => 16,
            CapabilitySet::V10_7 { .. } => 17,
            CapabilitySet::Unknown(_) => 0,
        }
    }

    fn is_avc_supported(cap: &CapabilitySet) -> bool {
        match cap {
            CapabilitySet::V8_1 { flags } => {
                flags.contains(gfx::CapabilitiesV81Flags::AVC420_ENABLED)
            }
            // V10.4+ support AVC unless AVC_DISABLED flag is set
            CapabilitySet::V10_4 { flags }
            | CapabilitySet::V10_5 { flags }
            | CapabilitySet::V10_6 { flags } => {
                !flags.contains(gfx::CapabilitiesV104Flags::AVC_DISABLED)
            }
            CapabilitySet::V10_7 { flags } => {
                !flags.contains(gfx::CapabilitiesV107Flags::AVC_DISABLED)
            }
            CapabilitySet::V10 { flags } | CapabilitySet::V10_2 { flags } => {
                !flags.contains(gfx::CapabilitiesV10Flags::AVC_DISABLED)
            }
            CapabilitySet::V10_3 { flags } => {
                !flags.contains(gfx::CapabilitiesV103Flags::AVC_DISABLED)
            }
            _ => false,
        }
    }

    // Select the highest-priority capability
    let best = advertised.iter().max_by_key(|cap| version_priority(cap));

    fn is_avc444_supported(cap: &CapabilitySet) -> bool {
        // AVC444 requires V10.4+ with AVC enabled
        match cap {
            CapabilitySet::V10_4 { flags }
            | CapabilitySet::V10_5 { flags }
            | CapabilitySet::V10_6 { flags } => {
                !flags.contains(gfx::CapabilitiesV104Flags::AVC_DISABLED)
            }
            CapabilitySet::V10_7 { flags } => {
                !flags.contains(gfx::CapabilitiesV107Flags::AVC_DISABLED)
            }
            _ => false,
        }
    }

    if let Some(cap) = best {
        let avc = is_avc_supported(cap);
        let avc444 = is_avc444_supported(cap);
        (cap.clone(), avc, avc444)
    } else {
        (
            CapabilitySet::V8 {
                flags: gfx::CapabilitiesV8Flags::empty(),
            },
            false,
            false,
        )
    }
}

impl DvcServerProcessor for GfxHandler {}

/// Wrap raw GFX PDU bytes in ZGFX (RDP8_SEGMENTED_DATA) uncompressed single-segment format.
///
/// Per MS-RDPEGFX 2.2.2, all GFX PDUs sent from server to client over the DVC channel MUST be
/// wrapped in RDP_SEGMENTED_DATA. For uncompressed data this is just 2 bytes of overhead:
///   - 0xE0: SegmentedDescriptor::Single
///   - 0x04: CompressionType = RDP8 (0x4), no compression flags
fn wrap_zgfx_uncompressed(gfx_pdu: &[u8]) -> Vec<u8> {
    let mut wrapped = Vec::with_capacity(2 + gfx_pdu.len());
    wrapped.push(0xE0); // RDP_SEGMENTED_DATA descriptor: SINGLE
    wrapped.push(0x04); // RDP8_BULK_ENCODED_DATA: CompressionType=RDP8, not compressed
    wrapped.extend_from_slice(gfx_pdu);
    wrapped
}

/// Build GFX PDUs to reset surfaces for a multi-monitor layout.
/// Deletes old surfaces, sends ResetGraphics with monitor list,
/// creates new per-output surfaces, and maps them to their positions.
pub fn build_reset_surface_pdus(
    old_outputs: &[GfxOutputInfo],
    new_outputs: &[GfxOutputInfo],
    composite_width: u16,
    composite_height: u16,
) -> anyhow::Result<Vec<u8>> {
    use ironrdp_pdu::rdp::vc::dvc::gfx::DeleteSurfacePdu;

    let mut raw = Vec::new();

    // Delete all old surfaces
    for o in old_outputs {
        let delete = gfx::ServerPdu::DeleteSurface(DeleteSurfacePdu {
            surface_id: o.surface_id,
        });
        let encoded =
            encode_vec(&delete).map_err(|e| anyhow::anyhow!("failed to encode GFX PDU: {e}"))?;
        raw.extend_from_slice(&encoded);
    }

    // ResetGraphics with monitor list
    let monitors: Vec<Monitor> = new_outputs
        .iter()
        .enumerate()
        .map(|(i, o)| Monitor {
            left: o.x as i32,
            top: o.y as i32,
            right: (o.x as i32) + (o.width as i32) - 1,
            bottom: (o.y as i32) + (o.height as i32) - 1,
            flags: if i == 0 {
                MonitorFlags::PRIMARY
            } else {
                MonitorFlags::empty()
            },
        })
        .collect();

    let reset = gfx::ServerPdu::ResetGraphics(ResetGraphicsPdu {
        width: composite_width as u32,
        height: composite_height as u32,
        monitors,
    });
    let encoded =
        encode_vec(&reset).map_err(|e| anyhow::anyhow!("failed to encode GFX PDU: {e}"))?;
    raw.extend_from_slice(&encoded);

    // Create and map each new surface
    for o in new_outputs {
        let create = gfx::ServerPdu::CreateSurface(CreateSurfacePdu {
            surface_id: o.surface_id,
            width: o.width,
            height: o.height,
            pixel_format: PixelFormat::XRgb,
        });
        let map = gfx::ServerPdu::MapSurfaceToOutput(MapSurfaceToOutputPdu {
            surface_id: o.surface_id,
            output_origin_x: o.x,
            output_origin_y: o.y,
        });
        for pdu in [create, map] {
            let encoded =
                encode_vec(&pdu).map_err(|e| anyhow::anyhow!("failed to encode GFX PDU: {e}"))?;
            raw.extend_from_slice(&encoded);
        }
    }

    Ok(wrap_zgfx_uncompressed(&raw))
}

/// Encode an H.264 frame as GFX PDU bytes ready to send via ServerEvent::Dvc.
///
/// Returns the concatenated bytes of StartFrame + WireToSurface1(AVC420) + EndFrame.
pub fn encode_gfx_avc420_frame(
    h264_data: &[u8],
    surface_id: u16,
    width: u16,
    height: u16,
    frame_id: u32,
) -> anyhow::Result<Vec<u8>> {
    encode_gfx_avc420_frame_at(h264_data, surface_id, 0, 0, width, height, frame_id)
}

/// Encode a single H.264 frame as a GFX PDU, placed at (dest_x, dest_y) on the surface.
pub fn encode_gfx_avc420_frame_at(
    h264_data: &[u8],
    surface_id: u16,
    dest_x: u16,
    dest_y: u16,
    width: u16,
    height: u16,
    frame_id: u32,
) -> anyhow::Result<Vec<u8>> {
    // StartFrame
    let start_frame = gfx::ServerPdu::StartFrame(StartFramePdu {
        timestamp: Timestamp {
            milliseconds: 0,
            seconds: 0,
            minutes: 0,
            hours: 0,
        },
        frame_id,
    });

    // Build AVC420 bitmap stream — rectangles are relative to the H.264 frame
    let avc_stream = Avc420BitmapStream {
        rectangles: vec![InclusiveRectangle {
            left: 0,
            top: 0,
            right: width.saturating_sub(1),
            bottom: height.saturating_sub(1),
        }],
        quant_qual_vals: vec![QuantQuality {
            quantization_parameter: 22,
            progressive: false,
            quality: 100,
        }],
        data: h264_data,
    };

    // Encode AVC420 stream into bitmap_data for WireToSurface1
    let avc_bytes = encode_vec(&avc_stream)
        .map_err(|e| anyhow::anyhow!("failed to encode AVC420 stream: {e}"))?;

    // destination_rectangle is where on the surface this frame goes
    let wire_to_surface = gfx::ServerPdu::WireToSurface1(WireToSurface1Pdu {
        surface_id,
        codec_id: Codec1Type::Avc420,
        pixel_format: PixelFormat::XRgb,
        destination_rectangle: InclusiveRectangle {
            left: dest_x,
            top: dest_y,
            right: (dest_x + width).saturating_sub(1),
            bottom: (dest_y + height).saturating_sub(1),
        },
        bitmap_data: avc_bytes,
    });

    // EndFrame
    let end_frame = gfx::ServerPdu::EndFrame(EndFramePdu { frame_id });

    // Encode all three PDUs, concatenate, then ZGFX-wrap
    let mut raw = Vec::new();
    for pdu in [start_frame, wire_to_surface, end_frame] {
        let encoded =
            encode_vec(&pdu).map_err(|e| anyhow::anyhow!("failed to encode GFX PDU: {e}"))?;
        raw.extend_from_slice(&encoded);
    }

    Ok(wrap_zgfx_uncompressed(&raw))
}

/// Encode multiple tile frames into a single GFX StartFrame/EndFrame.
/// Each tile becomes a WireToSurface1Pdu with its own destination_rectangle.
pub fn encode_gfx_avc420_tiles(
    tiles: &[crate::adaptive::TileFrame],
    surface_id: u16,
    frame_id: u32,
) -> anyhow::Result<Vec<u8>> {
    if tiles.is_empty() {
        return Ok(Vec::new());
    }

    let mut raw = Vec::new();

    let start_frame = gfx::ServerPdu::StartFrame(StartFramePdu {
        timestamp: Timestamp {
            milliseconds: 0,
            seconds: 0,
            minutes: 0,
            hours: 0,
        },
        frame_id,
    });
    let start_bytes =
        encode_vec(&start_frame).map_err(|e| anyhow::anyhow!("start frame: {e}"))?;
    raw.extend_from_slice(&start_bytes);

    for tile in tiles {
        let avc_stream = Avc420BitmapStream {
            rectangles: vec![InclusiveRectangle {
                left: 0,
                top: 0,
                right: tile.width.saturating_sub(1),
                bottom: tile.height.saturating_sub(1),
            }],
            quant_qual_vals: vec![QuantQuality {
                quantization_parameter: 22,
                progressive: false,
                quality: 100,
            }],
            data: &tile.data,
        };

        let avc_bytes = encode_vec(&avc_stream)
            .map_err(|e| anyhow::anyhow!("AVC420 tile: {e}"))?;

        let wire = gfx::ServerPdu::WireToSurface1(WireToSurface1Pdu {
            surface_id,
            codec_id: Codec1Type::Avc420,
            pixel_format: PixelFormat::XRgb,
            destination_rectangle: InclusiveRectangle {
                left: tile.x,
                top: tile.y,
                right: (tile.x + tile.width).saturating_sub(1),
                bottom: (tile.y + tile.height).saturating_sub(1),
            },
            bitmap_data: avc_bytes,
        });

        let wire_bytes =
            encode_vec(&wire).map_err(|e| anyhow::anyhow!("wire to surface: {e}"))?;
        raw.extend_from_slice(&wire_bytes);
    }

    let end_frame = gfx::ServerPdu::EndFrame(EndFramePdu { frame_id });
    let end_bytes = encode_vec(&end_frame).map_err(|e| anyhow::anyhow!("end frame: {e}"))?;
    raw.extend_from_slice(&end_bytes);

    Ok(wrap_zgfx_uncompressed(&raw))
}

/// Encode a GFX frame using AVC444 codec (dual-stream or luma-only).
///
/// - `luma_h264`: H.264 NAL data for stream 1 (standard YUV420 encode)
/// - `chroma_h264`: Optional H.264 NAL data for stream 2 (chroma residuals)
///   If None, sends luma-only (LC=1).
pub fn encode_gfx_avc444_frame(
    luma_h264: &[u8],
    chroma_h264: Option<&[u8]>,
    surface_id: u16,
    width: u16,
    height: u16,
    frame_id: u32,
) -> anyhow::Result<Vec<u8>> {
    let start_frame = gfx::ServerPdu::StartFrame(StartFramePdu {
        timestamp: Timestamp {
            milliseconds: 0,
            seconds: 0,
            minutes: 0,
            hours: 0,
        },
        frame_id,
    });

    let rect = InclusiveRectangle {
        left: 0,
        top: 0,
        right: width.saturating_sub(1),
        bottom: height.saturating_sub(1),
    };
    let qq = QuantQuality {
        quantization_parameter: 22,
        progressive: false,
        quality: 100,
    };

    let mk_rect = || InclusiveRectangle {
        left: 0,
        top: 0,
        right: width.saturating_sub(1),
        bottom: height.saturating_sub(1),
    };
    let mk_qq = || QuantQuality {
        quantization_parameter: 22,
        progressive: false,
        quality: 100,
    };

    // Manually construct the AVC444 wire format to ensure correctness.
    // Format: [4-byte header][RFX_AVC420_BITMAP_STREAM 1][RFX_AVC420_BITMAP_STREAM 2]
    // Header bits [29:0] = cbAvc420EncodedBitstream1, bits [31:30] = LC
    let stream1 = Avc420BitmapStream {
        rectangles: vec![mk_rect()],
        quant_qual_vals: vec![mk_qq()],
        data: luma_h264,
    };
    let stream1_bytes = encode_vec(&stream1)
        .map_err(|e| anyhow::anyhow!("failed to encode AVC420 stream1: {e}"))?;

    let avc_bytes = if let Some(chroma_data) = chroma_h264 {
        let stream2 = Avc420BitmapStream {
            rectangles: vec![mk_rect()],
            quant_qual_vals: vec![mk_qq()],
            data: chroma_data,
        };
        let stream2_bytes = encode_vec(&stream2)
            .map_err(|e| anyhow::anyhow!("failed to encode AVC420 stream2: {e}"))?;

        // LC=0 (LUMA_AND_CHROMA): both streams present
        let lc: u32 = 0;
        let header: u32 = (stream1_bytes.len() as u32 & 0x3FFFFFFF) | (lc << 30);

        info!(
            lc = 0,
            header_hex = format!("{:08x}", header),
            stream1_size = stream1_bytes.len(),
            stream2_size = stream2_bytes.len(),
            stream1_first16 = format!("{:02x?}", &stream1_bytes[..16.min(stream1_bytes.len())]),
            stream2_first16 = format!("{:02x?}", &stream2_bytes[..16.min(stream2_bytes.len())]),
            "AVC444 LC=0 payload"
        );

        let mut buf = Vec::with_capacity(4 + stream1_bytes.len() + stream2_bytes.len());
        buf.extend_from_slice(&header.to_le_bytes());
        buf.extend_from_slice(&stream1_bytes);
        buf.extend_from_slice(&stream2_bytes);
        buf
    } else {
        // LC=1 (LUMA only): just stream1
        let lc: u32 = 1;
        let header: u32 = (stream1_bytes.len() as u32 & 0x3FFFFFFF) | (lc << 30);

        let mut buf = Vec::with_capacity(4 + stream1_bytes.len());
        buf.extend_from_slice(&header.to_le_bytes());
        buf.extend_from_slice(&stream1_bytes);
        buf
    };

    // Always use AVC444 (0x000E) — AVC444v2 (0x000F) may not be supported by all clients
    let codec_id = Codec1Type::Avc444;

    let wire_to_surface = gfx::ServerPdu::WireToSurface1(WireToSurface1Pdu {
        surface_id,
        codec_id,
        pixel_format: PixelFormat::XRgb,
        destination_rectangle: InclusiveRectangle {
            left: 0,
            top: 0,
            right: width.saturating_sub(1),
            bottom: height.saturating_sub(1),
        },
        bitmap_data: avc_bytes,
    });

    let end_frame = gfx::ServerPdu::EndFrame(EndFramePdu { frame_id });

    let mut raw = Vec::new();
    for pdu in [start_frame, wire_to_surface, end_frame] {
        let encoded =
            encode_vec(&pdu).map_err(|e| anyhow::anyhow!("failed to encode GFX PDU: {e}"))?;
        raw.extend_from_slice(&encoded);
    }

    Ok(wrap_zgfx_uncompressed(&raw))
}

/// Send raw pre-built AVC444 payload bytes in a WireToSurface1 PDU.
pub fn encode_gfx_avc444_raw(
    avc444_payload: &[u8],
    surface_id: u16,
    width: u16,
    height: u16,
    frame_id: u32,
) -> anyhow::Result<Vec<u8>> {
    let start_frame = gfx::ServerPdu::StartFrame(StartFramePdu {
        timestamp: Timestamp { milliseconds: 0, seconds: 0, minutes: 0, hours: 0 },
        frame_id,
    });

    let wire_to_surface = gfx::ServerPdu::WireToSurface1(WireToSurface1Pdu {
        surface_id,
        codec_id: Codec1Type::Avc444,
        pixel_format: PixelFormat::XRgb,
        destination_rectangle: InclusiveRectangle {
            left: 0,
            top: 0,
            right: width.saturating_sub(1),
            bottom: height.saturating_sub(1),
        },
        bitmap_data: avc444_payload.to_vec(),
    });

    let end_frame = gfx::ServerPdu::EndFrame(EndFramePdu { frame_id });

    let mut raw = Vec::new();
    for pdu in [start_frame, wire_to_surface, end_frame] {
        let encoded =
            encode_vec(&pdu).map_err(|e| anyhow::anyhow!("failed to encode GFX PDU: {e}"))?;
        raw.extend_from_slice(&encoded);
    }

    Ok(wrap_zgfx_uncompressed(&raw))
}
