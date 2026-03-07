use smithay::backend::input::{
    Axis, ButtonState, InputBackend, InputEvent, KeyboardKeyEvent, PointerAxisEvent,
    PointerButtonEvent, PointerMotionAbsoluteEvent,
};
use smithay::desktop::WindowSurfaceType;
use smithay::input::keyboard::FilterResult;
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent};
use smithay::utils::SERIAL_COUNTER;

use tracing::info;

use crate::state::DinatorState;

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
        info!(keycode = ?event.key_code(), "keyboard event");
        let serial = SERIAL_COUNTER.next_serial();
        let time = event.time_msec();
        let keycode = event.key_code();
        let state = event.state();

        let keyboard = self.seat.get_keyboard().unwrap();

        keyboard.input::<(), _>(
            self,
            keycode,
            state,
            serial,
            time,
            |_state, _modifiers, _keysym| FilterResult::Forward,
        );
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
            let result = window.surface_under(rel, WindowSurfaceType::ALL);
            if result.is_none() {
                let geo = window.geometry();
                info!(
                    px = pos.x, py = pos.y,
                    wloc_x = loc.x, wloc_y = loc.y,
                    rel_x = rel.0, rel_y = rel.1,
                    geo_x = geo.loc.x, geo_y = geo.loc.y,
                    geo_w = geo.size.w, geo_h = geo.size.h,
                    "element found but surface_under returned None"
                );
            }
            result.map(|(s, offset)| {
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
            info!(x = loc.x, y = loc.y, "pointer click");
            if let Some((window, _)) = self.space.element_under(loc) {
                info!("click hit window");
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
