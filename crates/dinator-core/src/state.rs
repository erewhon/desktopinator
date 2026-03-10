use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
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
use smithay::wayland::content_type::ContentTypeState;
use smithay::wayland::cursor_shape::CursorShapeManagerState;
use smithay::wayland::fractional_scale::FractionalScaleManagerState;
use smithay::wayland::relative_pointer::RelativePointerManagerState;
use smithay::wayland::single_pixel_buffer::SinglePixelBufferState;
use smithay::wayland::viewporter::ViewporterState;
use smithay::wayland::xdg_activation::XdgActivationState;
use smithay::wayland::xdg_foreign::XdgForeignState;
use smithay::wayland::shell::xdg::decoration::XdgDecorationState;
use smithay::wayland::shell::wlr_layer::WlrLayerShellState;
use smithay::wayland::shm::ShmState;
use smithay::wayland::xwayland_shell::XWaylandShellState;
use smithay::xwayland::{X11Surface, X11Wm};

use tracing::info;

use dinator_ipc::IpcEvent;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;

use dinator_plugin_api::{PluginAction, PluginEvent, PluginRuntime, WindowRule};
use dinator_tiling::{CenteredMasterLayout, ColumnLayout, DwindleLayout, Layout, MonocleLayout, Rect, WindowId};

/// Thread-safe broadcaster for IPC events.
/// IPC client threads register their sender here; the compositor emits events.
pub type EventBroadcaster = Arc<Mutex<Vec<std::sync::mpsc::Sender<IpcEvent>>>>;

/// Background configuration for the compositor.
#[derive(Debug, Clone)]
pub enum Background {
    /// Solid color [r, g, b, a] (0.0-1.0).
    Solid([f32; 4]),
    /// Vertical gradient from top color to bottom color.
    Gradient { top: [f32; 4], bottom: [f32; 4] },
}

impl Default for Background {
    fn default() -> Self {
        Background::Solid([0.1, 0.1, 0.1, 1.0])
    }
}

/// Parse a background spec string into a Background.
///
/// Formats:
///   "#RRGGBB"           — solid hex color
///   "r,g,b"             — solid color (0-255 or 0.0-1.0)
///   "#RRGGBB-#RRGGBB"   — vertical gradient (top-bottom)
///   "r,g,b-r,g,b"       — vertical gradient
pub fn parse_background(spec: &str) -> Option<Background> {
    if let Some((top, bottom)) = spec.split_once('-') {
        // Check it's not a hex color starting with #
        if top.starts_with('#') && !bottom.starts_with('#') {
            // Single hex color like "#112233"
            let c = parse_color(spec)?;
            return Some(Background::Solid(c));
        }
        let top = parse_color(top)?;
        let bottom = parse_color(bottom)?;
        Some(Background::Gradient { top, bottom })
    } else {
        let c = parse_color(spec)?;
        Some(Background::Solid(c))
    }
}

fn parse_color(s: &str) -> Option<[f32; 4]> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix('#') {
        if hex.len() == 6 {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            return Some([r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0]);
        }
        return None;
    }
    let parts: Vec<&str> = s.split(',').collect();
    if parts.len() == 3 {
        let vals: Vec<f32> = parts.iter().filter_map(|p| p.trim().parse::<f32>().ok()).collect();
        if vals.len() == 3 {
            if vals.iter().all(|v| *v <= 1.0) {
                return Some([vals[0], vals[1], vals[2], 1.0]);
            } else {
                return Some([vals[0] / 255.0, vals[1] / 255.0, vals[2] / 255.0, 1.0]);
            }
        }
    }
    None
}

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
    pub fractional_scale_state: FractionalScaleManagerState,
    pub viewporter_state: ViewporterState,
    pub single_pixel_buffer_state: SinglePixelBufferState,
    pub relative_pointer_state: RelativePointerManagerState,
    pub content_type_state: ContentTypeState,
    pub xdg_foreign_state: XdgForeignState,
    pub layer_shell_state: WlrLayerShellState,
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

    // Floating & fullscreen
    pub floating: HashSet<WindowId>,
    pub fullscreen: HashSet<WindowId>,

    // IPC event broadcasting
    pub event_broadcaster: EventBroadcaster,

    // Plugin system
    pub plugin_runtime: Option<Box<dyn PluginRuntime>>,

    /// Plugin-registered keybindings: (keysym, alt, ctrl, shift, logo, callback_id).
    pub plugin_keybindings: Vec<(u32, bool, bool, bool, bool, String)>,

    /// Window rules from plugins: match criteria → auto-apply float/fullscreen.
    pub window_rules: Vec<WindowRule>,

    // Background
    pub background: Background,

    // Workspaces
    pub active_workspace: usize,
    pub window_workspace: HashMap<WindowId, usize>,
    pub workspace_order: HashMap<usize, Vec<WindowId>>,
    pub workspace_focus: HashMap<usize, Option<WindowId>>,

    // Clipboard sync (RDP ↔ Wayland)
    /// Called when a Wayland client sets the clipboard. Receives UTF-8 text.
    pub on_clipboard_change: Option<Box<dyn Fn(String)>>,
    /// Text from RDP client clipboard, available for Wayland apps to paste.
    pub rdp_clipboard_text: Option<String>,

    // XWayland
    pub xwayland_shell_state: XWaylandShellState,
    pub x11_wm: Option<X11Wm>,
    /// Map X11 surfaces to window IDs for tiling integration.
    pub x11_surface_to_id: HashMap<u32, WindowId>,
    /// X11 windows waiting for wl_surface pairing (mapped but no surface yet).
    pub pending_x11_windows: Vec<X11Surface>,
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
        let fractional_scale_state = FractionalScaleManagerState::new::<Self>(&display_handle);
        let viewporter_state = ViewporterState::new::<Self>(&display_handle);
        let single_pixel_buffer_state = SinglePixelBufferState::new::<Self>(&display_handle);
        let relative_pointer_state = RelativePointerManagerState::new::<Self>(&display_handle);
        let content_type_state = ContentTypeState::new::<Self>(&display_handle);
        let xdg_foreign_state = XdgForeignState::new::<Self>(&display_handle);
        let layer_shell_state = WlrLayerShellState::new::<Self>(&display_handle);
        let xwayland_shell_state = XWaylandShellState::new::<Self>(&display_handle);
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
            fractional_scale_state,
            viewporter_state,
            single_pixel_buffer_state,
            relative_pointer_state,
            content_type_state,
            xdg_foreign_state,
            layer_shell_state,
            seat_state,
            seat,
            space,
            start_time: Instant::now(),
            layout: Box::new(ColumnLayout::default()),
            window_order: Vec::new(),
            window_map: HashMap::new(),
            surface_to_id: HashMap::new(),
            floating: HashSet::new(),
            fullscreen: HashSet::new(),
            event_broadcaster: Arc::new(Mutex::new(Vec::new())),
            plugin_runtime: None,
            plugin_keybindings: Vec::new(),
            window_rules: Vec::new(),
            background: Background::default(),
            active_workspace: 1,
            window_workspace: HashMap::new(),
            workspace_order: HashMap::new(),
            workspace_focus: HashMap::new(),
            on_clipboard_change: None,
            rdp_clipboard_text: None,
            xwayland_shell_state,
            x11_wm: None,
            x11_surface_to_id: HashMap::new(),
            pending_x11_windows: Vec::new(),
        }
    }

    pub fn next_window_id() -> WindowId {
        WindowId(NEXT_WINDOW_ID.fetch_add(1, Ordering::Relaxed))
    }

    /// Get the WlSurface for a Window, whether it's Wayland or X11.
    pub fn window_wl_surface(window: &Window) -> Option<WlSurface> {
        if let Some(toplevel) = window.toplevel() {
            Some(toplevel.wl_surface().clone())
        } else if let Some(x11) = window.x11_surface() {
            x11.wl_surface().clone()
        } else {
            None
        }
    }

    /// Broadcast an IPC event to all subscribed clients.
    /// Removes disconnected subscribers automatically.
    /// Also forwards to the plugin runtime if present.
    pub fn emit_event(&mut self, event: IpcEvent) {
        {
            let mut subs = self.event_broadcaster.lock().unwrap();
            subs.retain(|tx| tx.send(event.clone()).is_ok());
        }

        // Forward to plugin runtime
        if let Some(ref mut runtime) = self.plugin_runtime {
            let plugin_event = match &event {
                IpcEvent::WindowOpened { id, app_id, title } => Some(PluginEvent::WindowOpened {
                    id: *id,
                    app_id: app_id.clone(),
                    title: title.clone(),
                }),
                IpcEvent::WindowClosed { id } => Some(PluginEvent::WindowClosed { id: *id }),
                IpcEvent::WindowFocused { id } => Some(PluginEvent::WindowFocused { id: *id }),
                IpcEvent::LayoutChanged { name } => Some(PluginEvent::LayoutChanged {
                    name: name.clone(),
                }),
                IpcEvent::WorkspaceChanged { workspace } => {
                    Some(PluginEvent::WorkspaceChanged { workspace: *workspace })
                }
                IpcEvent::WindowMovedWorkspace { id, workspace } => {
                    Some(PluginEvent::WindowMovedWorkspace {
                        id: *id,
                        workspace: *workspace,
                    })
                }
                _ => None,
            };
            if let Some(pe) = plugin_event {
                runtime.on_event(&pe);
            }
        }

        // Execute any actions queued by plugin event handlers
        self.execute_plugin_actions();
    }

    /// Drain and execute any pending plugin actions.
    pub fn execute_plugin_actions(&mut self) {
        let actions = if let Some(ref mut runtime) = self.plugin_runtime {
            runtime.drain_actions()
        } else {
            return;
        };

        for action in actions {
            match action {
                PluginAction::Spawn { cmd, args } => {
                    info!(cmd = %cmd, "plugin action: spawn");
                    if let Err(e) = std::process::Command::new(&cmd).args(&args).spawn() {
                        info!(error = %e, "plugin spawn failed");
                    }
                }
                PluginAction::SetLayout { name } => {
                    info!(layout = %name, "plugin action: set_layout");
                    if self.set_layout(&name) {
                        let output = self.space.outputs().next().cloned();
                        if let Some(output) = output {
                            self.retile(&output);
                        }
                    }
                }
                PluginAction::FocusNext => self.focus_next(),
                PluginAction::FocusPrev => self.focus_prev(),
                PluginAction::CloseWindow => self.close_focused_window(),
                PluginAction::SwapMaster => self.swap_master(),
                PluginAction::ToggleFloat => { self.toggle_float(); }
                PluginAction::ToggleFullscreen => { self.toggle_fullscreen(); }
                PluginAction::Log { message } => {
                    info!(plugin_log = %message);
                }
                PluginAction::SwitchWorkspace { workspace } => {
                    self.switch_workspace(workspace);
                }
                PluginAction::MoveToWorkspace { workspace } => {
                    self.move_to_workspace(workspace);
                }
            }
        }
    }

    /// Check if a window matches any window rule and return the first match.
    pub fn match_window_rule(&self, app_id: Option<&str>, title: Option<&str>) -> Option<&WindowRule> {
        self.window_rules.iter().find(|rule| {
            let app_match = match (&rule.app_id, app_id) {
                (Some(pattern), Some(id)) => id == pattern,
                (Some(_), None) => false,
                (None, _) => true,
            };
            let title_match = match (&rule.title, title) {
                (Some(pattern), Some(t)) => t.contains(pattern.as_str()),
                (Some(_), None) => false,
                (None, _) => true,
            };
            app_match && title_match
        })
    }

    /// Switch to a workspace (1-9).
    pub fn switch_workspace(&mut self, workspace: usize) {
        if workspace < 1 || workspace > 9 || workspace == self.active_workspace {
            return;
        }

        info!(from = self.active_workspace, to = workspace, "switching workspace");

        // Save current focus
        let current_focus = self
            .seat
            .get_keyboard()
            .and_then(|kb| kb.current_focus())
            .and_then(|s| self.surface_to_id.get(&s).copied());
        self.workspace_focus
            .insert(self.active_workspace, current_focus);

        // Save current window_order to workspace_order
        self.workspace_order
            .insert(self.active_workspace, self.window_order.clone());

        // Unmap all current workspace windows from Space
        for &id in &self.window_order {
            if let Some(window) = self.window_map.get(&id) {
                self.space.unmap_elem(window);
            }
        }

        // Clear keyboard focus
        if let Some(keyboard) = self.seat.get_keyboard() {
            let serial = smithay::utils::SERIAL_COUNTER.next_serial();
            keyboard.set_focus(self, Option::<WlSurface>::None, serial);
        }

        // Switch to new workspace
        self.active_workspace = workspace;

        // Restore window_order from workspace_order
        self.window_order = self
            .workspace_order
            .get(&workspace)
            .cloned()
            .unwrap_or_default();

        // Map and retile
        let output = self.space.outputs().next().cloned();
        if let Some(ref output) = output {
            self.retile(output);
        }

        // Restore focus
        let saved_focus = self
            .workspace_focus
            .get(&workspace)
            .copied()
            .flatten();
        let focus_id = saved_focus.or_else(|| self.window_order.last().copied());
        if let Some(id) = focus_id {
            if let Some(window) = self.window_map.get(&id) {
                let window = window.clone();
                self.space.raise_element(&window, true);
                if let Some(surface) = Self::window_wl_surface(&window) {
                    if let Some(keyboard) = self.seat.get_keyboard() {
                        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                        keyboard.set_focus(self, Some(surface), serial);
                    }
                }
            }
        }

        self.emit_event(IpcEvent::WorkspaceChanged { workspace });
    }

    /// Move the focused window to a workspace (1-9).
    pub fn move_to_workspace(&mut self, workspace: usize) {
        if workspace < 1 || workspace > 9 || workspace == self.active_workspace {
            return;
        }

        let Some(keyboard) = self.seat.get_keyboard() else { return };
        let Some(surface) = keyboard.current_focus() else { return };
        let Some(&id) = self.surface_to_id.get(&surface) else { return };

        info!(window = id.0, to = workspace, "moving window to workspace");

        // Update workspace assignment
        self.window_workspace.insert(id, workspace);

        // Remove from current window_order (active workspace)
        self.window_order.retain(|w| *w != id);

        // Add to target workspace's order
        self.workspace_order
            .entry(workspace)
            .or_default()
            .push(id);

        // Unmap from Space
        if let Some(window) = self.window_map.get(&id) {
            self.space.unmap_elem(window);
        }

        // Retile remaining windows
        let output = self.space.outputs().next().cloned();
        if let Some(ref output) = output {
            self.retile(output);
        }

        // Focus next window on current workspace
        if let Some(&next_id) = self.window_order.last() {
            if let Some(window) = self.window_map.get(&next_id) {
                if let Some(surface) = Self::window_wl_surface(window) {
                    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                    keyboard.set_focus(self, Some(surface), serial);
                }
            }
        } else {
            let serial = smithay::utils::SERIAL_COUNTER.next_serial();
            keyboard.set_focus(self, Option::<WlSurface>::None, serial);
        }

        self.emit_event(IpcEvent::WindowMovedWorkspace {
            id: id.0,
            workspace,
        });
    }

    /// Re-tile all windows on the given output.
    /// Floating and fullscreen windows are excluded from the tiling layout.
    pub fn retile(&mut self, output: &Output) {
        let geo = self.space.output_geometry(output);
        let Some(geo) = geo else { return };

        let area = Rect {
            x: geo.loc.x,
            y: geo.loc.y,
            width: geo.size.w,
            height: geo.size.h,
        };

        // Only tile windows that are not floating or fullscreen
        let tiled_windows: Vec<WindowId> = self
            .window_order
            .iter()
            .copied()
            .filter(|id| !self.floating.contains(id) && !self.fullscreen.contains(id))
            .collect();

        let placements = self.layout.arrange(&tiled_windows, area);

        for placement in placements {
            if let Some(window) = self.window_map.get(&placement.id) {
                let loc: smithay::utils::Point<i32, smithay::utils::Logical> =
                    (placement.rect.x, placement.rect.y).into();
                self.space.map_element(window.clone(), loc, false);

                if let Some(toplevel) = window.toplevel() {
                    toplevel.with_pending_state(|state| {
                        state.size =
                            Some((placement.rect.width, placement.rect.height).into());
                        state.states.unset(xdg_toplevel::State::Fullscreen);
                    });
                    toplevel.send_pending_configure();
                } else if let Some(x11) = window.x11_surface() {
                    let rect = smithay::utils::Rectangle::new(
                        (placement.rect.x, placement.rect.y).into(),
                        (placement.rect.width, placement.rect.height).into(),
                    );
                    let _ = x11.configure(Some(rect));
                }
            }
        }

        // Fullscreen windows fill the entire output
        for &id in &self.fullscreen {
            if let Some(window) = self.window_map.get(&id) {
                self.space.map_element(window.clone(), (geo.loc.x, geo.loc.y), false);
                if let Some(toplevel) = window.toplevel() {
                    toplevel.with_pending_state(|state| {
                        state.size = Some((geo.size.w, geo.size.h).into());
                        state.states.set(xdg_toplevel::State::Fullscreen);
                    });
                    toplevel.send_pending_configure();
                } else if let Some(x11) = window.x11_surface() {
                    let rect = smithay::utils::Rectangle::new(
                        (geo.loc.x, geo.loc.y).into(),
                        (geo.size.w, geo.size.h).into(),
                    );
                    let _ = x11.configure(Some(rect));
                    let _ = x11.set_fullscreen(true);
                }
                // Raise fullscreen windows above tiled ones
                self.space.raise_element(window, false);
            }
        }
    }

    /// Set the tiling layout by name. Returns true if changed.
    /// Checks built-in layouts first, then plugin-provided layouts.
    pub fn set_layout(&mut self, name: &str) -> bool {
        match name {
            "column" => {
                self.layout = Box::new(ColumnLayout::default());
                return true;
            }
            "monocle" => {
                self.layout = Box::new(MonocleLayout::default());
                return true;
            }
            "dwindle" => {
                self.layout = Box::new(DwindleLayout::default());
                return true;
            }
            "centered" => {
                self.layout = Box::new(CenteredMasterLayout::default());
                return true;
            }
            _ => {}
        }

        // Check plugin layouts
        if let Some(ref mut runtime) = self.plugin_runtime {
            if let Some(layout) = runtime.create_layout(name) {
                self.layout = layout;
                return true;
            }
        }

        false
    }

    /// List all available layout names (built-in + plugin-provided).
    pub fn available_layouts(&self) -> Vec<String> {
        let mut names = vec![
            "column".to_string(),
            "monocle".to_string(),
            "dwindle".to_string(),
            "centered".to_string(),
        ];
        if let Some(ref runtime) = self.plugin_runtime {
            names.extend(runtime.layout_names());
        }
        names
    }

    /// Toggle the focused window between floating and tiled.
    pub fn toggle_float(&mut self) -> Option<(WindowId, bool)> {
        let keyboard = self.seat.get_keyboard()?;
        let surface = keyboard.current_focus()?;
        let id = *self.surface_to_id.get(&surface)?;

        let is_floating = if self.floating.contains(&id) {
            self.floating.remove(&id);
            false
        } else {
            // Remove from fullscreen if it was fullscreen
            self.fullscreen.remove(&id);
            self.floating.insert(id);
            true
        };

        let output = self.space.outputs().next().cloned();
        if let Some(output) = output {
            if !is_floating {
                // Returning to tiled: retile will place it
                self.retile(&output);
            } else {
                // Going floating: center it at a reasonable size
                let geo = self.space.output_geometry(&output);
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
                            state.states.unset(xdg_toplevel::State::Fullscreen);
                        });
                        toplevel.send_pending_configure();
                    } else if let Some(x11) = window.x11_surface() {
                        let rect = smithay::utils::Rectangle::new(
                            (x, y).into(),
                            (w, h).into(),
                        );
                        let _ = x11.configure(Some(rect));
                    }
                }
                // Retile remaining tiled windows
                self.retile(&output);
            }
        }

        Some((id, is_floating))
    }

    /// Toggle fullscreen for the focused window.
    pub fn toggle_fullscreen(&mut self) -> Option<(WindowId, bool)> {
        let keyboard = self.seat.get_keyboard()?;
        let surface = keyboard.current_focus()?;
        let id = *self.surface_to_id.get(&surface)?;

        let is_fullscreen = if self.fullscreen.contains(&id) {
            self.fullscreen.remove(&id);
            false
        } else {
            // Remove from floating if it was floating
            self.floating.remove(&id);
            self.fullscreen.insert(id);
            true
        };

        let output = self.space.outputs().next().cloned();
        if let Some(output) = output {
            self.retile(&output);
        }

        Some((id, is_fullscreen))
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
        let Some(keyboard) = self.seat.get_keyboard() else { return };
        let focus = keyboard.current_focus();
        if let Some(surface) = focus {
            if let Some(id) = self.surface_to_id.get(&surface) {
                if let Some(window) = self.window_map.get(id) {
                    if let Some(toplevel) = window.toplevel() {
                        toplevel.send_close();
                    } else if let Some(x11) = window.x11_surface() {
                        let _ = x11.close();
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

        let Some(keyboard) = self.seat.get_keyboard() else { return };
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

        let Some(keyboard) = self.seat.get_keyboard() else { return };
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
            if let Some(surface) = Self::window_wl_surface(&window) {
                let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                keyboard.set_focus(self, Some(surface), serial);
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
