//! XWayland integration — runs X11 apps inside the Wayland compositor.

use smithay::desktop::Window;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Rectangle, SERIAL_COUNTER};
use smithay::wayland::xwayland_shell::{XWaylandShellHandler, XWaylandShellState};
use smithay::xwayland::xwm::{Reorder, ResizeEdge, X11Window, XwmId};
use smithay::xwayland::{X11Surface, X11Wm, XwmHandler};

use tracing::info;

use dinator_ipc::IpcEvent;

use crate::state::DinatorState;

impl XwmHandler for DinatorState {
    fn xwm_state(&mut self, _xwm: XwmId) -> &mut X11Wm {
        self.x11_wm.as_mut().expect("X11Wm not initialized")
    }

    fn new_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn new_override_redirect_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn map_window_request(&mut self, _xwm: XwmId, window: X11Surface) {
        info!(
            title = ?window.title(),
            class = ?window.class(),
            "XWayland: map window request"
        );
        window.set_mapped(true).expect("set X11 window mapped");
    }

    fn map_window_notify(&mut self, _xwm: XwmId, window: X11Surface) {
        if window.is_override_redirect() {
            self.map_x11_override_redirect(window);
            return;
        }

        // Try to finish mapping now if wl_surface is already paired
        if window.wl_surface().is_some() {
            self.finish_x11_map(window);
        } else {
            // Surface pairing hasn't happened yet — defer until surface_associated
            info!(
                title = ?window.title(),
                class = ?window.class(),
                "XWayland: deferring window map (no wl_surface yet)"
            );
            self.pending_x11_windows.push(window);
        }
    }

    fn mapped_override_redirect_window(&mut self, _xwm: XwmId, window: X11Surface) {
        self.map_x11_override_redirect(window);
    }

    fn unmapped_window(&mut self, _xwm: XwmId, window: X11Surface) {
        info!(
            title = ?window.title(),
            class = ?window.class(),
            "XWayland: window unmapped"
        );

        if window.is_override_redirect() {
            self.unmap_x11_override_redirect(&window);
            return;
        }

        // Remove from pending list if it was waiting for surface pairing
        self.pending_x11_windows
            .retain(|w| w.window_id() != window.window_id());

        if let Some(id) = self.x11_surface_to_id.remove(&window.window_id()) {
            self.emit_event(IpcEvent::WindowClosed { id: id.0 });

            // Remove from workspace tracking
            let ws = self.window_workspace.remove(&id);
            for order in self.workspace_order.values_mut() {
                order.retain(|w| *w != id);
            }
            self.floating.remove(&id);
            self.fullscreen.remove(&id);

            for focus in self.workspace_focus.values_mut() {
                if *focus == Some(id) {
                    *focus = None;
                }
            }

            if let Some(wl_surface) = window.wl_surface() {
                self.surface_to_id.remove(&wl_surface);
            }

            if let Some(w) = self.window_map.remove(&id) {
                self.space.unmap_elem(&w);
            }

            // Retile the output showing this window's workspace
            if let Some(ws) = ws {
                if let Some(output) = self.output_for_workspace(ws) {
                    self.retile(&output);
                }
            }

            // Focus next window on the focused workspace
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

    fn destroyed_window(&mut self, _xwm: XwmId, window: X11Surface) {
        self.pending_x11_windows
            .retain(|w| w.window_id() != window.window_id());

        if let Some(id) = self.x11_surface_to_id.remove(&window.window_id()) {
            self.window_workspace.remove(&id);
            for order in self.workspace_order.values_mut() {
                order.retain(|w| *w != id);
            }
            self.floating.remove(&id);
            self.fullscreen.remove(&id);

            if let Some(wl_surface) = window.wl_surface() {
                self.surface_to_id.remove(&wl_surface);
            }

            if let Some(w) = self.window_map.remove(&id) {
                self.space.unmap_elem(&w);
            }
        }
    }

    fn configure_request(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        x: Option<i32>,
        y: Option<i32>,
        w: Option<u32>,
        h: Option<u32>,
        _reorder: Option<Reorder>,
    ) {
        let is_floating = self
            .x11_surface_to_id
            .get(&window.window_id())
            .map(|id| self.floating.contains(id))
            .unwrap_or(false);

        if is_floating || window.is_override_redirect() {
            let geo = window.geometry();
            let rect = Rectangle::new(
                (x.unwrap_or(geo.loc.x), y.unwrap_or(geo.loc.y)).into(),
                (
                    w.map(|v| v as i32).unwrap_or(geo.size.w),
                    h.map(|v| v as i32).unwrap_or(geo.size.h),
                )
                    .into(),
            );
            let _ = window.configure(Some(rect));
        } else {
            let _ = window.configure(None);
        }
    }

    fn configure_notify(
        &mut self,
        _xwm: XwmId,
        _window: X11Surface,
        _geometry: Rectangle<i32, Logical>,
        _above: Option<X11Window>,
    ) {
    }

    fn resize_request(
        &mut self,
        _xwm: XwmId,
        _window: X11Surface,
        _button: u32,
        _resize_edge: ResizeEdge,
    ) {
    }

    fn move_request(&mut self, _xwm: XwmId, _window: X11Surface, _button: u32) {}

    fn maximize_request(&mut self, _xwm: XwmId, window: X11Surface) {
        let _ = window.set_maximized(true);
    }

    fn unmaximize_request(&mut self, _xwm: XwmId, window: X11Surface) {
        let _ = window.set_maximized(false);
    }

    fn fullscreen_request(&mut self, _xwm: XwmId, window: X11Surface) {
        if let Some(&id) = self.x11_surface_to_id.get(&window.window_id()) {
            self.floating.remove(&id);
            self.fullscreen.insert(id);
            if let Some(output) = self.get_focused_output() {
                self.retile(&output);
            }
        }
    }

    fn unfullscreen_request(&mut self, _xwm: XwmId, window: X11Surface) {
        if let Some(&id) = self.x11_surface_to_id.get(&window.window_id()) {
            self.fullscreen.remove(&id);
            let _ = window.set_fullscreen(false);
            if let Some(output) = self.get_focused_output() {
                self.retile(&output);
            }
        }
    }

    fn minimize_request(&mut self, _xwm: XwmId, _window: X11Surface) {}
}

impl XWaylandShellHandler for DinatorState {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.xwayland_shell_state
    }

    fn surface_associated(&mut self, _xwm: XwmId, _wl_surface: WlSurface, surface: X11Surface) {
        info!(
            title = ?surface.title(),
            class = ?surface.class(),
            override_redirect = surface.is_override_redirect(),
            "XWayland: surface associated"
        );

        // Already mapped? Skip.
        if self.x11_surface_to_id.contains_key(&surface.window_id()) {
            return;
        }

        // Override-redirect windows (menus, tooltips) may have been deferred
        // because wl_surface wasn't ready when map_window_notify fired.
        if surface.is_override_redirect() {
            self.map_x11_override_redirect(surface);
            return;
        }

        // Check if this window was waiting for surface pairing
        let was_pending = self
            .pending_x11_windows
            .iter()
            .any(|w| w.window_id() == surface.window_id());

        if was_pending {
            self.pending_x11_windows
                .retain(|w| w.window_id() != surface.window_id());
            self.finish_x11_map(surface);
        }
    }
}

smithay::delegate_xwayland_shell!(DinatorState);

impl DinatorState {
    /// Finish mapping an X11 window — called when both mapped and wl_surface are ready.
    fn finish_x11_map(&mut self, window: X11Surface) {
        let Some(wl_surface) = window.wl_surface() else {
            return;
        };

        // Don't double-map
        if self.x11_surface_to_id.contains_key(&window.window_id()) {
            return;
        }

        // Clear launch-in-progress cursor
        self.pending_cursor = Some(smithay::input::pointer::CursorImageStatus::Named(
            smithay::input::pointer::CursorIcon::Default,
        ));
        self.launch_cursor_set_at = None;

        let id = Self::next_window_id();
        let smithay_window = Window::new_x11_window(window.clone());
        let ws = self.focused_workspace();

        // Auto-float transient windows, dialogs, splashes, utilities, tooltips
        use smithay::xwayland::xwm::WmWindowType;
        let is_transient = window.is_transient_for().is_some();
        let title_lower = window.title().to_lowercase();
        let should_float = is_transient
            || matches!(
                window.window_type(),
                Some(WmWindowType::Dialog)
                    | Some(WmWindowType::Splash)
                    | Some(WmWindowType::Utility)
                    | Some(WmWindowType::Toolbar)
                    | Some(WmWindowType::Notification)
            )
            || title_lower == "splash";

        info!(
            title = ?window.title(),
            class = ?window.class(),
            window_type = ?window.window_type(),
            transient = is_transient,
            float = should_float,
            id = id.0,
            "XWayland: window mapped"
        );

        self.ws_window_list_mut(ws).push(id);
        self.window_map.insert(id, smithay_window.clone());
        self.surface_to_id.insert(wl_surface.clone(), id);
        self.x11_surface_to_id.insert(window.window_id(), id);
        self.window_workspace.insert(id, ws);

        if should_float {
            self.floating.insert(id);
        }

        let _ = window.set_activated(true);

        // Map and retile
        let output = self.get_focused_output();
        if should_float {
            // X11 dialogs often report 1x1 geometry at map time.
            // Use a reasonable default size and center on the output.
            let geo = window.geometry();
            let (win_w, win_h) = if geo.size.w > 10 && geo.size.h > 10 {
                (geo.size.w, geo.size.h)
            } else {
                // Default dialog size: 2/3 of output
                if let Some(ref out) = output {
                    if let Some(out_geo) = self.space.output_geometry(out) {
                        (out_geo.size.w * 2 / 3, out_geo.size.h * 2 / 3)
                    } else {
                        (800, 600)
                    }
                } else {
                    (800, 600)
                }
            };

            let (cx, cy) = if let Some(ref out) = output {
                if let Some(out_geo) = self.space.output_geometry(out) {
                    (
                        out_geo.loc.x + (out_geo.size.w - win_w) / 2,
                        out_geo.loc.y + (out_geo.size.h - win_h) / 2,
                    )
                } else {
                    (geo.loc.x, geo.loc.y)
                }
            } else {
                (geo.loc.x, geo.loc.y)
            };

            info!(x = cx, y = cy, w = win_w, h = win_h, "XWayland: floating at");

            // Configure the X11 window with the computed size and position
            let rect = smithay::utils::Rectangle::new(
                (cx, cy).into(),
                (win_w, win_h).into(),
            );
            let _ = window.configure(Some(rect));

            self.space.map_element(smithay_window, (cx, cy), false);
            if let Some(w) = self.window_map.get(&id) {
                self.space.raise_element(w, true);
            }
        } else {
            self.space.map_element(smithay_window, (0, 0), false);
        }
        if let Some(ref output) = output {
            output.enter(&wl_surface);
            self.retile(output);
        }

        // Give keyboard focus
        let serial = SERIAL_COUNTER.next_serial();
        if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.set_focus(self, Some(wl_surface), serial);
        }

        // Emit IPC event
        let app_id = window.class();
        let title = window.title();
        self.emit_event(IpcEvent::WindowOpened {
            id: id.0,
            app_id: if app_id.is_empty() {
                None
            } else {
                Some(app_id.clone())
            },
            title: if title.is_empty() {
                None
            } else {
                Some(title.clone())
            },
        });

        // Apply window rules
        let rule = self
            .match_window_rule(
                if app_id.is_empty() {
                    None
                } else {
                    Some(app_id.as_str())
                },
                if title.is_empty() {
                    None
                } else {
                    Some(title.as_str())
                },
            )
            .cloned();
        if let Some(rule) = rule {
            if rule.float {
                info!(class = ?window.class(), "XWayland window rule: auto-float");
                self.floating.insert(id);
                if let Some(ref output) = self.get_focused_output() {
                    self.retile(output);
                }
            } else if rule.fullscreen {
                info!(class = ?window.class(), "XWayland window rule: auto-fullscreen");
                self.fullscreen.insert(id);
                if let Some(ref output) = self.get_focused_output() {
                    self.retile(output);
                }
            }
        }
    }

    /// Map an override-redirect X11 window (tooltip, menu, popup).
    fn map_x11_override_redirect(&mut self, window: X11Surface) {
        let Some(wl_surface) = window.wl_surface() else {
            return;
        };

        let id = Self::next_window_id();
        let smithay_window = Window::new_x11_window(window.clone());
        let geo = window.geometry();

        self.window_map.insert(id, smithay_window.clone());
        self.surface_to_id.insert(wl_surface, id);
        self.x11_surface_to_id.insert(window.window_id(), id);
        self.floating.insert(id);

        self.space
            .map_element(smithay_window.clone(), (geo.loc.x, geo.loc.y), false);
        self.space.raise_element(&smithay_window, true);
    }

    /// Unmap an override-redirect X11 window.
    fn unmap_x11_override_redirect(&mut self, window: &X11Surface) {
        if let Some(id) = self.x11_surface_to_id.remove(&window.window_id()) {
            self.floating.remove(&id);

            if let Some(wl_surface) = window.wl_surface() {
                self.surface_to_id.remove(&wl_surface);
            }

            if let Some(w) = self.window_map.remove(&id) {
                self.space.unmap_elem(&w);
            }
        }
    }
}
