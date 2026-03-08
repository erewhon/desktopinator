use std::os::unix::io::OwnedFd;

use smithay::backend::renderer::utils::on_commit_buffer_handler;
use smithay::delegate_compositor;
use smithay::delegate_data_device;
use smithay::delegate_output;
use smithay::delegate_seat;
use smithay::delegate_shm;
use smithay::delegate_xdg_shell;
use smithay::desktop::Window;
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
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::wayland::selection::data_device::{
    ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
};
use smithay::wayland::selection::SelectionHandler;
use smithay::wayland::shm::{ShmHandler, ShmState};

use tracing::info;

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

        let found = self
            .space
            .elements()
            .find(|w| w.toplevel().is_some_and(|t| t.wl_surface() == surface))
            .cloned();

        if let Some(window) = found {
            window.on_commit();

            // If the window committed a buffer that doesn't match its configured
            // tiled size, force a re-configure to constrain it back.
            // We use send_configure() instead of retile/send_pending_configure
            // because the latter is a no-op when the pending size hasn't changed.
            if let Some(toplevel) = window.toplevel() {
                let configured_size = toplevel.current_state().size;
                let actual = window.geometry().size;
                if let Some(target) = configured_size {
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
        let keyboard = self.seat.get_keyboard().unwrap();
        keyboard.set_focus(self, Some(surface.wl_surface().clone()), serial);
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        if let Some(id) = self.surface_to_id.remove(surface.wl_surface()) {
            self.window_order.retain(|w| *w != id);
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
                        let keyboard = self.seat.get_keyboard().unwrap();
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
    fn focus_changed(&mut self, _seat: &Seat<Self>, _focused: Option<&WlSurface>) {}
}

delegate_seat!(DinatorState);

// --- Output ---

impl OutputHandler for DinatorState {
    fn output_bound(&mut self, _output: Output, _wl_output: WlOutput) {}
}

delegate_output!(DinatorState);
