use std::os::unix::io::OwnedFd;

use smithay::backend::renderer::utils::on_commit_buffer_handler;
use smithay::delegate_compositor;
use smithay::delegate_data_device;
use smithay::delegate_output;
use smithay::delegate_seat;
use smithay::delegate_shm;
use smithay::delegate_xdg_shell;
use smithay::desktop::{Window, WindowSurfaceType, layer_map_for_output};
use smithay::input::pointer::CursorImageStatus;
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_buffer;
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::protocol::wl_seat::WlSeat;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Serial, SERIAL_COUNTER};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{CompositorClientState, CompositorHandler, CompositorState};
use smithay::wayland::output::OutputHandler;
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
};
use smithay::wayland::shell::xdg::decoration::XdgDecorationHandler;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::wayland::selection::data_device::{
    ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
};
use smithay::wayland::selection::SelectionHandler;
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
        &client.get_data::<ClientState>().unwrap().compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);

        // Handle layer surface commits
        let output = self.space.outputs().next().cloned();
        if let Some(output) = output {
            let mut layer_map = layer_map_for_output(&output);
            let layer = layer_map
                .layer_for_surface(surface, WindowSurfaceType::ALL)
                .cloned();
            if let Some(layer) = layer {
                layer_map.arrange();
                drop(layer_map);

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

            // If the window committed a buffer that doesn't match its configured
            // tiled size, force a re-configure to constrain it back.
            // We use the latest SENT configure size (current_server_state), not the
            // last acked size (current_state), because during a layout switch the
            // client may not have acked the new configure yet. Using the acked size
            // would fight against the pending layout change.
            if let Some(toplevel) = window.toplevel() {
                let target = compositor::with_states(toplevel.wl_surface(), |states| {
                    let attrs = states
                        .data_map
                        .get::<XdgToplevelSurfaceData>()
                        .unwrap()
                        .lock()
                        .unwrap();
                    attrs.current_server_state().size
                });
                let actual = window.geometry().size;
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

delegate_compositor!(DinatorState);

// --- XDG Shell ---

impl XdgShellHandler for DinatorState {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        info!("new toplevel window");
        let window = Window::new_wayland_window(surface.clone());
        let id = Self::next_window_id();

        self.window_order.push(id);
        self.window_map.insert(id, window.clone());
        self.surface_to_id
            .insert(surface.wl_surface().clone(), id);
        self.window_workspace.insert(id, self.active_workspace);

        // Mark the toplevel as activated
        surface.with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Activated);
        });

        // Map the window and retile
        let output = self.space.outputs().next().cloned();
        self.space.map_element(window, (0, 0), false);
        if let Some(ref output) = output {
            // Manually send wl_surface.enter so the client knows its output
            // before committing a buffer. Without this, Space::refresh() won't
            // send it because the window's bbox is zero until the first commit,
            // creating a deadlock with clients that wait for output info.
            output.enter(surface.wl_surface());

            info!(
                windows = self.window_order.len(),
                output = %output.name(),
                "retiling after new window"
            );
            self.retile(output);
        } else {
            info!("no output available for tiling");
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
        let rule = self.match_window_rule(
            app_id.as_deref(),
            title.as_deref(),
        ).cloned();
        if let Some(rule) = rule {
            if rule.float {
                info!(app_id = ?app_id, "window rule: auto-float");
                self.floating.insert(id);
                let output = self.space.outputs().next().cloned();
                if let Some(ref output) = output {
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
                let output = self.space.outputs().next().cloned();
                if let Some(ref output) = output {
                    self.retile(output);
                }
            }
        }

        self.emit_event(IpcEvent::WindowOpened { id: id.0, app_id, title });
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        if let Some(id) = self.surface_to_id.remove(surface.wl_surface()) {
            self.emit_event(IpcEvent::WindowClosed { id: id.0 });

            self.window_order.retain(|w| *w != id);
            self.floating.remove(&id);
            self.fullscreen.remove(&id);

            // Clean up workspace tracking
            self.window_workspace.remove(&id);
            for order in self.workspace_order.values_mut() {
                order.retain(|w| *w != id);
            }
            for focus in self.workspace_focus.values_mut() {
                if *focus == Some(id) {
                    *focus = None;
                }
            }

            if let Some(window) = self.window_map.remove(&id) {
                self.space.unmap_elem(&window);
            }

            let output = self.space.outputs().next().cloned();
            if let Some(output) = output {
                self.retile(&output);
            }

            // Focus the last window in the order, if any remain
            if let Some(&next_id) = self.window_order.last() {
                if let Some(window) = self.window_map.get(&next_id) {
                    if let Some(toplevel) = window.toplevel() {
                        let serial = SERIAL_COUNTER.next_serial();
                        if let Some(keyboard) = self.seat.get_keyboard() {
                            keyboard.set_focus(
                                self,
                                Some(toplevel.wl_surface().clone()),
                                serial,
                            );
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
            let output = self.space.outputs().next().cloned();
            if let Some(output) = output {
                self.retile(&output);
            }
        }
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        if let Some(id) = self.surface_to_id.get(surface.wl_surface()).copied() {
            self.fullscreen.remove(&id);
            let output = self.space.outputs().next().cloned();
            if let Some(output) = output {
                self.retile(&output);
            }
        }
    }

    fn new_popup(&mut self, _surface: PopupSurface, _positioner: PositionerState) {
        // TODO: popup support
    }

    fn grab(&mut self, _surface: PopupSurface, _seat: WlSeat, _serial: Serial) {
        // TODO: popup grabs
    }

    fn reposition_request(
        &mut self,
        _surface: PopupSurface,
        _positioner: PositionerState,
        _token: u32,
    ) {
        // TODO: popup repositioning
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

use smithay::wayland::selection::primary_selection::{PrimarySelectionHandler, PrimarySelectionState};

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
                            keyboard.set_focus(
                                self,
                                Some(toplevel.wl_surface().clone()),
                                serial,
                            );
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

    fn cursor_image(&mut self, _seat: &Seat<Self>, _image: CursorImageStatus) {}
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
        _layer: Layer,
        namespace: String,
    ) {
        let output = output
            .as_ref()
            .and_then(|o| Output::from_resource(o))
            .or_else(|| self.space.outputs().next().cloned());

        let desktop_surface = DesktopLayerSurface::new(surface, namespace);

        if let Some(ref output) = output {
            let mut layer_map = layer_map_for_output(output);
            layer_map.map_layer(&desktop_surface).ok();
        }
    }

    fn ack_configure(
        &mut self,
        _surface: WlSurface,
        _configure: LayerSurfaceConfigure,
    ) {
    }

    fn layer_destroyed(&mut self, surface: WlrLayerSurface) {
        let output = self.space.outputs().next().cloned();
        if let Some(output) = output {
            let mut layer_map = layer_map_for_output(&output);
            // Find the desktop layer surface that wraps this wlr surface
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
            if let Some(toplevel) = window.toplevel() {
                let keyboard = self.seat.get_keyboard().unwrap();
                let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                keyboard.set_focus(self, Some(toplevel.wl_surface().clone()), serial);
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
