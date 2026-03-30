//! DRM/KMS backend for running desktopinator directly on hardware.
//!
//! Uses libseat for session management, udev for GPU/monitor discovery,
//! libinput for keyboard/mouse, and DRM/GBM for direct GPU output.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use anyhow::Context;
use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
use smithay::backend::allocator::Fourcc as DrmFourcc;
use smithay::backend::drm::{
    DrmDevice, DrmDeviceFd, DrmEvent, DrmNode, GbmBufferedSurface, NodeType,
};
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::libinput::{LibinputInputBackend, LibinputSessionInterface};
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::{Bind, ImportDma};
use smithay::backend::session::libseat::LibSeatSession;
use smithay::backend::session::{Event as SessionEvent, Session};
use smithay::backend::udev::{UdevBackend, UdevEvent};
use smithay::desktop::layer_map_for_output;
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::{EventLoop, Interest, PostAction};
use smithay::reexports::drm::control::{connector, crtc, Device as CtrlDevice, ModeTypeFlags};
use smithay::reexports::input::Libinput;
use smithay::reexports::rustix::fs::OFlags;
use smithay::reexports::wayland_server::{Display, ListeningSocket};
use smithay::utils::DeviceFd;
use tracing::info;

use dinator_core::DinatorState;

use crate::config;

/// Combined state for the DRM backend event loop.
struct DrmBackendData {
    state: DinatorState,
    renderer: Option<GlesRenderer>,
    drm_outputs: HashMap<crtc::Handle, DrmOutputState>,
    /// CRTCs that have a pending page flip (awaiting VBlank).
    pending_flips: std::collections::HashSet<crtc::Handle>,
}

/// Per-output rendering state for DRM.
struct DrmOutputState {
    output: Output,
    crtc: crtc::Handle,
    surface: GbmBufferedSurface<GbmAllocator<DrmDeviceFd>, ()>,
    damage_tracker: OutputDamageTracker,
}

pub fn run_drm(cfg: &config::Config) -> anyhow::Result<()> {
    info!("starting desktopinator (DRM/KMS backend)");

    let mut event_loop: EventLoop<DrmBackendData> = EventLoop::try_new()?;
    let display: Display<DinatorState> = Display::new()?;
    let display_handle = display.handle();

    // Open a libseat session
    let (mut session, notifier) =
        LibSeatSession::new().context("failed to create libseat session")?;
    info!(seat = %session.seat(), "libseat session opened");

    // Wayland socket
    let listening_socket = ListeningSocket::bind_auto("wayland", 0..33)?;
    let socket_name = listening_socket
        .socket_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    info!(socket = %socket_name, "wayland socket listening");
    std::env::set_var("WAYLAND_DISPLAY", &socket_name);

    // Compositor state
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

    // Build the combined backend data
    let mut data = DrmBackendData {
        state,
        renderer: None,
        drm_outputs: HashMap::new(),
        pending_flips: std::collections::HashSet::new(),
    };

    // Accept Wayland clients
    event_loop
        .handle()
        .insert_source(
            Generic::new(listening_socket, Interest::READ, smithay::reexports::calloop::Mode::Level),
            |_, socket, data| {
                if let Some(stream) = socket.accept()? {
                    let client_state = std::sync::Arc::new(dinator_core::ClientState::default());
                    if let Err(e) = data.state.display_handle.insert_client(stream, client_state) {
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
        .insert_source(notifier, |event, _, _data| match event {
            SessionEvent::PauseSession => {
                info!("session paused (VT switch away)");
                // TODO: drop DRM master, pause rendering
            }
            SessionEvent::ActivateSession => {
                info!("session resumed (VT switch back)");
                // TODO: re-acquire DRM master, force full redraw
            }
        })
        .map_err(|e| anyhow::anyhow!("failed to insert session source: {e}"))?;

    // Udev — discover GPUs
    let udev_backend = UdevBackend::new(&session.seat())
        .context("failed to create udev backend")?;

    for (dev_id, path) in udev_backend.device_list() {
        if let Err(e) = init_gpu(
            dev_id, &path, &mut session, &mut data, &display_handle, &mut event_loop,
        ) {
            tracing::warn!(path = %path.display(), error = %e, "failed to init GPU");
        }
    }

    info!(
        outputs = data.drm_outputs.len(),
        has_renderer = data.renderer.is_some(),
        "GPU initialization complete"
    );

    // GPU hotplug
    event_loop
        .handle()
        .insert_source(udev_backend, |event, _, _data| match event {
            UdevEvent::Added { device_id: _, path } => {
                info!(path = %path.display(), "GPU added (hotplug not yet supported)");
            }
            UdevEvent::Changed { device_id: _ } => {
                info!("GPU changed (connector hotplug — rescan not yet supported)");
            }
            UdevEvent::Removed { device_id: _ } => {
                info!("GPU removed");
            }
        })
        .map_err(|e| anyhow::anyhow!("failed to insert udev source: {e}"))?;

    // Libinput
    let mut libinput = Libinput::new_with_udev(LibinputSessionInterface::from(session.clone()));
    libinput
        .udev_assign_seat(&session.seat())
        .map_err(|_| anyhow::anyhow!("failed to assign libinput seat"))?;

    event_loop
        .handle()
        .insert_source(LibinputInputBackend::new(libinput), |event, _, data| {
            data.state.handle_input_event(event);
        })
        .map_err(|e| anyhow::anyhow!("failed to insert libinput source: {e}"))?;

    // TODO: XWayland (spawn_xwayland expects LoopHandle<DinatorState>, need adapter)
    info!("XWayland not yet supported in DRM backend");

    // Render timer — drives frame rendering for all outputs
    let timer = Timer::immediate();
    event_loop
        .handle()
        .insert_source(timer, |_, _, data| {
            render_all(data);
            TimeoutAction::ToDuration(Duration::from_micros(1_000_000 / 60))
        })
        .map_err(|e| anyhow::anyhow!("failed to insert timer: {e}"))?;

    info!("entering event loop (DRM) -- launch clients with WAYLAND_DISPLAY={socket_name}");

    // Main event loop
    event_loop
        .run(Duration::from_micros(1_000_000 / 60), &mut data, |data| {
            let display_ptr = &mut data.state.display as *mut Display<DinatorState>;
            if let Err(e) = unsafe { &mut *display_ptr }.dispatch_clients(&mut data.state) {
                tracing::error!("dispatch_clients error: {e}");
            }
            if let Err(e) = data.state.display.flush_clients() {
                tracing::error!("flush_clients error: {e}");
            }
        })
        .map_err(|e| anyhow::anyhow!("event loop error: {e}"))?;

    Ok(())
}

/// Render all outputs and send frame callbacks.
fn render_all(data: &mut DrmBackendData) {
    let Some(ref mut renderer) = data.renderer else { return };

    let crtcs: Vec<crtc::Handle> = data.drm_outputs.keys().copied().collect();
    for crtc in crtcs {
        // Skip outputs with pending page flips
        if data.pending_flips.contains(&crtc) {
            continue;
        }

        let drm_out = data.drm_outputs.get_mut(&crtc).unwrap();
        if render_output(renderer, drm_out, &mut data.state) {
            data.pending_flips.insert(crtc);
        }
    }

    // Send frame callbacks
    let elapsed = data.state.start_time.elapsed();
    for output in data.state.space.outputs().cloned().collect::<Vec<_>>() {
        data.state.space.elements().for_each(|window| {
            window.send_frame(&output, elapsed, None, |_, _| Some(output.clone()));
        });
        let layer_map = layer_map_for_output(&output);
        for layer in layer_map.layers() {
            layer.send_frame(&output, elapsed, None, |_, _| Some(output.clone()));
        }
    }

    data.state.space.refresh();
}

/// Render a single output. Returns true if a buffer was queued (page flip pending).
fn render_output(
    renderer: &mut GlesRenderer,
    drm_out: &mut DrmOutputState,
    state: &mut DinatorState,
) -> bool {
    let elements = match crate::build_render_elements(renderer, state, &drm_out.output) {
        Some(e) => e,
        None => return false,
    };

    let bg = crate::background_clear_color(state.background_for_output(&drm_out.output));

    // Get next buffer from swapchain
    let (mut dmabuf, _age) = match drm_out.surface.next_buffer() {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(output = %drm_out.output.name(), error = ?e, "next_buffer failed");
            return false;
        }
    };

    // Bind → render → queue
    let mut target = match renderer.bind(&mut dmabuf) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(output = %drm_out.output.name(), error = ?e, "bind failed");
            return false;
        }
    };

    let render_ok = drm_out
        .damage_tracker
        .render_output(renderer, &mut target, 0, &elements, bg)
        .is_ok();

    drop(target);

    if render_ok {
        if let Err(e) = drm_out.surface.queue_buffer(None, None, ()) {
            tracing::warn!(output = %drm_out.output.name(), error = ?e, "queue_buffer failed");
            return false;
        }
        return true;
    }

    false
}

type dev_t = smithay::reexports::rustix::fs::Dev;

fn init_gpu(
    dev_id: dev_t,
    path: &Path,
    session: &mut LibSeatSession,
    data: &mut DrmBackendData,
    display_handle: &smithay::reexports::wayland_server::DisplayHandle,
    event_loop: &mut EventLoop<DrmBackendData>,
) -> anyhow::Result<()> {
    let node = match DrmNode::from_dev_id(dev_id) {
        Ok(n) => n,
        Err(_) => return Ok(()),
    };
    let render_node = node
        .node_with_type(NodeType::Render)
        .and_then(|n| n.ok())
        .unwrap_or(node);

    info!(path = %path.display(), node = %node, render = %render_node, "initializing GPU");

    let fd = session
        .open(path, OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY)
        .context("failed to open DRM device")?;
    let drm_fd = DrmDeviceFd::new(DeviceFd::from(fd));

    let (mut drm, drm_notifier) =
        DrmDevice::new(drm_fd.clone(), true).context("failed to create DRM device")?;

    let gbm = GbmDevice::new(drm_fd.clone()).context("failed to create GBM device")?;

    // Create renderer from this GPU (first GPU only)
    if data.renderer.is_none() {
        let egl_display =
            unsafe { EGLDisplay::new(gbm.clone()) }.context("failed to create EGL display")?;
        let egl_context =
            EGLContext::new(&egl_display).context("failed to create EGL context")?;
        let gles =
            unsafe { GlesRenderer::new(egl_context) }.context("failed to create GLES renderer")?;
        info!("EGL/GLES renderer created from GPU {}", path.display());
        data.renderer = Some(gles);
    }

    // Register DRM notifier for VBlank events
    event_loop
        .handle()
        .insert_source(drm_notifier, |event, _, data| match event {
            DrmEvent::VBlank(crtc) => {
                // Page flip complete — release buffer back to swapchain
                if let Some(drm_out) = data.drm_outputs.get_mut(&crtc) {
                    if let Err(e) = drm_out.surface.frame_submitted() {
                        tracing::warn!(?crtc, error = ?e, "frame_submitted failed");
                    }
                }
                data.pending_flips.remove(&crtc);
            }
            DrmEvent::Error(e) => {
                tracing::error!(error = ?e, "DRM error");
            }
        })
        .map_err(|e| anyhow::anyhow!("failed to insert DRM notifier: {e}"))?;

    // Scan connectors
    let res = drm.resource_handles().context("failed to get DRM resources")?;

    for conn in res.connectors() {
        let conn_info = match drm.get_connector(*conn, false) {
            Ok(info) => info,
            Err(_) => continue,
        };
        if conn_info.state() != connector::State::Connected {
            continue;
        }

        let encoder = conn_info
            .current_encoder()
            .and_then(|e| drm.get_encoder(e).ok());
        let crtc_handle = encoder
            .as_ref()
            .and_then(|e| e.crtc())
            .or_else(|| {
                conn_info.encoders().iter().find_map(|e| {
                    drm.get_encoder(*e).ok().and_then(|_| {
                        res.crtcs()
                            .iter()
                            .find(|&&c| !data.drm_outputs.contains_key(&c))
                            .copied()
                    })
                })
            });

        let Some(crtc) = crtc_handle else {
            tracing::warn!("no CRTC for connector {:?}", conn_info.interface());
            continue;
        };

        let drm_mode = conn_info
            .modes()
            .iter()
            .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
            .or_else(|| conn_info.modes().first())
            .copied();

        let Some(drm_mode) = drm_mode else {
            tracing::warn!("no modes for connector {:?}", conn_info.interface());
            continue;
        };

        let (w, h) = (drm_mode.size().0 as i32, drm_mode.size().1 as i32);
        let refresh = drm_mode.vrefresh() as i32 * 1000;

        // Create DRM surface + GBM buffered surface
        let drm_surface = drm
            .create_surface(crtc, drm_mode, &[*conn])
            .context("failed to create DRM surface")?;

        let allocator =
            GbmAllocator::new(gbm.clone(), GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT);

        let gbm_surface = GbmBufferedSurface::new(
            drm_surface,
            allocator,
            &[DrmFourcc::Argb8888, DrmFourcc::Xrgb8888],
            data.renderer.as_ref().unwrap().dmabuf_formats(),
        )
        .context("failed to create GBM buffered surface")?;

        // Smithay Output
        let connector_name = format!(
            "{}-{}",
            conn_info.interface().as_str(),
            conn_info.interface_id()
        );
        let physical_size = (
            conn_info.size().unwrap_or((0, 0)).0 as i32,
            conn_info.size().unwrap_or((0, 0)).1 as i32,
        );

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

        let x_offset: i32 = data
            .state
            .space
            .outputs()
            .filter_map(|o| data.state.space.output_geometry(o))
            .map(|g| g.loc.x + g.size.w)
            .max()
            .unwrap_or(0);

        data.state.space.map_output(&output, (x_offset, 0));
        data.state.register_output(&output);

        let damage_tracker = OutputDamageTracker::from_output(&output);

        info!(
            name = %connector_name,
            mode = format!("{}x{}@{}Hz", w, h, drm_mode.vrefresh()),
            "DRM output created"
        );

        data.drm_outputs.insert(
            crtc,
            DrmOutputState {
                output,
                crtc,
                surface: gbm_surface,
                damage_tracker,
            },
        );
    }

    Ok(())
}
