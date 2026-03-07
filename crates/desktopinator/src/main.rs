use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::solid::{SolidColorBuffer, SolidColorRenderElement};
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::element::RenderElement;
use smithay::backend::renderer::gles::{GlesFrame, GlesRenderer};
use smithay::desktop::space::SpaceRenderElements;
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{EventLoop, Interest, PostAction};
use smithay::reexports::wayland_server::{Display, ListeningSocket};
use smithay::utils::{Physical, Point, Rectangle, Transform};
use tracing::info;

use dinator_core::DinatorState;

type SpaceElements = SpaceRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>;

enum OutputRenderElements {
    Space(SpaceElements),
    Border(SolidColorRenderElement),
}

impl smithay::backend::renderer::element::Element for OutputRenderElements {
    fn id(&self) -> &smithay::backend::renderer::element::Id {
        match self {
            Self::Space(e) => e.id(),
            Self::Border(e) => e.id(),
        }
    }
    fn current_commit(&self) -> smithay::backend::renderer::utils::CommitCounter {
        match self {
            Self::Space(e) => e.current_commit(),
            Self::Border(e) => e.current_commit(),
        }
    }
    fn geometry(&self, scale: smithay::utils::Scale<f64>) -> Rectangle<i32, Physical> {
        match self {
            Self::Space(e) => e.geometry(scale),
            Self::Border(e) => e.geometry(scale),
        }
    }
    fn src(&self) -> Rectangle<f64, smithay::utils::Buffer> {
        match self {
            Self::Space(e) => e.src(),
            Self::Border(e) => e.src(),
        }
    }
}

impl RenderElement<GlesRenderer> for OutputRenderElements {
    fn draw<'a>(
        &self,
        frame: &mut GlesFrame<'a, '_>,
        src: Rectangle<f64, smithay::utils::Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
    ) -> Result<(), smithay::backend::renderer::gles::GlesError> {
        match self {
            Self::Space(e) => e.draw(frame, src, dst, damage, opaque_regions),
            Self::Border(e) => {
                <SolidColorRenderElement as RenderElement<GlesRenderer>>::draw(
                    e,
                    frame,
                    src,
                    dst,
                    damage,
                    opaque_regions,
                )
            }
        }
    }
}

/// Build the render element list: space elements + focus border.
fn build_render_elements(
    renderer: &mut GlesRenderer,
    state: &DinatorState,
    output: &Output,
) -> Option<Vec<OutputRenderElements>> {
    let space_elements: Vec<SpaceElements> =
        match state.space.render_elements_for_output(renderer, output, 1.0) {
            Ok(elements) => elements,
            Err(_) => return None,
        };

    let mut elements: Vec<OutputRenderElements> = space_elements
        .into_iter()
        .map(OutputRenderElements::Space)
        .collect();

    // Add focus indicator border behind the focused window
    let border_width = 2;
    let border_color = [0.4f32, 0.6, 0.9, 1.0]; // blue
    if let Some(focused) = state.focused_window() {
        if let Some(geo) = state.space.element_geometry(focused) {
            let buf = SolidColorBuffer::new(
                (geo.size.w + 2 * border_width, geo.size.h + 2 * border_width),
                border_color,
            );
            let loc: Point<i32, Physical> =
                (geo.loc.x - border_width, geo.loc.y - border_width).into();
            elements.push(OutputRenderElements::Border(
                SolidColorRenderElement::from_buffer(&buf, loc, 1.0, 1.0, Kind::Unspecified),
            ));
        }
    }

    Some(elements)
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let headless = std::env::args().any(|a| a == "--headless");

    if headless {
        info!("starting desktopinator (headless)");
        run_headless()
    } else {
        info!("starting desktopinator (winit)");
        run_winit()
    }
}

fn run_winit() -> anyhow::Result<()> {
    use smithay::backend::winit::{self, WinitEvent};

    let (mut backend, winit_evt_loop) = winit::init::<GlesRenderer>()
        .map_err(|e| anyhow::anyhow!("winit init failed: {e:?}"))?;

    info!("winit backend initialized");

    let mut event_loop: EventLoop<DinatorState> =
        EventLoop::try_new().context("failed to create event loop")?;

    let display = Display::new().context("failed to create wayland display")?;

    let listening_socket =
        ListeningSocket::bind_auto("wayland", 0..33).context("failed to bind wayland socket")?;
    let socket_name = listening_socket
        .socket_name()
        .context("no socket name")?
        .to_string_lossy()
        .into_owned();
    info!(socket = %socket_name, "wayland socket listening");

    std::env::set_var("WAYLAND_DISPLAY", &socket_name);

    let size = backend.window_size();
    let mode = Mode {
        size,
        refresh: 60_000,
    };
    let output = Output::new(
        "winit-0".into(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "desktopinator".into(),
            model: "winit".into(),
        },
    );
    output.change_current_state(
        Some(mode),
        Some(Transform::Flipped180),
        None,
        Some((0, 0).into()),
    );
    output.set_preferred(mode);

    let display_handle = display.handle();
    let mut state = DinatorState::new(display, event_loop.get_signal());

    output.create_global::<DinatorState>(&display_handle);
    state.space.map_output(&output, (0, 0));
    state.seat.add_keyboard(Default::default(), 200, 25)?;
    state.seat.add_pointer();

    event_loop
        .handle()
        .insert_source(
            Generic::new(listening_socket, Interest::READ, calloop::Mode::Level),
            |_, socket, state| {
                if let Some(stream) = socket.accept()? {
                    let client_state = Arc::new(dinator_core::ClientState::default());
                    state
                        .display_handle
                        .insert_client(stream, client_state)
                        .unwrap();
                }
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| anyhow::anyhow!("failed to insert socket source: {e}"))?;

    let mut damage_tracker = OutputDamageTracker::from_output(&output);
    let mut last_output_size = backend.window_size();

    event_loop
        .handle()
        .insert_source(winit_evt_loop, move |event, _, state| match event {
            WinitEvent::Resized { size, .. } => {
                if size != last_output_size {
                    last_output_size = size;
                    let mode = Mode {
                        size,
                        refresh: 60_000,
                    };
                    output.change_current_state(Some(mode), None, None, None);
                    state.retile(&output);
                }
            }
            WinitEvent::Input(input_event) => {
                state.handle_input_event(input_event);
            }
            WinitEvent::Redraw => {
                let size = backend.window_size();
                let damage = Rectangle::new((0, 0).into(), size);

                {
                    let Ok((renderer, mut framebuffer)) = backend.bind() else {
                        return;
                    };

                    let Some(elements) = build_render_elements(renderer, state, &output) else {
                        return;
                    };

                    let _ = damage_tracker.render_output(
                        renderer,
                        &mut framebuffer,
                        0,
                        &elements,
                        [0.1, 0.1, 0.1, 1.0],
                    );
                }

                backend.submit(Some(&[damage])).unwrap();

                state.space.elements().for_each(|window| {
                    window.send_frame(&output, state.start_time.elapsed(), None, |_, _| {
                        Some(output.clone())
                    });
                });

                state.space.refresh();
                backend.window().request_redraw();
            }
            WinitEvent::CloseRequested => {
                state.loop_signal.stop();
            }
            _ => {}
        })
        .map_err(|e| anyhow::anyhow!("failed to insert winit source: {e}"))?;

    info!("entering event loop -- launch clients with WAYLAND_DISPLAY={socket_name}");

    event_loop
        .run(Duration::from_millis(16), &mut state, |state| {
            let display_ptr = &mut state.display as *mut Display<DinatorState>;
            unsafe { &mut *display_ptr }.dispatch_clients(state).unwrap();
            state.display.flush_clients().unwrap();
        })
        .context("event loop error")?;

    info!("shutting down");
    Ok(())
}

/// VNC input event sent from the VNC server thread to the compositor event loop.
enum VncInputEvent {
    PointerMove { x: u16, y: u16, button_mask: u8 },
    Key { keysym: u32, pressed: bool },
}

fn run_headless() -> anyhow::Result<()> {
    use smithay::backend::allocator::Fourcc;
    use smithay::backend::egl::context::EGLContext;
    use smithay::backend::egl::native::EGLSurfacelessDisplay;
    use smithay::backend::egl::EGLDisplay;
    use smithay::backend::renderer::gles::GlesRenderbuffer;
    use smithay::backend::renderer::{Bind, ExportMem, Offscreen};
    use smithay::backend::input::KeyState;
    use smithay::input::keyboard::{keysyms, FilterResult};
    use smithay::input::pointer::{ButtonEvent, MotionEvent};
    use smithay::reexports::calloop::channel::{self, Channel};
    use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
    use smithay::utils::{Size, SERIAL_COUNTER};

    use rustvncserver::server::ServerEvent;
    use rustvncserver::VncServer;

    let width: u16 = 1920;
    let height: u16 = 1080;
    let vnc_port: u16 = 5900;

    // Create EGL display without a window
    let egl_display = unsafe { EGLDisplay::new(EGLSurfacelessDisplay) }
        .context("failed to create EGL surfaceless display")?;
    let egl_context = EGLContext::new(&egl_display).context("failed to create EGL context")?;
    let mut renderer = unsafe { GlesRenderer::new(egl_context) }
        .map_err(|e| anyhow::anyhow!("failed to create GlesRenderer: {e:?}"))?;

    info!("headless GlesRenderer initialized");

    let mut renderbuffer: GlesRenderbuffer = renderer
        .create_buffer(Fourcc::Abgr8888, Size::from((width as i32, height as i32)))
        .map_err(|e| anyhow::anyhow!("failed to create renderbuffer: {e:?}"))?;

    info!(width, height, "offscreen renderbuffer created");

    // Create VNC server
    let (vnc_server, mut vnc_event_rx) = VncServer::new(
        width,
        height,
        "desktopinator".to_string(),
        None, // no password
    );
    let vnc_framebuffer = vnc_server.framebuffer().clone();

    // Create a calloop channel to receive VNC input events in the compositor event loop
    let (vnc_input_tx, vnc_input_rx): (channel::Sender<VncInputEvent>, Channel<VncInputEvent>) =
        channel::channel();

    // Create a channel to send pixel data to the VNC server's tokio runtime
    let (pixel_tx, mut pixel_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(2);

    // Start VNC server + event handler on a tokio runtime in a background thread
    let vnc_input_tx_clone = vnc_input_tx.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
        rt.block_on(async {
            // Handle pixel updates from the compositor
            let fb = vnc_framebuffer.clone();
            tokio::spawn(async move {
                while let Some(pixels) = pixel_rx.recv().await {
                    if let Err(e) = fb.update_from_slice(&pixels).await {
                        tracing::error!("update_from_slice failed: {e}");
                    }
                }
            });

            // Handle VNC events (client input) in a background task
            let tx = vnc_input_tx_clone;
            tokio::spawn(async move {
                while let Some(event) = vnc_event_rx.recv().await {
                    match event {
                        ServerEvent::ClientConnected { client_id } => {
                            info!(client_id, "VNC client connected");
                        }
                        ServerEvent::ClientDisconnected { client_id } => {
                            info!(client_id, "VNC client disconnected");
                        }
                        ServerEvent::PointerMove {
                            x,
                            y,
                            button_mask,
                            ..
                        } => {
                            let _ = tx.send(VncInputEvent::PointerMove {
                                x,
                                y,
                                button_mask,
                            });
                        }
                        ServerEvent::KeyPress { key, down, .. } => {
                            let _ = tx.send(VncInputEvent::Key {
                                keysym: key,
                                pressed: down,
                            });
                        }
                        _ => {}
                    }
                }
            });

            if let Err(e) = vnc_server.listen(vnc_port).await {
                tracing::error!(error = %e, "VNC server error");
            }
        });
    });

    info!(port = vnc_port, "VNC server started");

    let mut event_loop: EventLoop<DinatorState> =
        EventLoop::try_new().context("failed to create event loop")?;

    let display = Display::new().context("failed to create wayland display")?;

    let listening_socket =
        ListeningSocket::bind_auto("wayland", 0..33).context("failed to bind wayland socket")?;
    let socket_name = listening_socket
        .socket_name()
        .context("no socket name")?
        .to_string_lossy()
        .into_owned();
    info!(socket = %socket_name, "wayland socket listening");

    std::env::set_var("WAYLAND_DISPLAY", &socket_name);

    let mode = Mode {
        size: (width as i32, height as i32).into(),
        refresh: 60_000,
    };
    let output = Output::new(
        "headless-0".into(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "desktopinator".into(),
            model: "headless".into(),
        },
    );
    output.change_current_state(
        Some(mode),
        Some(Transform::Normal),
        None,
        Some((0, 0).into()),
    );
    output.set_preferred(mode);

    let display_handle = display.handle();
    let mut state = DinatorState::new(display, event_loop.get_signal());

    output.create_global::<DinatorState>(&display_handle);
    state.space.map_output(&output, (0, 0));
    state.seat.add_keyboard(Default::default(), 200, 25)?;
    state.seat.add_pointer();

    // Accept new Wayland clients
    event_loop
        .handle()
        .insert_source(
            Generic::new(listening_socket, Interest::READ, calloop::Mode::Level),
            |_, socket, state| {
                if let Some(stream) = socket.accept()? {
                    let client_state = Arc::new(dinator_core::ClientState::default());
                    state
                        .display_handle
                        .insert_client(stream, client_state)
                        .unwrap();
                }
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| anyhow::anyhow!("failed to insert socket source: {e}"))?;

    // Handle VNC input events in the compositor event loop
    let mut last_button_mask: u8 = 0;
    let mut pressed_keys: std::collections::HashSet<u32> = std::collections::HashSet::new();
    event_loop
        .handle()
        .insert_source(vnc_input_rx, move |event, _, state| {
            let channel::Event::Msg(event) = event else {
                return;
            };
            match event {
                VncInputEvent::PointerMove {
                    x,
                    y,
                    button_mask,
                } => {
                    let pointer = state.seat.get_pointer().unwrap();
                    let serial = SERIAL_COUNTER.next_serial();

                    // Pointer motion
                    let pos = (x as f64, y as f64);
                    let under = state.space.element_under(pos);
                    let surface_under = under.and_then(|(window, loc)| {
                        use smithay::desktop::WindowSurfaceType;
                        let rel = (pos.0 - loc.x as f64, pos.1 - loc.y as f64);
                        window
                            .surface_under(rel, WindowSurfaceType::ALL)
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
                        state,
                        surface_under,
                        &MotionEvent {
                            location: pos.into(),
                            serial,
                            time: 0,
                        },
                    );
                    pointer.frame(state);

                    // Button changes
                    let changed = button_mask ^ last_button_mask;
                    for bit in 0..8u8 {
                        if changed & (1 << bit) != 0 {
                            let pressed = button_mask & (1 << bit) != 0;
                            // VNC button mapping: 0=left, 1=middle, 2=right
                            // Linux button codes: 272=left(BTN_LEFT), 274=middle, 273=right
                            let button_code = match bit {
                                0 => 0x110, // BTN_LEFT (272)
                                1 => 0x112, // BTN_MIDDLE (274)
                                2 => 0x111, // BTN_RIGHT (273)
                                _ => continue,
                            };
                            let btn_state = if pressed {
                                smithay::backend::input::ButtonState::Pressed
                            } else {
                                smithay::backend::input::ButtonState::Released
                            };
                            pointer.button(
                                state,
                                &ButtonEvent {
                                    button: button_code,
                                    state: btn_state,
                                    serial: SERIAL_COUNTER.next_serial(),
                                    time: 0,
                                },
                            );
                            pointer.frame(state);

                            // Click to focus
                            if pressed {
                                let loc = pointer.current_location();
                                if let Some((window, _)) = state.space.element_under(loc) {
                                    let window = window.clone();
                                    state.space.raise_element(&window, true);
                                    if let Some(toplevel) = window.toplevel() {
                                        let keyboard = state.seat.get_keyboard().unwrap();
                                        keyboard.set_focus(
                                            state,
                                            Some(toplevel.wl_surface().clone()),
                                            SERIAL_COUNTER.next_serial(),
                                        );
                                    }
                                }
                            }
                        }
                    }
                    last_button_mask = button_mask;
                }
                VncInputEvent::Key { keysym, pressed } => {
                    let keyboard = state.seat.get_keyboard().unwrap();
                    let serial = SERIAL_COUNTER.next_serial();

                    // Convert X11 keysym to XKB keycode
                    if let Some(keycode) = xkeysym_to_xkb_keycode(keysym) {
                        // Filter out VNC client-side key repeats (duplicate presses)
                        // The compositor handles its own key repeat via XKB
                        if pressed {
                            if !pressed_keys.insert(keycode) {
                                return; // already pressed, skip repeat
                            }
                        } else {
                            pressed_keys.remove(&keycode);
                        }

                        let key_state = if pressed {
                            KeyState::Pressed
                        } else {
                            KeyState::Released
                        };

                        // Check for compositor keybindings (Alt+key)
                        let action = keyboard.input::<Option<KeyAction>, _>(
                            state,
                            keycode.into(),
                            key_state,
                            serial,
                            0,
                            |_state, modifiers, ksym| {
                                if !modifiers.alt {
                                    return FilterResult::Forward;
                                }
                                let sym = ksym.modified_sym();
                                match sym.raw() {
                                    keysyms::KEY_Return | keysyms::KEY_j | keysyms::KEY_k
                                    | keysyms::KEY_q | keysyms::KEY_Q | keysyms::KEY_space => {
                                        if key_state == KeyState::Pressed {
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
                                    state.close_focused_window();
                                }
                                KeyAction::FocusNext => state.focus_next(),
                                KeyAction::FocusPrev => state.focus_prev(),
                                KeyAction::SwapMaster => state.swap_master(),
                                KeyAction::Quit => {
                                    info!("keybinding: quit");
                                    state.loop_signal.stop();
                                }
                            }
                        }
                    }
                }
            }
        })
        .map_err(|e| anyhow::anyhow!("failed to insert VNC input source: {e}"))?;

    let mut damage_tracker = OutputDamageTracker::from_output(&output);
    let output_for_render = output.clone();

    // Timer-based redraw at ~60fps
    let timer = Timer::immediate();
    event_loop
        .handle()
        .insert_source(timer, move |_, _, state| {
            let output = &output_for_render;

            // Render to offscreen buffer
            {
                let mut target = renderer
                    .bind(&mut renderbuffer)
                    .expect("failed to bind renderbuffer");

                if let Some(elements) = build_render_elements(&mut renderer, state, output) {
                    let _ = damage_tracker.render_output(
                        &mut renderer,
                        &mut target,
                        0,
                        &elements,
                        [0.1, 0.1, 0.1, 1.0],
                    );
                }

                // Export pixels to VNC framebuffer
                let region = Rectangle::from_size(
                    Size::from((width as i32, height as i32)),
                );
                if let Ok(mapping) = renderer.copy_framebuffer(&target, region, Fourcc::Abgr8888) {
                    if let Ok(pixels) = renderer.map_texture(&mapping) {
                        let _ = pixel_tx.try_send(pixels.to_vec());
                    }
                }
            }

            // Send frame callbacks to clients
            state.space.elements().for_each(|window| {
                window.send_frame(output, state.start_time.elapsed(), None, |_, _| {
                    Some(output.clone())
                });
            });

            state.space.refresh();

            TimeoutAction::ToDuration(Duration::from_millis(16))
        })
        .map_err(|e| anyhow::anyhow!("failed to insert timer source: {e}"))?;

    info!(
        vnc_port,
        "entering event loop (headless) -- VNC on :{vnc_port}, launch clients with WAYLAND_DISPLAY={socket_name}"
    );

    event_loop
        .run(Duration::from_millis(16), &mut state, |state| {
            let display_ptr = &mut state.display as *mut Display<DinatorState>;
            unsafe { &mut *display_ptr }
                .dispatch_clients(state)
                .unwrap();
            state.display.flush_clients().unwrap();
        })
        .context("event loop error")?;

    info!("shutting down");
    Ok(())
}

enum KeyAction {
    LaunchTerminal,
    CloseWindow,
    FocusNext,
    FocusPrev,
    SwapMaster,
    Quit,
}

/// Convert an X11 keysym to an evdev keycode.
/// VNC sends X11 keysyms; smithay's keyboard.input() expects evdev keycodes.
fn xkeysym_to_xkb_keycode(keysym: u32) -> Option<u32> {
    // Convert X11 keysym to XKB keycode (evdev keycode + 8)
    use smithay::input::keyboard::keysyms;
    let evdev = match keysym {
        // Letters (lowercase and uppercase map to the same evdev code)
        keysyms::KEY_a | keysyms::KEY_A => 30,
        keysyms::KEY_b | keysyms::KEY_B => 48,
        keysyms::KEY_c | keysyms::KEY_C => 46,
        keysyms::KEY_d | keysyms::KEY_D => 32,
        keysyms::KEY_e | keysyms::KEY_E => 18,
        keysyms::KEY_f | keysyms::KEY_F => 33,
        keysyms::KEY_g | keysyms::KEY_G => 34,
        keysyms::KEY_h | keysyms::KEY_H => 35,
        keysyms::KEY_i | keysyms::KEY_I => 23,
        keysyms::KEY_j | keysyms::KEY_J => 36,
        keysyms::KEY_k | keysyms::KEY_K => 37,
        keysyms::KEY_l | keysyms::KEY_L => 38,
        keysyms::KEY_m | keysyms::KEY_M => 50,
        keysyms::KEY_n | keysyms::KEY_N => 49,
        keysyms::KEY_o | keysyms::KEY_O => 24,
        keysyms::KEY_p | keysyms::KEY_P => 25,
        keysyms::KEY_q | keysyms::KEY_Q => 16,
        keysyms::KEY_r | keysyms::KEY_R => 19,
        keysyms::KEY_s | keysyms::KEY_S => 31,
        keysyms::KEY_t | keysyms::KEY_T => 20,
        keysyms::KEY_u | keysyms::KEY_U => 22,
        keysyms::KEY_v | keysyms::KEY_V => 47,
        keysyms::KEY_w | keysyms::KEY_W => 17,
        keysyms::KEY_x | keysyms::KEY_X => 45,
        keysyms::KEY_y | keysyms::KEY_Y => 21,
        keysyms::KEY_z | keysyms::KEY_Z => 44,
        // Numbers
        keysyms::KEY_1 | keysyms::KEY_exclam => 2,
        keysyms::KEY_2 | keysyms::KEY_at => 3,
        keysyms::KEY_3 | keysyms::KEY_numbersign => 4,
        keysyms::KEY_4 | keysyms::KEY_dollar => 5,
        keysyms::KEY_5 | keysyms::KEY_percent => 6,
        keysyms::KEY_6 | keysyms::KEY_asciicircum => 7,
        keysyms::KEY_7 | keysyms::KEY_ampersand => 8,
        keysyms::KEY_8 | keysyms::KEY_asterisk => 9,
        keysyms::KEY_9 | keysyms::KEY_parenleft => 10,
        keysyms::KEY_0 | keysyms::KEY_parenright => 11,
        // Special keys
        keysyms::KEY_Return => 28,
        keysyms::KEY_Escape => 1,
        keysyms::KEY_BackSpace => 14,
        keysyms::KEY_Tab => 15,
        keysyms::KEY_space => 57,
        keysyms::KEY_minus | keysyms::KEY_underscore => 12,
        keysyms::KEY_equal | keysyms::KEY_plus => 13,
        keysyms::KEY_bracketleft | keysyms::KEY_braceleft => 26,
        keysyms::KEY_bracketright | keysyms::KEY_braceright => 27,
        keysyms::KEY_backslash | keysyms::KEY_bar => 43,
        keysyms::KEY_semicolon | keysyms::KEY_colon => 39,
        keysyms::KEY_apostrophe | keysyms::KEY_quotedbl => 40,
        keysyms::KEY_grave | keysyms::KEY_asciitilde => 41,
        keysyms::KEY_comma | keysyms::KEY_less => 51,
        keysyms::KEY_period | keysyms::KEY_greater => 52,
        keysyms::KEY_slash | keysyms::KEY_question => 53,
        // Modifiers
        keysyms::KEY_Shift_L => 42,
        keysyms::KEY_Shift_R => 54,
        keysyms::KEY_Control_L => 29,
        keysyms::KEY_Control_R => 97,
        keysyms::KEY_Alt_L => 56,
        keysyms::KEY_Alt_R => 100,
        keysyms::KEY_Super_L => 125,
        keysyms::KEY_Super_R => 126,
        keysyms::KEY_Caps_Lock => 58,
        // Function keys
        keysyms::KEY_F1 => 59,
        keysyms::KEY_F2 => 60,
        keysyms::KEY_F3 => 61,
        keysyms::KEY_F4 => 62,
        keysyms::KEY_F5 => 63,
        keysyms::KEY_F6 => 64,
        keysyms::KEY_F7 => 65,
        keysyms::KEY_F8 => 66,
        keysyms::KEY_F9 => 67,
        keysyms::KEY_F10 => 68,
        keysyms::KEY_F11 => 87,
        keysyms::KEY_F12 => 88,
        // Navigation
        keysyms::KEY_Home => 102,
        keysyms::KEY_End => 107,
        keysyms::KEY_Page_Up => 104,
        keysyms::KEY_Page_Down => 109,
        keysyms::KEY_Up => 103,
        keysyms::KEY_Down => 108,
        keysyms::KEY_Left => 105,
        keysyms::KEY_Right => 106,
        keysyms::KEY_Insert => 110,
        keysyms::KEY_Delete => 111,
        _ => return None,
    };
    // XKB keycodes are evdev keycodes + 8
    Some(evdev + 8)
}
