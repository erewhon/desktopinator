use smithay::backend::input::{
    Axis, ButtonState, InputBackend, InputEvent, KeyState, KeyboardKeyEvent, PointerAxisEvent,
    PointerButtonEvent, PointerMotionAbsoluteEvent,
};
use smithay::desktop::{WindowSurfaceType, layer_map_for_output};
use smithay::wayland::shell::wlr_layer::Layer as WlrLayer;
use smithay::input::keyboard::keysyms;
use smithay::input::keyboard::FilterResult;
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent};
use smithay::utils::SERIAL_COUNTER;

use tracing::info;

use crate::state::DinatorState;

enum KeyAction {
    LaunchTerminal,
    LaunchLauncher,
    CloseWindow,
    FocusNext,
    FocusPrev,
    SwapMaster,
    Quit,
    PluginCallback(String),
    SwitchWorkspace(usize),
    MoveToWorkspace(usize),
}

impl DinatorState {
    pub fn handle_input_event<B: InputBackend>(&mut self, event: InputEvent<B>) {
        match event {
            InputEvent::Keyboard { event } => self.handle_keyboard(event),
            InputEvent::PointerMotionAbsolute { event } => {
                self.handle_pointer_motion_absolute(event)
            }
            InputEvent::PointerButton { event } => self.handle_pointer_button(event),
            InputEvent::PointerAxis { event } => self.handle_pointer_axis(event),
            _ => {}
        }
    }

    fn handle_keyboard<B: InputBackend>(&mut self, event: impl KeyboardKeyEvent<B>) {
        let serial = SERIAL_COUNTER.next_serial();
        let time = event.time_msec();
        let keycode = event.key_code();
        let press_state = event.state();

        let Some(keyboard) = self.seat.get_keyboard() else { return };

        let plugin_bindings = self.plugin_keybindings.clone();
        let action = keyboard.input::<Option<KeyAction>, _>(
            self,
            keycode,
            press_state,
            serial,
            time,
            |_state, modifiers, keysym| {
                let sym = keysym.modified_sym();

                // Use Alt as the compositor modifier. Super conflicts with
                // host compositors (KDE, GNOME) that grab it for their own use,
                // causing stuck modifier state in nested sessions.
                if modifiers.alt {
                    // Workspace switching: Alt+1-9
                    let ws = keysym_to_workspace(sym.raw());
                    if let Some(n) = ws {
                        if press_state == KeyState::Pressed {
                            let action = if modifiers.shift {
                                KeyAction::MoveToWorkspace(n)
                            } else {
                                KeyAction::SwitchWorkspace(n)
                            };
                            return FilterResult::Intercept(Some(action));
                        } else {
                            return FilterResult::Intercept(None);
                        }
                    }

                    match sym.raw() {
                        keysyms::KEY_Return | keysyms::KEY_d | keysyms::KEY_j | keysyms::KEY_k
                        | keysyms::KEY_q | keysyms::KEY_Q | keysyms::KEY_space => {
                            if press_state == KeyState::Pressed {
                                let action = match sym.raw() {
                                    keysyms::KEY_Return => KeyAction::LaunchTerminal,
                                    keysyms::KEY_d => KeyAction::LaunchLauncher,
                                    keysyms::KEY_q => KeyAction::CloseWindow,
                                    keysyms::KEY_Q => KeyAction::Quit,
                                    keysyms::KEY_j => KeyAction::FocusNext,
                                    keysyms::KEY_k => KeyAction::FocusPrev,
                                    keysyms::KEY_space => KeyAction::SwapMaster,
                                    _ => unreachable!(),
                                };
                                return FilterResult::Intercept(Some(action));
                            } else {
                                return FilterResult::Intercept(None);
                            }
                        }
                        _ => {}
                    }
                }

                // Check plugin-registered keybindings
                for (ks, alt, ctrl, shift, logo, ref cb_id) in &plugin_bindings {
                    if sym.raw() == *ks
                        && modifiers.alt == *alt
                        && modifiers.ctrl == *ctrl
                        && modifiers.shift == *shift
                        && modifiers.logo == *logo
                    {
                        if press_state == KeyState::Pressed {
                            return FilterResult::Intercept(Some(
                                KeyAction::PluginCallback(cb_id.clone()),
                            ));
                        } else {
                            return FilterResult::Intercept(None);
                        }
                    }
                }

                FilterResult::Forward
            },
        );

        if let Some(Some(action)) = action {
            match action {
                KeyAction::LaunchTerminal => {
                    info!("keybinding: launch terminal");
                    if let Err(e) = std::process::Command::new("foot").spawn() {
                        info!(error = %e, "failed to launch foot");
                    }
                }
                KeyAction::LaunchLauncher => {
                    info!("keybinding: launch fuzzel");
                    if let Err(e) = std::process::Command::new("fuzzel").spawn() {
                        info!(error = %e, "failed to launch fuzzel");
                    }
                }
                KeyAction::CloseWindow => {
                    info!("keybinding: close window");
                    self.close_focused_window();
                }
                KeyAction::FocusNext => self.focus_next(),
                KeyAction::FocusPrev => self.focus_prev(),
                KeyAction::SwapMaster => self.swap_master(),
                KeyAction::PluginCallback(ref callback_id) => {
                    info!(callback = %callback_id, "plugin keybinding");
                    if let Some(ref mut runtime) = self.plugin_runtime {
                        runtime.invoke_callback(callback_id);
                    }
                    self.execute_plugin_actions();
                }
                KeyAction::SwitchWorkspace(n) => self.switch_workspace(n),
                KeyAction::MoveToWorkspace(n) => self.move_to_workspace(n),
                KeyAction::Quit => {
                    info!("keybinding: quit");
                    self.loop_signal.stop();
                }
            }
        }
    }

    fn handle_pointer_motion_absolute<B: InputBackend>(
        &mut self,
        event: impl PointerMotionAbsoluteEvent<B>,
    ) {
        let output = self.get_focused_output();
        let Some(output) = output else { return };
        let Some(output_geo) = self.space.output_geometry(&output) else {
            return;
        };

        let pos = event.position_transformed(output_geo.size);
        let serial = SERIAL_COUNTER.next_serial();

        let Some(pointer) = self.seat.get_pointer() else { return };

        // Check layer surfaces first (Overlay and Top layers take priority)
        let layer_map = layer_map_for_output(&output);
        let mut surface_under = None;
        for layer in [WlrLayer::Overlay, WlrLayer::Top] {
            if let Some(layer_surface) = layer_map.layer_under(layer, pos) {
                if let Some(geo) = layer_map.layer_geometry(layer_surface) {
                    let rel = (pos.x - geo.loc.x as f64, pos.y - geo.loc.y as f64);
                    if let Some((s, offset)) = layer_surface.surface_under(rel, WindowSurfaceType::ALL) {
                        surface_under = Some((
                            s,
                            smithay::utils::Point::<f64, smithay::utils::Logical>::from((
                                geo.loc.x as f64 + offset.x as f64,
                                geo.loc.y as f64 + offset.y as f64,
                            )),
                        ));
                        break;
                    }
                }
            }
        }
        drop(layer_map);

        // Fall back to space windows
        if surface_under.is_none() {
            let under = self.space.element_under(pos);
            surface_under = under.and_then(|(window, loc)| {
                let rel = (pos.x - loc.x as f64, pos.y - loc.y as f64);
                window.surface_under(rel, WindowSurfaceType::ALL)
                    .map(|(s, offset)| {
                        (
                            s,
                            smithay::utils::Point::<f64, smithay::utils::Logical>::from((
                                loc.x as f64 + offset.x as f64,
                                loc.y as f64 + offset.y as f64,
                            )),
                        )
                    })
            });
        }

        pointer.motion(
            self,
            surface_under,
            &MotionEvent {
                location: (pos.x, pos.y).into(),
                serial,
                time: event.time_msec(),
            },
        );
        pointer.frame(self);
    }

    fn handle_pointer_button<B: InputBackend>(&mut self, event: impl PointerButtonEvent<B>) {
        let serial = SERIAL_COUNTER.next_serial();
        let Some(pointer) = self.seat.get_pointer() else { return };

        pointer.button(
            self,
            &ButtonEvent {
                button: event.button_code(),
                state: event.state(),
                serial,
                time: event.time_msec(),
            },
        );

        pointer.frame(self);

        // Click to focus
        if event.state() == ButtonState::Pressed {
            let loc = pointer.current_location();
            if let Some((window, _)) = self.space.element_under(loc) {
                let window = window.clone();
                self.space.raise_element(&window, true);
                if let Some(toplevel) = window.toplevel() {
                    let Some(keyboard) = self.seat.get_keyboard() else { return };
                    keyboard.set_focus(self, Some(toplevel.wl_surface().clone()), serial);
                }
            }
        }
    }

    fn handle_pointer_axis<B: InputBackend>(&mut self, event: impl PointerAxisEvent<B>) {
        let Some(pointer) = self.seat.get_pointer() else { return };

        let mut frame = AxisFrame::new(event.time_msec());

        if let Some(amount) = event.amount(Axis::Horizontal) {
            frame = frame.value(Axis::Horizontal, amount);
        }
        if let Some(amount) = event.amount(Axis::Vertical) {
            frame = frame.value(Axis::Vertical, amount);
        }
        frame = frame.source(event.source());

        pointer.axis(self, frame);
        pointer.frame(self);
    }
}

/// Map number keysyms (both shifted and unshifted) to workspace numbers 1-9.
fn keysym_to_workspace(sym: u32) -> Option<usize> {
    match sym {
        keysyms::KEY_1 | keysyms::KEY_exclam => Some(1),
        keysyms::KEY_2 | keysyms::KEY_at => Some(2),
        keysyms::KEY_3 | keysyms::KEY_numbersign => Some(3),
        keysyms::KEY_4 | keysyms::KEY_dollar => Some(4),
        keysyms::KEY_5 | keysyms::KEY_percent => Some(5),
        keysyms::KEY_6 | keysyms::KEY_asciicircum => Some(6),
        keysyms::KEY_7 | keysyms::KEY_ampersand => Some(7),
        keysyms::KEY_8 | keysyms::KEY_asterisk => Some(8),
        keysyms::KEY_9 | keysyms::KEY_parenleft => Some(9),
        _ => None,
    }
}
