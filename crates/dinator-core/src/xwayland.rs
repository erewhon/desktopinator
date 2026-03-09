//! XWayland integration — runs X11 apps inside the Wayland compositor.

use smithay::desktop::Window;
use smithay::utils::{Rectangle, SERIAL_COUNTER};
use smithay::wayland::xwayland_shell::{XWaylandShellHandler, XWaylandShellState};
use smithay::xwayland::xwm::{Reorder, ResizeEdge, XwmId, X11Window};
use smithay::xwayland::{X11Surface, X11Wm, XwmHandler};
use smithay::utils::Logical;

use tracing::info;

use dinator_ipc::IpcEvent;

use crate::state::DinatorState;

impl XwmHandler for DinatorState {
    fn xwm_state(&mut self, _xwm: XwmId) -> &mut X11Wm {
        self.x11_wm.as_mut().expect("X11Wm not initialized")
    }

    fn new_window(&mut self, _xwm: XwmId, _window: X11Surface) {
        // Window created but not yet mapped — nothing to do
    }

    fn new_override_redirect_window(&mut self, _xwm: XwmId, _window: X11Surface) {
        // Override-redirect windows (tooltips, menus, etc.) — nothing to do until mapped
    }

    fn map_window_request(&mut self, _xwm: XwmId, window: X11Surface) {
        info!(
            title = ?window.title(),
            class = ?window.class(),
            "XWayland: map window request"
        );
        window.set_mapped(true).expect("set X11 window mapped");
    }

    fn map_window_notify(&mut self, _xwm: XwmId, window: X11Surface) {
        // Window is now mapped and has a wl_surface — add to tiling
        if window.is_override_redirect() {
            // Override-redirect windows (menus, tooltips) float above everything
            self.map_x11_override_redirect(window);
            return;
        }

        let Some(wl_surface) = window.wl_surface() else {
            return;
        };

        let id = Self::next_window_id();
        let smithay_window = Window::new_x11_window(window.clone());

        self.window_order.push(id);
        self.window_map.insert(id, smithay_window.clone());
        self.surface_to_id.insert(wl_surface.clone(), id);
        self.x11_surface_to_id.insert(window.window_id(), id);
        self.window_workspace.insert(id, self.active_workspace);

        let _ = window.set_activated(true);

        // Map and retile
        let output = self.space.outputs().next().cloned();
        self.space.map_element(smithay_window, (0, 0), false);
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
            app_id: if app_id.is_empty() { None } else { Some(app_id.clone()) },
            title: if title.is_empty() { None } else { Some(title.clone()) },
        });

        // Apply window rules
        let rule = self
            .match_window_rule(
                if app_id.is_empty() { None } else { Some(app_id.as_str()) },
                if title.is_empty() { None } else { Some(title.as_str()) },
            )
            .cloned();
        if let Some(rule) = rule {
            if rule.float {
                info!(class = ?window.class(), "XWayland window rule: auto-float");
                self.floating.insert(id);
                let output = self.space.outputs().next().cloned();
                if let Some(ref output) = output {
                    self.retile(output);
                }
            } else if rule.fullscreen {
                info!(class = ?window.class(), "XWayland window rule: auto-fullscreen");
                self.fullscreen.insert(id);
                let output = self.space.outputs().next().cloned();
                if let Some(ref output) = output {
                    self.retile(output);
                }
            }
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

        if let Some(id) = self.x11_surface_to_id.remove(&window.window_id()) {
            self.emit_event(IpcEvent::WindowClosed { id: id.0 });

            self.window_order.retain(|w| *w != id);
            self.floating.remove(&id);
            self.fullscreen.remove(&id);
            self.window_workspace.remove(&id);

            for order in self.workspace_order.values_mut() {
                order.retain(|w| *w != id);
            }
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

            let output = self.space.outputs().next().cloned();
            if let Some(output) = output {
                self.retile(&output);
            }

            // Focus next window
            if let Some(&next_id) = self.window_order.last() {
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
        // Clean up just in case unmapped_window wasn't called
        if let Some(id) = self.x11_surface_to_id.remove(&window.window_id()) {
            self.window_order.retain(|w| *w != id);
            self.floating.remove(&id);
            self.fullscreen.remove(&id);
            self.window_workspace.remove(&id);

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
        // For tiled windows, ignore position/size requests — tiling controls layout.
        // For floating/override-redirect, honor them.
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
            // Tiled: just acknowledge with current geometry
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
        // Nothing to do
    }

    fn resize_request(
        &mut self,
        _xwm: XwmId,
        _window: X11Surface,
        _button: u32,
        _resize_edge: ResizeEdge,
    ) {
        // Tiling WM — don't support interactive resize
    }

    fn move_request(&mut self, _xwm: XwmId, _window: X11Surface, _button: u32) {
        // Tiling WM — don't support interactive move
    }

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
            let output = self.space.outputs().next().cloned();
            if let Some(output) = output {
                self.retile(&output);
            }
        }
    }

    fn unfullscreen_request(&mut self, _xwm: XwmId, window: X11Surface) {
        if let Some(&id) = self.x11_surface_to_id.get(&window.window_id()) {
            self.fullscreen.remove(&id);
            let _ = window.set_fullscreen(false);
            let output = self.space.outputs().next().cloned();
            if let Some(output) = output {
                self.retile(&output);
            }
        }
    }

    fn minimize_request(&mut self, _xwm: XwmId, _window: X11Surface) {
        // No minimize support in tiling WM
    }
}

impl XWaylandShellHandler for DinatorState {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.xwayland_shell_state
    }
}

smithay::delegate_xwayland_shell!(DinatorState);

impl DinatorState {
    /// Map an override-redirect X11 window (tooltip, menu, popup).
    /// These float above everything and aren't tiled.
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
        // Don't add to window_order — override-redirect windows aren't tiled
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
