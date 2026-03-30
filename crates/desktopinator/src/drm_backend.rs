//! DRM/KMS backend for running desktopinator directly on hardware.
//!
//! Uses libseat for session management, udev for GPU/monitor discovery,
//! libinput for keyboard/mouse, and DRM/GBM for direct GPU output.

use std::collections::HashMap;
use std::os::fd::FromRawFd;
use std::path::Path;

use anyhow::Context;
use smithay::backend::allocator::gbm::GbmDevice;
use smithay::backend::drm::{DrmDevice, DrmDeviceFd, DrmNode, NodeType};
use smithay::backend::libinput::{LibinputInputBackend, LibinputSessionInterface};
use smithay::backend::session::libseat::LibSeatSession;
use smithay::backend::session::{Session, Event as SessionEvent};
use smithay::backend::udev::{UdevBackend, UdevEvent};
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{EventLoop, Interest, PostAction};
use smithay::reexports::drm::control::{self, connector, crtc, Device as CtrlDevice, ModeTypeFlags};
use smithay::reexports::input::Libinput;
use smithay::reexports::rustix::fs::OFlags;
use smithay::reexports::wayland_server::{Display, ListeningSocket};
use smithay::utils::DeviceFd;
use tracing::info;

use dinator_core::DinatorState;

use crate::config;

/// Per-GPU state.
struct GpuState {
    drm: DrmDevice,
    gbm: GbmDevice<DrmDeviceFd>,
    outputs: HashMap<crtc::Handle, DrmOutput>,
    render_node: DrmNode,
}

/// Per-monitor (CRTC) output state.
struct DrmOutput {
    output: Output,
    _crtc: crtc::Handle,
}

pub fn run_drm(cfg: &config::Config) -> anyhow::Result<()> {
    info!("starting desktopinator (DRM/KMS backend)");

    let mut event_loop: EventLoop<DinatorState> = EventLoop::try_new()?;
    let display: Display<DinatorState> = Display::new()?;
    let display_handle = display.handle();

    // Open a libseat session for privilege management
    let (mut session, notifier) =
        LibSeatSession::new().context("failed to create libseat session")?;
    info!(seat = %session.seat(), "libseat session opened");

    // Wayland socket
    let listening_socket = ListeningSocket::bind_auto("wayland", 0..33)?;
    let socket_name = listening_socket.socket_name().unwrap().to_string_lossy().to_string();
    info!(socket = %socket_name, "wayland socket listening");
    std::env::set_var("WAYLAND_DISPLAY", &socket_name);

    // Create compositor state
    let mut state = DinatorState::new(display, event_loop.get_signal());
    let plugin_keybindings = crate::init_plugins(&mut state);
    state.plugin_keybindings = plugin_keybindings
        .into_iter()
        .map(|kb| (kb.keysym, kb.mods.0, kb.mods.1, kb.mods.2, kb.mods.3, kb.callback_id))
        .collect();

    // Apply config
    if let Some(ref bg_spec) = cfg.background {
        if let Some(bg) = dinator_core::parse_background(bg_spec) {
            state.set_background(bg);
        }
    }
    if let Some(gap) = cfg.gap {
        state.set_layout_gap(gap);
    }
    if let Some(ref layout_name) = cfg.layout {
        state.set_layout(layout_name);
    }

    state.seat.add_keyboard(Default::default(), 200, 25)?;
    state.seat.add_pointer();

    // Accept Wayland clients
    event_loop
        .handle()
        .insert_source(
            Generic::new(listening_socket, Interest::READ, smithay::reexports::calloop::Mode::Level),
            |_, socket, state| {
                if let Some(stream) = socket.accept()? {
                    let client_state = std::sync::Arc::new(dinator_core::ClientState::default());
                    if let Err(e) = state.display_handle.insert_client(stream, client_state) {
                        tracing::error!("failed to insert client: {e}");
                    }
                }
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| anyhow::anyhow!("failed to insert socket source: {e}"))?;

    // Session events (VT switching)
    event_loop
        .handle()
        .insert_source(notifier, |event, _, _state| {
            match event {
                SessionEvent::PauseSession => {
                    info!("session paused (VT switch away)");
                }
                SessionEvent::ActivateSession => {
                    info!("session resumed (VT switch back)");
                }
            }
        })
        .map_err(|e| anyhow::anyhow!("failed to insert session source: {e}"))?;

    // Udev backend — discover GPUs
    let udev_backend = UdevBackend::new(&session.seat())
        .context("failed to create udev backend")?;

    let mut gpu_states: HashMap<dev_t, GpuState> = HashMap::new();

    // Process initial GPU list
    for (dev_id, path) in udev_backend.device_list() {
        if let Err(e) = init_gpu(dev_id, &path, &mut session, &mut state, &mut gpu_states, &display_handle) {
            tracing::warn!(path = %path.display(), error = %e, "failed to init GPU");
        }
    }

    // Listen for GPU hotplug
    event_loop
        .handle()
        .insert_source(udev_backend, move |event, _, _state| {
            match event {
                UdevEvent::Added { device_id, path } => {
                    info!(path = %path.display(), "GPU added");
                }
                UdevEvent::Changed { device_id } => {
                    info!("GPU changed (connector hotplug)");
                }
                UdevEvent::Removed { device_id } => {
                    info!("GPU removed");
                }
            }
        })
        .map_err(|e| anyhow::anyhow!("failed to insert udev source: {e}"))?;

    // Libinput — keyboard/mouse/touchpad
    let mut libinput = Libinput::new_with_udev(LibinputSessionInterface::from(session.clone()));
    libinput
        .udev_assign_seat(&session.seat())
        .map_err(|_| anyhow::anyhow!("failed to assign libinput seat"))?;

    let libinput_backend = LibinputInputBackend::new(libinput.clone());
    event_loop
        .handle()
        .insert_source(libinput_backend, |event, _, state| {
            state.handle_input_event(event);
        })
        .map_err(|e| anyhow::anyhow!("failed to insert libinput source: {e}"))?;

    // TODO: IPC server (needs handle_ipc_command extracted from main.rs)

    // XWayland
    if let Err(e) = crate::spawn_xwayland(&event_loop.handle(), &display_handle) {
        tracing::warn!(error = %e, "XWayland not available");
    }

    info!(
        "entering event loop (DRM) -- launch clients with WAYLAND_DISPLAY={socket_name}"
    );

    // Main event loop
    let frame_interval = std::time::Duration::from_micros(1_000_000 / 60);
    event_loop
        .run(frame_interval, &mut state, |state| {
            let display_ptr = &mut state.display as *mut Display<DinatorState>;
            if let Err(e) = unsafe { &mut *display_ptr }.dispatch_clients(state) {
                tracing::error!("dispatch_clients error: {e}");
            }
            if let Err(e) = state.display.flush_clients() {
                tracing::error!("flush_clients error: {e}");
            }
            state.space.refresh();
        })
        .map_err(|e| anyhow::anyhow!("event loop error: {e}"))?;

    Ok(())
}

type dev_t = smithay::reexports::rustix::fs::Dev;

fn init_gpu(
    dev_id: dev_t,
    path: &Path,
    session: &mut LibSeatSession,
    state: &mut DinatorState,
    gpu_states: &mut HashMap<dev_t, GpuState>,
    display_handle: &smithay::reexports::wayland_server::DisplayHandle,
) -> anyhow::Result<()> {
    // Only process DRM devices
    let node = match DrmNode::from_dev_id(dev_id) {
        Ok(n) => n,
        Err(e) => {
            tracing::debug!(error = %e, "not a DRM device, skipping");
            return Ok(());
        }
    };
    let render_node = node
        .node_with_type(NodeType::Render)
        .and_then(|n| n.ok())
        .unwrap_or(node);

    info!(
        path = %path.display(),
        node = %node,
        render = %render_node,
        "initializing GPU"
    );

    // Open the DRM device via the session (for privilege escalation)
    let fd = session
        .open(path, OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY)
        .context("failed to open DRM device")?;
    let drm_fd = DrmDeviceFd::new(DeviceFd::from(fd));

    let (drm, drm_notifier) = DrmDevice::new(drm_fd.clone(), true)
        .context("failed to create DRM device")?;

    let gbm = GbmDevice::new(drm_fd)
        .context("failed to create GBM device")?;

    // Scan connectors and create outputs
    let mut outputs = HashMap::new();
    let res = drm.resource_handles().context("failed to get DRM resources")?;

    for conn in res.connectors() {
        let conn_info = match drm.get_connector(*conn, false) {
            Ok(info) => info,
            Err(_) => continue,
        };
        if conn_info.state() != connector::State::Connected {
            continue;
        }

        // Find a CRTC for this connector
        let encoder = conn_info
            .current_encoder()
            .and_then(|e| drm.get_encoder(e).ok());
        let crtc_handle = encoder
            .as_ref()
            .and_then(|e| e.crtc())
            .or_else(|| {
                // Try to find a free CRTC
                conn_info.encoders().iter().find_map(|e| {
                    drm.get_encoder(*e).ok().and_then(|enc| {
                        res.crtcs().iter().find(|&&c| {
                            !outputs.contains_key(&c)
                        }).copied()
                    })
                })
            });

        let Some(crtc) = crtc_handle else {
            tracing::warn!("no CRTC available for connector {:?}", conn_info.interface());
            continue;
        };

        // Pick the preferred mode
        let mode = conn_info
            .modes()
            .iter()
            .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
            .or_else(|| conn_info.modes().first())
            .copied();

        let Some(mode) = mode else {
            tracing::warn!("no modes for connector {:?}", conn_info.interface());
            continue;
        };

        let (w, h) = (mode.size().0 as i32, mode.size().1 as i32);
        let refresh = mode.vrefresh() as i32 * 1000; // mHz

        // Create Smithay output
        let connector_name = format!(
            "{}-{}",
            conn_info.interface().as_str(),
            conn_info.interface_id()
        );

        let physical_size = (conn_info.size().unwrap_or((0, 0)).0 as i32,
                            conn_info.size().unwrap_or((0, 0)).1 as i32);

        let output = Output::new(
            connector_name.clone(),
            PhysicalProperties {
                size: physical_size.into(),
                subpixel: Subpixel::Unknown,
                make: "desktopinator".into(),
                model: "drm".into(),
            },
        );

        let smithay_mode = Mode {
            size: (w, h).into(),
            refresh,
        };
        output.change_current_state(Some(smithay_mode), None, None, None);
        output.set_preferred(smithay_mode);
        output.create_global::<DinatorState>(display_handle);

        // Position outputs side by side
        let x_offset: i32 = state.space.outputs()
            .filter_map(|o| state.space.output_geometry(o))
            .map(|g| g.loc.x + g.size.w)
            .max()
            .unwrap_or(0);

        state.space.map_output(&output, (x_offset, 0));
        state.register_output(&output);

        info!(
            name = %connector_name,
            mode = format!("{}x{}@{}Hz", w, h, mode.vrefresh()),
            "DRM output created"
        );

        outputs.insert(crtc, DrmOutput {
            output,
            _crtc: crtc,
        });
    }

    gpu_states.insert(dev_id, GpuState {
        drm,
        gbm,
        outputs,
        render_node,
    });

    Ok(())
}
