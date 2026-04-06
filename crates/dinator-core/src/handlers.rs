use std::os::unix::io::OwnedFd;

use smithay::backend::renderer::utils::on_commit_buffer_handler;
use smithay::delegate_compositor;
use smithay::delegate_data_device;
use smithay::delegate_output;
use smithay::delegate_seat;
use smithay::delegate_shm;
use smithay::delegate_xdg_shell;
use smithay::desktop::{layer_map_for_output, Window, WindowSurfaceType};
use smithay::input::pointer::CursorImageStatus;
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::output::Output;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::protocol::wl_buffer;
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::protocol::wl_seat::WlSeat;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Serial, SERIAL_COUNTER};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{CompositorClientState, CompositorHandler, CompositorState};
use smithay::wayland::output::OutputHandler;
use smithay::wayland::selection::data_device::{
    self, ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
};
use smithay::wayland::selection::{SelectionHandler, SelectionSource, SelectionTarget};
use smithay::wayland::shell::xdg::decoration::XdgDecorationHandler;
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
};
use smithay::wayland::shm::{ShmHandler, ShmState};

use tracing::info;

use smithay::wayland::compositor;
use smithay::wayland::shell::xdg::XdgToplevelSurfaceData;

use dinator_ipc::IpcEvent;

use crate::state::{ClientState, DinatorState};

// --- Compositor ---

impl CompositorHandler for DinatorState {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(
        &self,
        client: &'a smithay::reexports::wayland_server::Client,
    ) -> &'a CompositorClientState {
        if let Some(state) = client.get_data::<ClientState>() {
            return &state.compositor_state;
        }
        // XWayland client uses XWaylandClientData instead of ClientState
        if let Some(state) = client.get_data::<smithay::xwayland::XWaylandClientData>() {
            return &state.compositor_state;
        }
        panic!("client has no compositor state");
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);

        // Handle layer surface commits — check all outputs
        for output in self.space.outputs().cloned().collect::<Vec<_>>() {
            let mut layer_map = layer_map_for_output(&output);
            let layer = layer_map
                .layer_for_surface(surface, WindowSurfaceType::ALL)
                .cloned();
            if let Some(layer) = layer {
                layer_map.arrange();
                drop(layer_map);

                // Send initial configure if not yet sent
                let wlr_surface = layer.layer_surface();
                if wlr_surface.has_pending_changes() {
                    wlr_surface.send_pending_configure();
                }

                // Give keyboard focus to layer surfaces with exclusive interactivity
                let keyboard_interactivity = compositor::with_states(surface, |states| {
                    states
                        .cached_state
                        .get::<LayerSurfaceCachedState>()
                        .current()
                        .keyboard_interactivity
                });
                if keyboard_interactivity == KeyboardInteractivity::Exclusive {
                    let keyboard = self.seat.get_keyboard().unwrap();
                    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                    keyboard.set_focus(self, Some(layer.wl_surface().clone()), serial);
                }
                return;
            }
            drop(layer_map);
        }

        let found = self
            .space
            .elements()
            .find(|w| w.toplevel().is_some_and(|t| t.wl_surface() == surface))
            .cloned();

        if let Some(window) = found {
            window.on_commit();

            if let Some(toplevel) = window.toplevel() {
                // Check if this toplevel just got a parent set — auto-float it
                let has_parent = compositor::with_states(toplevel.wl_surface(), |states| {
                    states
                        .data_map
                        .get::<XdgToplevelSurfaceData>()
                        .map(|d| d.lock().unwrap().parent.is_some())
                        .unwrap_or(false)
                });

                // Track commits per window and auto-float windows that have a
                // parent set, or that still have no app_id after several commits
                // (notification popups, Firefox alerts, etc.)
                let should_auto_float = if let Some(&id) = self.surface_to_id.get(surface) {
                    let count = self.window_commits.entry(id).or_insert(0);
                    *count += 1;

                    if self.floating.contains(&id) {
                        false
                    } else if has_parent {
                        true
                    } else if *count == 3 {
                        // After 3 commits, check if app_id is still missing
                        let app_id = compositor::with_states(toplevel.wl_surface(), |states| {
                            states
                                .data_map
                                .get::<XdgToplevelSurfaceData>()
                                .and_then(|d| d.lock().unwrap().app_id.clone())
                        });
                        if app_id.is_none() {
                            tracing::info!(id = id.0, commits = *count, "window has no app_id after 3 commits");
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                } else {
                    false
                };

                if should_auto_float {
                    if let Some(&id) = self.surface_to_id.get(surface) {
                        if !self.floating.contains(&id) {
                            let geo = window.geometry();
                            tracing::info!(
                                id = id.0,
                                w = geo.size.w,
                                h = geo.size.h,
                                "auto-floating popup/child on commit"
                            );
                            self.floating.insert(id);
                            if let Some(output) = self.get_focused_output() {
                                // Center on output
                                if let Some(out_geo) = self.space.output_geometry(&output) {
                                    let w = geo.size.w.max(100);
                                    let h = geo.size.h.max(100);
                                    let cx = out_geo.loc.x + (out_geo.size.w - w) / 2;
                                    let cy = out_geo.loc.y + (out_geo.size.h - h) / 2;
                                    self.space.map_element(window.clone(), (cx, cy), false);
                                }
                                self.space.raise_element(&window, true);
                                self.retile(&output);
                            }
                        }
                    }
                }

                // If the window committed a buffer that doesn't match its configured
                // tiled size, force a re-configure to constrain it back.
                // Also retile if the window has zero size (initial commit).
                // (Skip for floating windows — they manage their own size)
                if let Some(&id) = self.surface_to_id.get(surface) {
                    if !self.floating.contains(&id) {
                        let actual = window.geometry().size;

                        // Zero-size window on commit → retile to send proper configure
                        if actual.w == 0 || actual.h == 0 {
                            if let Some(output) = self.get_focused_output() {
                                self.retile(&output);
                            }
                        } else {
                            let target = compositor::with_states(toplevel.wl_surface(), |states| {
                                let attrs = states
                                    .data_map
                                    .get::<XdgToplevelSurfaceData>()
                                    .unwrap()
                                    .lock()
                                    .unwrap();
                                attrs.current_server_state().size
                            });
                            if let Some(target) = target {
                                if actual != target {
                                    toplevel.with_pending_state(|state| {
                                        state.size = Some(target);
                                    });
                                    toplevel.send_configure();
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

delegate_compositor!(DinatorState);

// --- XDG Shell ---

impl XdgShellHandler for DinatorState {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let window = Window::new_wayland_window(surface.clone());
        let id = Self::next_window_id();
        let ws = self.focused_workspace();

        info!("new toplevel window");

        // Clear launch-in-progress cursor
        self.pending_cursor = Some(CursorImageStatus::Named(
            smithay::input::pointer::CursorIcon::Default,
        ));
        self.launch_cursor_set_at = None;

        self.window_map.insert(id, window.clone());
        self.surface_to_id.insert(surface.wl_surface().clone(), id);
        self.window_workspace.insert(id, ws);
        self.ws_window_list_mut(ws).push(id);

        // Mark the toplevel as activated
        surface.with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Activated);
        });

        // Map the window and retile
        let output = self.get_focused_output();
        self.space.map_element(window, (0, 0), false);
        if let Some(ref output) = output {
            output.enter(surface.wl_surface());
            self.retile(output);
        }

        // Give keyboard focus to the new window
        let serial = SERIAL_COUNTER.next_serial();
        if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.set_focus(self, Some(surface.wl_surface().clone()), serial);
        }

        // Emit IPC event
        let (app_id, title) = compositor::with_states(surface.wl_surface(), |states| {
            let attrs = states.data_map.get::<XdgToplevelSurfaceData>();
            let attrs = attrs.map(|d| d.lock().unwrap());
            (
                attrs.as_ref().and_then(|a| a.app_id.clone()),
                attrs.as_ref().and_then(|a| a.title.clone()),
            )
        });

        // Apply window rules
        let rule = self
            .match_window_rule(app_id.as_deref(), title.as_deref())
            .cloned();
        if let Some(rule) = rule {
            if rule.float {
                info!(app_id = ?app_id, "window rule: auto-float");
                self.floating.insert(id);
                if let Some(ref output) = self.get_focused_output() {
                    // Center the floating window
                    let geo = self.space.output_geometry(output);
                    if let (Some(geo), Some(window)) = (geo, self.window_map.get(&id)) {
                        let w = geo.size.w * 2 / 3;
                        let h = geo.size.h * 2 / 3;
                        let x = geo.loc.x + (geo.size.w - w) / 2;
                        let y = geo.loc.y + (geo.size.h - h) / 2;
                        self.space.map_element(window.clone(), (x, y), false);
                        self.space.raise_element(window, true);
                        if let Some(toplevel) = window.toplevel() {
                            toplevel.with_pending_state(|state| {
                                state.size = Some((w, h).into());
                            });
                            toplevel.send_pending_configure();
                        }
                    }
                    self.retile(output);
                }
            } else if rule.fullscreen {
                info!(app_id = ?app_id, "window rule: auto-fullscreen");
                self.fullscreen.insert(id);
                if let Some(ref output) = self.get_focused_output() {
                    self.retile(output);
                }
            }
        }

        self.emit_event(IpcEvent::WindowOpened {
            id: id.0,
            app_id,
            title,
        });
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        if let Some(id) = self.surface_to_id.remove(surface.wl_surface()) {
            self.emit_event(IpcEvent::WindowClosed { id: id.0 });

            // Remove from workspace window list
            let ws = self.window_workspace.remove(&id);
            for order in self.workspace_order.values_mut() {
                order.retain(|w| *w != id);
            }
            self.floating.remove(&id);
            self.fullscreen.remove(&id);

            // Clean up workspace focus tracking
            for focus in self.workspace_focus.values_mut() {
                if *focus == Some(id) {
                    *focus = None;
                }
            }

            if let Some(window) = self.window_map.remove(&id) {
                self.space.unmap_elem(&window);
            }

            // Retile the output that was showing this window's workspace
            if let Some(ws) = ws {
                if let Some(output) = self.output_for_workspace(ws) {
                    self.retile(&output);
                }
            }

            // Focus the last window in the current workspace, if any remain
            let focus_ws = self.focused_workspace();
            let ws_windows = self.ws_window_list(focus_ws).to_vec();
            if let Some(&next_id) = ws_windows.last() {
                if let Some(window) = self.window_map.get(&next_id) {
                    if let Some(surface) = Self::window_wl_surface(window) {
                        let serial = SERIAL_COUNTER.next_serial();
                        if let Some(keyboard) = self.seat.get_keyboard() {
                            keyboard.set_focus(self, Some(surface), serial);
                        }
                    }
                }
            }
        }
    }

    fn maximize_request(&mut self, surface: ToplevelSurface) {
        // In a tiling WM, windows are already maximized within their tile.
        // Just ack the request by setting the state and sending configure.
        surface.with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Maximized);
        });
        surface.send_configure();
    }

    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        surface.with_pending_state(|state| {
            state.states.unset(xdg_toplevel::State::Maximized);
        });
        surface.send_configure();
    }

    fn fullscreen_request(
        &mut self,
        surface: ToplevelSurface,
        _output: Option<smithay::reexports::wayland_server::protocol::wl_output::WlOutput>,
    ) {
        if let Some(id) = self.surface_to_id.get(surface.wl_surface()).copied() {
            self.floating.remove(&id);
            self.fullscreen.insert(id);
            if let Some(output) = self.get_focused_output() {
                self.retile(&output);
            }
        }
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        if let Some(id) = self.surface_to_id.get(surface.wl_surface()).copied() {
            self.fullscreen.remove(&id);
            if let Some(output) = self.get_focused_output() {
                self.retile(&output);
            }
        }
    }

    fn new_popup(&mut self, surface: PopupSurface, positioner: PositionerState) {
        let geo = positioner.get_geometry();
        tracing::info!(
            x = geo.loc.x, y = geo.loc.y,
            w = geo.size.w, h = geo.size.h,
            "new_popup created"
        );
        // Apply positioner geometry before sending configure
        surface.with_pending_state(|state| {
            state.geometry = positioner.get_geometry();
            state.positioner = positioner;
        });
        surface.send_configure().ok();
    }

    fn grab(&mut self, surface: PopupSurface, _seat: WlSeat, serial: Serial) {
        // Give keyboard focus to the popup so it can receive input.
        // A full popup grab (keyboard + pointer) is needed for proper
        // dismiss-on-click-outside behavior, but this is enough for
        // Firefox context menus to render and accept clicks.
        if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.set_focus(self, Some(surface.wl_surface().clone()), serial);
        }
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        surface.with_pending_state(|state| {
            state.geometry = positioner.get_geometry();
            state.positioner = positioner;
        });
        surface.send_repositioned(token);
        surface.send_configure().ok();
    }
}

delegate_xdg_shell!(DinatorState);

// --- XDG Decoration ---

impl XdgDecorationHandler for DinatorState {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode;
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(Mode::ServerSide);
        });
        toplevel.send_configure();
    }

    fn request_mode(
        &mut self,
        toplevel: ToplevelSurface,
        _mode: smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode,
    ) {
        use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode;
        // Always use server-side decorations in a tiling WM
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(Mode::ServerSide);
        });
        toplevel.send_configure();
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode;
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(Mode::ServerSide);
        });
        toplevel.send_configure();
    }
}

smithay::delegate_xdg_decoration!(DinatorState);

// --- Data Device ---

impl SelectionHandler for DinatorState {
    type SelectionUserData = ();

    fn new_selection(
        &mut self,
        ty: SelectionTarget,
        source: Option<SelectionSource>,
        seat: Seat<Self>,
    ) {
        if ty != SelectionTarget::Clipboard {
            return;
        }

        if let Some(source) = source {
            let mime_types = source.mime_types();

            // Check for image mime types first
            let image_mime = mime_types
                .iter()
                .find(|m| m.starts_with("image/png") || m.starts_with("image/bmp"))
                .cloned();

            if let Some(mime) = image_mime.clone() {
                use std::io::Read;
                use std::os::unix::io::FromRawFd;

                if let Ok((read_end, write_end)) = std::os::unix::net::UnixStream::pair() {
                    let write_fd = unsafe {
                        std::os::unix::io::OwnedFd::from_raw_fd(
                            std::os::unix::io::IntoRawFd::into_raw_fd(write_end),
                        )
                    };
                    let _ = data_device::request_data_device_client_selection::<Self>(
                        &seat, mime.clone(), write_fd,
                    );
                    let mut read_stream = read_end;
                    let _ = read_stream.set_read_timeout(Some(std::time::Duration::from_millis(500)));
                    let mut buf = Vec::new();
                    let _ = read_stream.read_to_end(&mut buf);

                    if !buf.is_empty() {
                        tracing::info!(
                            mime = %mime,
                            bytes = buf.len(),
                            "clipboard: Wayland app copied image"
                        );
                        if let Some(ref callback) = self.on_image_clipboard_change {
                            callback(buf);
                        }
                    }
                }
            }

            // Check for text mime types
            let text_mime = mime_types
                .iter()
                .find(|m| {
                    m.starts_with("text/plain")
                        || m.starts_with("text/utf8")
                        || m.starts_with("UTF8_STRING")
                        || m.starts_with("STRING")
                })
                .cloned();

            if let Some(mime) = text_mime {
                // Read the clipboard text via request_data_device_client_selection
                use std::io::Read;
                use std::os::unix::io::FromRawFd;

                match std::os::unix::net::UnixStream::pair() {
                    Ok((read_end, write_end)) => {
                        let write_fd = unsafe {
                            std::os::unix::io::OwnedFd::from_raw_fd(
                                std::os::unix::io::IntoRawFd::into_raw_fd(write_end),
                            )
                        };
                        // Use the public API to request selection data
                        let _ = data_device::request_data_device_client_selection::<Self>(
                            &seat,
                            mime.clone(),
                            write_fd,
                        );

                        // Read from our end
                        let mut read_stream = read_end;
                        let _ = read_stream
                            .set_read_timeout(Some(std::time::Duration::from_millis(100)));
                        let mut buf = Vec::new();
                        let _ = read_stream.read_to_end(&mut buf);

                        if !buf.is_empty() {
                            let text = String::from_utf8_lossy(&buf).to_string();
                            tracing::info!(
                                text_len = text.len(),
                                "clipboard: Wayland app copied text"
                            );
                            if let Some(ref callback) = self.on_clipboard_change {
                                callback(text);
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "clipboard: failed to create socket pair");
                    }
                }
            }
        }
    }

    fn send_selection(
        &mut self,
        ty: SelectionTarget,
        mime_type: String,
        fd: OwnedFd,
        _seat: Seat<Self>,
        _user_data: &Self::SelectionUserData,
    ) {
        if ty != SelectionTarget::Clipboard {
            return;
        }

        // Serve RDP clipboard text to Wayland apps
        if let Some(ref text) = self.rdp_clipboard_text {
            if mime_type.starts_with("text/plain")
                || mime_type == "UTF8_STRING"
                || mime_type == "STRING"
            {
                use std::io::Write;
                let mut file = std::fs::File::from(fd);
                if let Err(e) = file.write_all(text.as_bytes()) {
                    tracing::warn!(error = %e, "clipboard: failed to write to fd");
                }
                tracing::debug!(
                    text_len = text.len(),
                    "clipboard: served RDP text to Wayland app"
                );
            }
        }
    }
}

impl DataDeviceHandler for DinatorState {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}

impl ClientDndGrabHandler for DinatorState {}
impl ServerDndGrabHandler for DinatorState {
    fn send(&mut self, _mime_type: String, _fd: OwnedFd, _seat: Seat<Self>) {}
}

delegate_data_device!(DinatorState);

// --- Primary Selection ---

use smithay::wayland::selection::primary_selection::{
    PrimarySelectionHandler, PrimarySelectionState,
};

impl PrimarySelectionHandler for DinatorState {
    fn primary_selection_state(&self) -> &PrimarySelectionState {
        &self.primary_selection_state
    }
}

smithay::delegate_primary_selection!(DinatorState);

// --- XDG Activation ---

use smithay::wayland::xdg_activation::{
    XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData,
};

impl XdgActivationHandler for DinatorState {
    fn activation_state(&mut self) -> &mut XdgActivationState {
        &mut self.xdg_activation_state
    }

    fn request_activation(
        &mut self,
        _token: XdgActivationToken,
        token_data: XdgActivationTokenData,
        surface: WlSurface,
    ) {
        // Show progress cursor — an app is starting
        self.pending_cursor = Some(CursorImageStatus::Named(
            smithay::input::pointer::CursorIcon::Progress,
        ));
        self.launch_cursor_set_at = Some(std::time::Instant::now());

        // Only honor recent activation requests (within 10 seconds)
        if token_data.timestamp.elapsed().as_secs() < 10 {
            // Find and focus the window that requested activation
            if let Some(id) = self.surface_to_id.get(&surface) {
                if let Some(window) = self.window_map.get(id) {
                    let window = window.clone();
                    self.space.raise_element(&window, true);
                    if let Some(toplevel) = window.toplevel() {
                        let serial = SERIAL_COUNTER.next_serial();
                        if let Some(keyboard) = self.seat.get_keyboard() {
                            keyboard.set_focus(self, Some(toplevel.wl_surface().clone()), serial);
                        }
                    }
                }
            }
        }
    }
}

smithay::delegate_xdg_activation!(DinatorState);

// --- SHM ---

impl BufferHandler for DinatorState {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl ShmHandler for DinatorState {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

delegate_shm!(DinatorState);

// --- Seat ---

impl SeatHandler for DinatorState {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        tracing::info!(?image, "cursor_image changed");
        self.pending_cursor = Some(image);
    }
    fn focus_changed(&mut self, _seat: &Seat<Self>, focused: Option<&WlSurface>) {
        if let Some(surface) = focused {
            if let Some(id) = self.surface_to_id.get(surface) {
                self.emit_event(IpcEvent::WindowFocused { id: id.0 });
            }
        }
    }
}

delegate_seat!(DinatorState);

// --- Cursor Shape ---

use smithay::wayland::tablet_manager::TabletSeatHandler;

impl TabletSeatHandler for DinatorState {}

smithay::delegate_cursor_shape!(DinatorState);

// --- Viewporter ---

smithay::delegate_viewporter!(DinatorState);

// --- Fractional Scale ---

use smithay::wayland::fractional_scale::FractionalScaleHandler;

impl FractionalScaleHandler for DinatorState {}

smithay::delegate_fractional_scale!(DinatorState);

// --- Single Pixel Buffer ---

smithay::delegate_single_pixel_buffer!(DinatorState);

// --- Relative Pointer ---

smithay::delegate_relative_pointer!(DinatorState);

// --- Content Type ---

smithay::delegate_content_type!(DinatorState);

// --- XDG Foreign ---

use smithay::wayland::xdg_foreign::{XdgForeignHandler, XdgForeignState};

impl XdgForeignHandler for DinatorState {
    fn xdg_foreign_state(&mut self) -> &mut XdgForeignState {
        &mut self.xdg_foreign_state
    }
}

smithay::delegate_xdg_foreign!(DinatorState);

// --- Layer Shell ---

use smithay::delegate_layer_shell;
// desktop::layer_map_for_output already imported at top
use smithay::desktop::LayerSurface as DesktopLayerSurface;
use smithay::wayland::shell::wlr_layer::{
    KeyboardInteractivity, Layer, LayerSurface as WlrLayerSurface, LayerSurfaceCachedState,
    LayerSurfaceConfigure, WlrLayerShellHandler, WlrLayerShellState,
};

impl WlrLayerShellHandler for DinatorState {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.layer_shell_state
    }

    fn new_layer_surface(
        &mut self,
        surface: WlrLayerSurface,
        output: Option<WlOutput>,
        layer: Layer,
        namespace: String,
    ) {
        tracing::info!(
            namespace = %namespace,
            layer = ?layer,
            "new layer surface"
        );

        let output = output
            .as_ref()
            .and_then(|o| Output::from_resource(o))
            .or_else(|| self.get_focused_output());

        let desktop_surface = DesktopLayerSurface::new(surface, namespace);

        if let Some(ref output) = output {
            let mut layer_map = layer_map_for_output(output);
            let result = layer_map.map_layer(&desktop_surface);
            layer_map.arrange();
            tracing::info!(
                result = ?result,
                layers = layer_map.layers().count(),
                "layer surface mapped"
            );
        }
    }

    fn ack_configure(&mut self, _surface: WlSurface, _configure: LayerSurfaceConfigure) {}

    fn layer_destroyed(&mut self, surface: WlrLayerSurface) {
        // Search all outputs for this layer surface
        for output in self.space.outputs().cloned().collect::<Vec<_>>() {
            let mut layer_map = layer_map_for_output(&output);
            let wl_surface = surface.wl_surface().clone();
            let layers: Vec<DesktopLayerSurface> = layer_map
                .layers()
                .filter(|l| l.wl_surface() == &wl_surface)
                .cloned()
                .collect();
            for l in layers {
                layer_map.unmap_layer(&l);
            }
            drop(layer_map);
        }

        // Restore keyboard focus to the focused window (if any)
        if let Some(window) = self.focused_window().cloned() {
            if let Some(surface) = Self::window_wl_surface(&window) {
                let keyboard = self.seat.get_keyboard().unwrap();
                let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                keyboard.set_focus(self, Some(surface), serial);
            }
        }
    }
}

delegate_layer_shell!(DinatorState);

// --- Output ---

impl OutputHandler for DinatorState {
    fn output_bound(&mut self, _output: Output, _wl_output: WlOutput) {}
}

delegate_output!(DinatorState);
