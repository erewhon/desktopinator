use smithay::backend::input::{
    Axis, ButtonState, InputBackend, InputEvent, KeyState, KeyboardKeyEvent, PointerAxisEvent,
    PointerButtonEvent, PointerMotionAbsoluteEvent,
};
use smithay::desktop::WindowSurfaceType;
use smithay::input::keyboard::keysyms;
use smithay::input::keyboard::FilterResult;
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent};
use smithay::utils::SERIAL_COUNTER;

use tracing::info;

use crate::state::DinatorState;

enum KeyAction {
    LaunchTerminal,
    CloseWindow,
    FocusNext,
    FocusPrev,
    SwapMaster,
    Quit,
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

        let keyboard = self.seat.get_keyboard().unwrap();

        let action = keyboard.input::<Option<KeyAction>, _>(
            self,
            keycode,
            press_state,
            serial,
            time,
            |_state, modifiers, keysym| {
                // Use Alt as the compositor modifier. Super conflicts with
                // host compositors (KDE, GNOME) that grab it for their own use,
                // causing stuck modifier state in nested sessions.
                if !modifiers.alt {
                    return FilterResult::Forward;
                }

                let sym = keysym.modified_sym();
                match sym.raw() {
                    keysyms::KEY_Return | keysyms::KEY_j | keysyms::KEY_k
                    | keysyms::KEY_q | keysyms::KEY_Q | keysyms::KEY_space => {
                        if press_state == KeyState::Pressed {
                            let action = match sym.raw() {
                                keysyms::KEY_Return => KeyAction::LaunchTerminal,
                                keysyms::KEY_q => KeyAction::CloseWindow,
                                keysyms::KEY_Q => KeyAction::Quit,
                                keysyms::KEY_j => KeyAction::FocusNext,
                                keysyms::KEY_k => KeyAction::FocusPrev,
                                keysyms::KEY_space => KeyAction::SwapMaster,
                                _ => unreachable!(),
                            };
                            FilterResult::Intercept(Some(action))
                        } else {
                            // Swallow release too so client doesn't see orphaned release
                            FilterResult::Intercept(None)
                        }
                    }
                    _ => FilterResult::Forward,
                }
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
                KeyAction::CloseWindow => {
                    info!("keybinding: close window");
                    self.close_focused_window();
                }
                KeyAction::FocusNext => self.focus_next(),
                KeyAction::FocusPrev => self.focus_prev(),
                KeyAction::SwapMaster => self.swap_master(),
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
        let output = self.space.outputs().next().cloned();
        let Some(output) = output else { return };
        let Some(output_geo) = self.space.output_geometry(&output) else {
            return;
        };

        let pos = event.position_transformed(output_geo.size);
        let serial = SERIAL_COUNTER.next_serial();

        let pointer = self.seat.get_pointer().unwrap();

        let under = self.space.element_under(pos);
        let surface_under = under.and_then(|(window, loc)| {
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
        let pointer = self.seat.get_pointer().unwrap();

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
                    let keyboard = self.seat.get_keyboard().unwrap();
                    keyboard.set_focus(self, Some(toplevel.wl_surface().clone()), serial);
                }
            }
        }
    }

    fn handle_pointer_axis<B: InputBackend>(&mut self, event: impl PointerAxisEvent<B>) {
        let pointer = self.seat.get_pointer().unwrap();

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
