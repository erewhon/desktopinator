use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use smithay::desktop::{Space, Window};
use smithay::input::{Seat, SeatState};
use smithay::output::Output;
use smithay::reexports::calloop::LoopSignal;
use smithay::reexports::wayland_server::backend::ClientData;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Display, DisplayHandle};
use smithay::wayland::compositor::CompositorState;
use smithay::wayland::selection::data_device::DataDeviceState;
use smithay::wayland::shell::xdg::XdgShellState;
use smithay::wayland::selection::primary_selection::PrimarySelectionState;
use smithay::wayland::cursor_shape::CursorShapeManagerState;
use smithay::wayland::xdg_activation::XdgActivationState;
use smithay::wayland::shell::xdg::decoration::XdgDecorationState;
use smithay::wayland::shm::ShmState;

use tracing::info;

use dinator_tiling::{ColumnLayout, Layout, Rect, WindowId};

static NEXT_WINDOW_ID: AtomicU64 = AtomicU64::new(1);

pub struct DinatorState {
    pub display: Display<Self>,
    pub display_handle: DisplayHandle,
    pub loop_signal: LoopSignal,

    // Smithay protocol state
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub shm_state: ShmState,
    pub data_device_state: DataDeviceState,
    pub primary_selection_state: PrimarySelectionState,
    pub xdg_decoration_state: XdgDecorationState,
    pub xdg_activation_state: XdgActivationState,
    pub cursor_shape_state: CursorShapeManagerState,
    pub seat_state: SeatState<Self>,
    pub seat: Seat<Self>,

    // Desktop
    pub space: Space<Window>,
    pub start_time: Instant,

    // Tiling
    pub layout: Box<dyn Layout>,
    pub window_order: Vec<WindowId>,
    pub window_map: HashMap<WindowId, Window>,
    pub surface_to_id: HashMap<WlSurface, WindowId>,
}

impl DinatorState {
    pub fn new(display: Display<Self>, loop_signal: LoopSignal) -> Self {
        let display_handle = display.handle();
        let compositor_state = CompositorState::new::<Self>(&display_handle);
        let xdg_shell_state = XdgShellState::new::<Self>(&display_handle);
        let shm_state = ShmState::new::<Self>(&display_handle, Vec::new());
        let data_device_state = DataDeviceState::new::<Self>(&display_handle);
        let primary_selection_state = PrimarySelectionState::new::<Self>(&display_handle);
        let xdg_decoration_state = XdgDecorationState::new::<Self>(&display_handle);
        let xdg_activation_state = XdgActivationState::new::<Self>(&display_handle);
        let cursor_shape_state = CursorShapeManagerState::new::<Self>(&display_handle);
        let mut seat_state = SeatState::new();
        let seat = seat_state.new_wl_seat(&display_handle, "desktopinator");

        let space = Space::default();

        Self {
            display,
            display_handle,
            loop_signal,
            compositor_state,
            xdg_shell_state,
            shm_state,
            data_device_state,
            primary_selection_state,
            xdg_decoration_state,
            xdg_activation_state,
            cursor_shape_state,
            seat_state,
            seat,
            space,
            start_time: Instant::now(),
            layout: Box::new(ColumnLayout::default()),
            window_order: Vec::new(),
            window_map: HashMap::new(),
            surface_to_id: HashMap::new(),
        }
    }

    pub fn next_window_id() -> WindowId {
        WindowId(NEXT_WINDOW_ID.fetch_add(1, Ordering::Relaxed))
    }

    /// Re-tile all windows on the given output.
    pub fn retile(&mut self, output: &Output) {
        let geo = self.space.output_geometry(output);
        let Some(geo) = geo else { return };

        let area = Rect {
            x: geo.loc.x,
            y: geo.loc.y,
            width: geo.size.w,
            height: geo.size.h,
        };

        let placements = self.layout.arrange(&self.window_order, area);

        for placement in placements {
            if let Some(window) = self.window_map.get(&placement.id) {
                let loc: smithay::utils::Point<i32, smithay::utils::Logical> =
                    (placement.rect.x, placement.rect.y).into();
                self.space.map_element(window.clone(), loc, false);

                if let Some(toplevel) = window.toplevel() {
                    toplevel.with_pending_state(|state| {
                        state.size =
                            Some((placement.rect.width, placement.rect.height).into());
                    });
                    toplevel.send_pending_configure();
                }
            }
        }
    }

    /// Returns the currently focused window, if any.
    pub fn focused_window(&self) -> Option<&Window> {
        let keyboard = self.seat.get_keyboard()?;
        let surface = keyboard.current_focus()?;
        let id = self.surface_to_id.get(&surface)?;
        self.window_map.get(id)
    }

    /// Close the currently focused window.
    pub fn close_focused_window(&mut self) {
        let keyboard = self.seat.get_keyboard().unwrap();
        let focus = keyboard.current_focus();
        if let Some(surface) = focus {
            if let Some(id) = self.surface_to_id.get(&surface) {
                if let Some(window) = self.window_map.get(id) {
                    if let Some(toplevel) = window.toplevel() {
                        toplevel.send_close();
                    }
                }
            }
        }
    }

    /// Focus the next window in the window order.
    pub fn focus_next(&mut self) {
        self.focus_cycle(1);
    }

    /// Focus the previous window in the window order.
    pub fn focus_prev(&mut self) {
        self.focus_cycle(-1);
    }

    /// Swap the focused window with the master (first) position.
    pub fn swap_master(&mut self) {
        if self.window_order.len() < 2 {
            return;
        }

        let keyboard = self.seat.get_keyboard().unwrap();
        let current_focus = keyboard.current_focus();

        let focused_idx = current_focus
            .as_ref()
            .and_then(|surface| self.surface_to_id.get(surface))
            .and_then(|id| self.window_order.iter().position(|w| w == id));

        if let Some(idx) = focused_idx {
            if idx != 0 {
                self.window_order.swap(0, idx);
                let output = self.space.outputs().next().cloned();
                if let Some(output) = output {
                    self.retile(&output);
                }
            }
        }
    }

    fn focus_cycle(&mut self, direction: i32) {
        if self.window_order.len() < 2 {
            return;
        }

        let keyboard = self.seat.get_keyboard().unwrap();
        let current_focus = keyboard.current_focus();

        let current_idx = current_focus
            .as_ref()
            .and_then(|surface| self.surface_to_id.get(surface))
            .and_then(|id| self.window_order.iter().position(|w| w == id))
            .unwrap_or(0);

        let len = self.window_order.len() as i32;
        let next_idx = ((current_idx as i32 + direction).rem_euclid(len)) as usize;
        let next_id = self.window_order[next_idx];

        if let Some(window) = self.window_map.get(&next_id) {
            let window = window.clone();
            self.space.raise_element(&window, true);
            if let Some(toplevel) = window.toplevel() {
                let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                keyboard.set_focus(self, Some(toplevel.wl_surface().clone()), serial);
                info!(idx = next_idx, "focus cycled");
            }
        }
    }
}

/// Client data stored per Wayland client connection.
#[derive(Default)]
pub struct ClientState {
    pub compositor_state: smithay::wayland::compositor::CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: smithay::reexports::wayland_server::backend::ClientId) {}
    fn disconnected(
        &self,
        _client_id: smithay::reexports::wayland_server::backend::ClientId,
        _reason: smithay::reexports::wayland_server::backend::DisconnectReason,
    ) {
    }
}
