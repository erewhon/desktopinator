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

        info!(
            area_x = area.x, area_y = area.y,
            area_w = area.width, area_h = area.height,
            windows = self.window_order.len(),
            "retile area"
        );

        let placements = self.layout.arrange(&self.window_order, area);

        for placement in placements {
            info!(
                id = placement.id.0,
                x = placement.rect.x, y = placement.rect.y,
                w = placement.rect.width, h = placement.rect.height,
                "placement"
            );
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
