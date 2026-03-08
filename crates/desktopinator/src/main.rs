mod ipc;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use dinator_ipc::{IpcCommand, IpcResponse};
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::solid::{SolidColorBuffer, SolidColorRenderElement};
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::element::RenderElement;
use smithay::backend::renderer::gles::{GlesFrame, GlesRenderer};
use smithay::desktop::space::SpaceRenderElements;
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::wayland::compositor;
use smithay::wayland::shell::xdg::XdgToplevelSurfaceData;
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{EventLoop, Interest, PostAction};
use smithay::reexports::wayland_server::{Display, ListeningSocket};
use smithay::utils::{Physical, Point, Rectangle, Transform};
use tracing::info;

use dinator_core::DinatorState;
use dinator_plugin_api::PluginRuntime;

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

    // Render cursor as a small white square at pointer position
    if let Some(pointer) = state.seat.get_pointer() {
        let pos = pointer.current_location();
        let cursor_size = 8;
        let cursor_buf = SolidColorBuffer::new((cursor_size, cursor_size), [1.0, 1.0, 1.0, 1.0]);
        let cursor_loc: Point<i32, Physical> = (pos.x as i32, pos.y as i32).into();
        elements.insert(
            0,
            OutputRenderElements::Border(SolidColorRenderElement::from_buffer(
                &cursor_buf,
                cursor_loc,
                1.0,
                1.0,
                Kind::Cursor,
            )),
        );
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

    let args: Vec<String> = std::env::args().collect();
    let headless = args.iter().any(|a| a == "--headless");

    if headless {
        // Parse --vnc-port PORT (default 5900)
        let vnc_port = args
            .windows(2)
            .find(|w| w[0] == "--vnc-port")
            .and_then(|w| w[1].parse::<u16>().ok())
            .unwrap_or(5900);

        // Parse --resolution WxH (default 1920x1080)
        let (width, height) = args
            .windows(2)
            .find(|w| w[0] == "--resolution")
            .and_then(|w| {
                let parts: Vec<&str> = w[1].split('x').collect();
                if parts.len() == 2 {
                    Some((parts[0].parse::<u16>().ok()?, parts[1].parse::<u16>().ok()?))
                } else {
                    None
                }
            })
            .unwrap_or((1920, 1080));

        info!("starting desktopinator (headless)");
        run_headless(width, height, vnc_port)
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
    init_plugins(&mut state);

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
                    if let Err(e) = state
                        .display_handle
                        .insert_client(stream, client_state)
                    {
                        tracing::error!("failed to insert client: {e}");
                    }
                }
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| anyhow::anyhow!("failed to insert socket source: {e}"))?;

    // IPC server (resize is a no-op in winit mode — window manager controls size)
    let pending_resize_winit: Arc<std::sync::Mutex<Option<(u16, u16)>>> =
        Arc::new(std::sync::Mutex::new(None));
    let ipc_rx = ipc::start_ipc_server(state.event_broadcaster.clone())?;
    let output_for_ipc = output.clone();
    event_loop
        .handle()
        .insert_source(ipc_rx, move |event, _, state| {
            use smithay::reexports::calloop::channel;
            let channel::Event::Msg(request) = event else {
                return;
            };
            let response = handle_ipc_command(
                &request.command,
                state,
                &output_for_ipc,
                &pending_resize_winit,
            );
            (request.respond)(response);
        })
        .map_err(|e| anyhow::anyhow!("failed to insert IPC source: {e}"))?;

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

                if let Err(e) = backend.submit(Some(&[damage])) {
                    tracing::error!("backend submit error: {e:?}");
                }

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
            if let Err(e) = unsafe { &mut *display_ptr }.dispatch_clients(state) {
                tracing::error!("dispatch_clients error: {e}");
            }
            if let Err(e) = state.display.flush_clients() {
                tracing::error!("flush_clients error: {e}");
            }
        })
        .context("event loop error")?;

    info!("shutting down");
    cleanup_and_exit(0)
}

/// VNC input event sent from the VNC server thread to the compositor event loop.
#[allow(dead_code)]
enum VncInputEvent {
    PointerMove { x: u16, y: u16, button_mask: u8 },
    Key { keysym: u32, pressed: bool },
    ResizeOutput { width: u16, height: u16 },
}

fn run_headless(width: u16, height: u16, vnc_port: u16) -> anyhow::Result<()> {
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
    let vnc_server = Arc::new(vnc_server);

    // Create a calloop channel to receive VNC input events in the compositor event loop
    let (vnc_input_tx, vnc_input_rx): (channel::Sender<VncInputEvent>, Channel<VncInputEvent>) =
        channel::channel();

    // Create a channel to send pixel data to the VNC server's tokio runtime
    let (pixel_tx, mut pixel_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(2);

    // Create a channel to send resize commands to the VNC framebuffer (compositor → tokio)
    let (vnc_resize_tx, mut vnc_resize_rx) = tokio::sync::mpsc::channel::<(u16, u16)>(4);

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

            // Handle framebuffer resize commands from the compositor
            let fb_resize = vnc_framebuffer.clone();
            let vnc_server_resize = vnc_server.clone();
            tokio::spawn(async move {
                while let Some((w, h)) = vnc_resize_rx.recv().await {
                    info!(width = w, height = h, "resizing VNC framebuffer");
                    if let Err(e) = fb_resize.resize(w, h).await {
                        tracing::error!("VNC framebuffer resize failed: {e}");
                    } else {
                        // Disconnect all clients so they reconnect at the new resolution
                        // (rustvncserver doesn't send DesktopSize pseudo-encoding updates)
                        info!("disconnecting VNC clients for resolution change");
                        vnc_server_resize.disconnect_all_clients().await;
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

            if let Err(e) = vnc_server.as_ref().listen(vnc_port).await {
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
    init_plugins(&mut state);

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
                    if let Err(e) = state
                        .display_handle
                        .insert_client(stream, client_state)
                    {
                        tracing::error!("failed to insert client: {e}");
                    }
                }
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| anyhow::anyhow!("failed to insert socket source: {e}"))?;

    // Pending resolution change, shared between input handler and render timer
    let pending_resize: Arc<std::sync::Mutex<Option<(u16, u16)>>> =
        Arc::new(std::sync::Mutex::new(None));
    let pending_resize_input = pending_resize.clone();
    let pending_resize_ipc = pending_resize.clone();

    // Start IPC server
    let ipc_rx = ipc::start_ipc_server(state.event_broadcaster.clone())?;
    let output_for_ipc = output.clone();
    event_loop
        .handle()
        .insert_source(ipc_rx, move |event, _, state| {
            let channel::Event::Msg(request) = event else {
                return;
            };
            let response = handle_ipc_command(
                &request.command,
                state,
                &output_for_ipc,
                &pending_resize_ipc,
            );
            (request.respond)(response);
        })
        .map_err(|e| anyhow::anyhow!("failed to insert IPC source: {e}"))?;

    // Handle VNC input events in the compositor event loop
    let output_for_input = output.clone();
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
                    let Some(pointer) = state.seat.get_pointer() else { return };
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
                                        let Some(keyboard) = state.seat.get_keyboard() else { return };
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
                VncInputEvent::ResizeOutput { width, height } => {
                    info!(width, height, "resolution change requested");
                    *pending_resize_input.lock().unwrap() = Some((width, height));
                }
                VncInputEvent::Key { keysym, pressed } => {
                    let Some(keyboard) = state.seat.get_keyboard() else { return };
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
                                    | keysyms::KEY_q | keysyms::KEY_Q | keysyms::KEY_space
                                    | keysyms::KEY_h | keysyms::KEY_l
                                    | keysyms::KEY_f | keysyms::KEY_v | keysyms::KEY_m
                                    | keysyms::KEY_plus | keysyms::KEY_equal
                                    | keysyms::KEY_minus => {
                                        if key_state == KeyState::Pressed {
                                            let action = match sym.raw() {
                                                keysyms::KEY_Return => KeyAction::LaunchTerminal,
                                                keysyms::KEY_q => KeyAction::CloseWindow,
                                                keysyms::KEY_Q => KeyAction::Quit,
                                                keysyms::KEY_j => KeyAction::FocusNext,
                                                keysyms::KEY_k => KeyAction::FocusPrev,
                                                keysyms::KEY_space => KeyAction::SwapMaster,
                                                keysyms::KEY_h => KeyAction::MasterShrink,
                                                keysyms::KEY_l => KeyAction::MasterGrow,
                                                keysyms::KEY_f => KeyAction::ToggleFullscreen,
                                                keysyms::KEY_v => KeyAction::ToggleFloat,
                                                keysyms::KEY_m => KeyAction::ToggleMonocle,
                                                keysyms::KEY_plus | keysyms::KEY_equal => {
                                                    KeyAction::ResolutionUp
                                                }
                                                keysyms::KEY_minus => KeyAction::ResolutionDown,
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
                                KeyAction::MasterGrow | KeyAction::MasterShrink => {
                                    let changed = if matches!(action, KeyAction::MasterGrow) {
                                        state.layout.grow_master()
                                    } else {
                                        state.layout.shrink_master()
                                    };
                                    if changed {
                                        let output = state.space.outputs().next().cloned();
                                        if let Some(output) = output {
                                            state.retile(&output);
                                        }
                                    }
                                }
                                KeyAction::ToggleFullscreen => {
                                    state.toggle_fullscreen();
                                }
                                KeyAction::ToggleFloat => {
                                    state.toggle_float();
                                }
                                KeyAction::ToggleMonocle => {
                                    let current = state.layout.name();
                                    let new_layout = if current == "monocle" { "column" } else { "monocle" };
                                    state.set_layout(new_layout);
                                    let output = state.space.outputs().next().cloned();
                                    if let Some(output) = output {
                                        state.retile(&output);
                                    }
                                    state.emit_event(dinator_ipc::IpcEvent::LayoutChanged {
                                        name: new_layout.to_string(),
                                    });
                                }
                                KeyAction::ResolutionUp | KeyAction::ResolutionDown => {
                                    let dir = if matches!(action, KeyAction::ResolutionUp) {
                                        1
                                    } else {
                                        -1
                                    };
                                    // Get current resolution from output
                                    if let Some(mode) = output_for_input.current_mode() {
                                        let (new_w, new_h) = next_resolution(
                                            mode.size.w as u16,
                                            mode.size.h as u16,
                                            dir,
                                        );
                                        info!(
                                            from = %format!("{}x{}", mode.size.w, mode.size.h),
                                            to = %format!("{new_w}x{new_h}"),
                                            "resolution change keybinding"
                                        );
                                        *pending_resize_input.lock().unwrap() =
                                            Some((new_w, new_h));
                                    }
                                }
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
    let mut current_width = width;
    let mut current_height = height;
    let pending_resize_render = pending_resize.clone();

    // Timer-based redraw at ~60fps
    let timer = Timer::immediate();
    event_loop
        .handle()
        .insert_source(timer, move |_, _, state| {
            let output = &output_for_render;

            // Check for pending resolution change
            if let Some((new_w, new_h)) = pending_resize_render.lock().unwrap().take() {
                if new_w != current_width || new_h != current_height {
                    info!(
                        old_w = current_width,
                        old_h = current_height,
                        new_w,
                        new_h,
                        "applying resolution change"
                    );

                    // Create new renderbuffer
                    match renderer
                        .create_buffer(Fourcc::Abgr8888, Size::from((new_w as i32, new_h as i32)))
                    {
                        Ok(new_rb) => {
                            renderbuffer = new_rb;
                            current_width = new_w;
                            current_height = new_h;

                            // Update output mode
                            let mode = Mode {
                                size: (new_w as i32, new_h as i32).into(),
                                refresh: 60_000,
                            };
                            output.change_current_state(Some(mode), None, None, None);
                            output.set_preferred(mode);

                            // Reset damage tracker for new size
                            damage_tracker = OutputDamageTracker::from_output(output);

                            // Re-tile windows for new resolution
                            state.retile(output);

                            // Tell VNC to resize its framebuffer
                            let _ = vnc_resize_tx.try_send((new_w, new_h));

                            info!(
                                width = new_w,
                                height = new_h,
                                "resolution change applied"
                            );

                            state.emit_event(dinator_ipc::IpcEvent::ResolutionChanged {
                                width: new_w,
                                height: new_h,
                            });
                        }
                        Err(e) => {
                            tracing::error!("failed to create new renderbuffer: {e:?}");
                        }
                    }
                }
            }

            // Render to offscreen buffer
            match renderer.bind(&mut renderbuffer) {
                Ok(mut target) => {
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
                        Size::from((current_width as i32, current_height as i32)),
                    );
                    if let Ok(mapping) = renderer.copy_framebuffer(&target, region, Fourcc::Abgr8888) {
                        if let Ok(pixels) = renderer.map_texture(&mapping) {
                            let _ = pixel_tx.try_send(pixels.to_vec());
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("failed to bind renderbuffer: {e:?}");
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
            if let Err(e) = unsafe { &mut *display_ptr }.dispatch_clients(state) {
                tracing::error!("dispatch_clients error: {e}");
            }
            if let Err(e) = state.display.flush_clients() {
                tracing::error!("flush_clients error: {e}");
            }
        })
        .context("event loop error")?;

    info!("shutting down");
    cleanup_and_exit(0)
}

/// Clean up resources and exit the process.
///
/// Background threads (IPC listener, tokio/VNC runtime) block on I/O that
/// cannot be interrupted without significant complexity, so we clean up
/// what we can and then exit the process.
fn cleanup_and_exit(code: i32) -> ! {
    let socket_path = dinator_ipc::socket_path();
    if socket_path.exists() {
        if let Err(e) = std::fs::remove_file(&socket_path) {
            tracing::error!(path = %socket_path.display(), error = %e, "failed to remove IPC socket");
        } else {
            info!(path = %socket_path.display(), "removed IPC socket");
        }
    }
    std::process::exit(code);
}

enum KeyAction {
    LaunchTerminal,
    CloseWindow,
    FocusNext,
    FocusPrev,
    SwapMaster,
    MasterGrow,
    MasterShrink,
    ToggleFullscreen,
    ToggleFloat,
    ToggleMonocle,
    ResolutionUp,
    ResolutionDown,
    Quit,
}

/// Common resolutions for cycling through with keybindings.
const RESOLUTIONS: &[(u16, u16)] = &[
    (1280, 720),
    (1366, 768),
    (1600, 900),
    (1920, 1080),
    (2560, 1440),
    (3840, 2160),
];

fn next_resolution(current_w: u16, current_h: u16, direction: i32) -> (u16, u16) {
    let current_pixels = current_w as u32 * current_h as u32;
    if direction > 0 {
        // Find the next larger resolution
        RESOLUTIONS
            .iter()
            .find(|&&(w, h)| (w as u32 * h as u32) > current_pixels)
            .copied()
            .unwrap_or(*RESOLUTIONS.last().unwrap())
    } else {
        // Find the next smaller resolution
        RESOLUTIONS
            .iter()
            .rev()
            .find(|&&(w, h)| (w as u32 * h as u32) < current_pixels)
            .copied()
            .unwrap_or(*RESOLUTIONS.first().unwrap())
    }
}

/// Initialize the Lua plugin runtime and load plugins from the config directory.
fn init_plugins(state: &mut DinatorState) {
    let plugin_dir = std::env::var("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::PathBuf::from(
                std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string()),
            )
            .join(".config")
        })
        .join("desktopinator/plugins");

    let mut runtime = dinator_lua::LuaRuntime::new();
    if let Err(e) = runtime.load_plugins(&plugin_dir) {
        tracing::warn!(error = %e, "failed to load plugins");
    }

    let plugin_count = runtime.plugin_info().len();
    let layout_count = runtime.layout_names().len();
    if plugin_count > 0 {
        info!(
            plugins = plugin_count,
            layouts = layout_count,
            "plugin system initialized"
        );
    }

    state.plugin_runtime = Some(Box::new(runtime));
}

/// Handle an IPC command from dinatorctl.
fn handle_ipc_command(
    command: &IpcCommand,
    state: &mut DinatorState,
    _output: &Output,
    pending_resize: &Arc<std::sync::Mutex<Option<(u16, u16)>>>,
) -> IpcResponse {
    match command {
        IpcCommand::Resize { width, height } => {
            info!(width, height, "IPC: resize");
            *pending_resize.lock().unwrap() = Some((*width, *height));
            IpcResponse::Ok {
                message: Some(format!("resizing to {width}x{height}")),
            }
        }
        IpcCommand::FocusNext => {
            state.focus_next();
            IpcResponse::Ok { message: None }
        }
        IpcCommand::FocusPrev => {
            state.focus_prev();
            IpcResponse::Ok { message: None }
        }
        IpcCommand::Close => {
            state.close_focused_window();
            IpcResponse::Ok { message: None }
        }
        IpcCommand::SwapMaster => {
            state.swap_master();
            IpcResponse::Ok { message: None }
        }
        IpcCommand::Spawn { cmd, args } => {
            info!(cmd, ?args, "IPC: spawn");
            match std::process::Command::new(cmd).args(args).spawn() {
                Ok(_) => IpcResponse::Ok {
                    message: Some(format!("spawned {cmd}")),
                },
                Err(e) => IpcResponse::Error {
                    message: format!("failed to spawn {cmd}: {e}"),
                },
            }
        }
        IpcCommand::Quit => {
            info!("IPC: quit");
            state.loop_signal.stop();
            IpcResponse::Ok { message: None }
        }
        IpcCommand::MasterGrow => {
            if state.layout.grow_master() {
                let output = state.space.outputs().next().cloned();
                if let Some(output) = output {
                    state.retile(&output);
                }
                let ratio = state.layout.master_ratio().unwrap_or(0.0);
                IpcResponse::Ok {
                    message: Some(format!("master ratio: {ratio:.0}%", ratio = ratio * 100.0)),
                }
            } else {
                IpcResponse::Ok {
                    message: Some("master ratio at maximum".to_string()),
                }
            }
        }
        IpcCommand::MasterShrink => {
            if state.layout.shrink_master() {
                let output = state.space.outputs().next().cloned();
                if let Some(output) = output {
                    state.retile(&output);
                }
                let ratio = state.layout.master_ratio().unwrap_or(0.0);
                IpcResponse::Ok {
                    message: Some(format!("master ratio: {ratio:.0}%", ratio = ratio * 100.0)),
                }
            } else {
                IpcResponse::Ok {
                    message: Some("master ratio at minimum".to_string()),
                }
            }
        }
        IpcCommand::ListWindows => {
            let windows: Vec<serde_json::Value> = state
                .window_order
                .iter()
                .enumerate()
                .map(|(i, id)| {
                    let is_floating = state.floating.contains(id);
                    let is_fullscreen = state.fullscreen.contains(id);
                    let mut entry = serde_json::json!({
                        "index": i,
                        "id": id.0,
                        "floating": is_floating,
                        "fullscreen": is_fullscreen,
                    });
                    if let Some(window) = state.window_map.get(id) {
                        if let Some(geo) = state.space.element_geometry(window) {
                            entry["x"] = geo.loc.x.into();
                            entry["y"] = geo.loc.y.into();
                            entry["width"] = geo.size.w.into();
                            entry["height"] = geo.size.h.into();
                        }
                        if let Some(toplevel) = window.toplevel() {
                            let (app_id, title) = compositor::with_states(toplevel.wl_surface(), |states| {
                                let attrs = states.data_map.get::<XdgToplevelSurfaceData>();
                                let attrs = attrs.map(|d| d.lock().unwrap());
                                (
                                    attrs.as_ref().and_then(|a| a.app_id.clone()),
                                    attrs.as_ref().and_then(|a| a.title.clone()),
                                )
                            });
                            if let Some(app_id) = app_id {
                                entry["app_id"] = app_id.into();
                            }
                            if let Some(title) = title {
                                entry["title"] = title.into();
                            }
                        }
                    }
                    entry
                })
                .collect();
            IpcResponse::Data {
                data: serde_json::Value::Array(windows),
            }
        }
        IpcCommand::SetLayout { name } => {
            info!(layout = %name, "IPC: set-layout");
            if state.set_layout(name) {
                let output = state.space.outputs().next().cloned();
                if let Some(output) = output {
                    state.retile(&output);
                }
                state.emit_event(dinator_ipc::IpcEvent::LayoutChanged {
                    name: name.clone(),
                });
                IpcResponse::Ok {
                    message: Some(format!("layout: {name}")),
                }
            } else {
                let available = state.available_layouts().join(", ");
                IpcResponse::Error {
                    message: format!("unknown layout: {name} (available: {available})"),
                }
            }
        }
        IpcCommand::ListLayouts => {
            let layouts = state.available_layouts();
            let current = state.layout.name().to_string();
            let data: Vec<serde_json::Value> = layouts
                .iter()
                .map(|name| {
                    serde_json::json!({
                        "name": name,
                        "active": *name == current,
                    })
                })
                .collect();
            IpcResponse::Data {
                data: serde_json::Value::Array(data),
            }
        }
        IpcCommand::ListPlugins => {
            let plugins = if let Some(ref runtime) = state.plugin_runtime {
                runtime.plugin_info()
            } else {
                Vec::new()
            };
            let data: Vec<serde_json::Value> = plugins
                .iter()
                .map(|p| {
                    serde_json::json!({
                        "id": p.id,
                        "name": p.name,
                        "version": p.version,
                        "source": p.source,
                        "layouts": p.layouts,
                    })
                })
                .collect();
            IpcResponse::Data {
                data: serde_json::Value::Array(data),
            }
        }
        IpcCommand::ReloadPlugins => {
            if let Some(ref mut runtime) = state.plugin_runtime {
                match runtime.reload() {
                    Ok(()) => {
                        let count = runtime.plugin_info().len();
                        info!(count, "plugins reloaded");
                        // Re-create active layout from new runtime, or fall back
                        let current = state.layout.name().to_string();
                        if current != "column" && current != "monocle" {
                            if let Some(new_layout) = runtime.create_layout(&current) {
                                state.layout = new_layout;
                                info!("re-created plugin layout '{current}' from reloaded plugin");
                            } else {
                                state.layout = Box::new(dinator_tiling::ColumnLayout::default());
                                info!("active plugin layout '{current}' no longer available, fell back to column");
                            }
                            let output = state.space.outputs().next().cloned();
                            if let Some(output) = output {
                                state.retile(&output);
                            }
                        }
                        IpcResponse::Ok {
                            message: Some(format!("reloaded {count} plugins")),
                        }
                    }
                    Err(e) => IpcResponse::Error {
                        message: format!("reload failed: {e}"),
                    },
                }
            } else {
                IpcResponse::Error {
                    message: "no plugin runtime configured".to_string(),
                }
            }
        }
        IpcCommand::ToggleFloat => {
            if let Some((id, is_floating)) = state.toggle_float() {
                info!(id = id.0, floating = is_floating, "IPC: toggle-float");
                IpcResponse::Ok {
                    message: Some(if is_floating {
                        format!("window {} floating", id.0)
                    } else {
                        format!("window {} tiled", id.0)
                    }),
                }
            } else {
                IpcResponse::Error {
                    message: "no focused window".to_string(),
                }
            }
        }
        IpcCommand::ToggleFullscreen => {
            if let Some((id, is_fullscreen)) = state.toggle_fullscreen() {
                info!(id = id.0, fullscreen = is_fullscreen, "IPC: toggle-fullscreen");
                IpcResponse::Ok {
                    message: Some(if is_fullscreen {
                        format!("window {} fullscreen", id.0)
                    } else {
                        format!("window {} normal", id.0)
                    }),
                }
            } else {
                IpcResponse::Error {
                    message: "no focused window".to_string(),
                }
            }
        }
        IpcCommand::Subscribe => {
            // Subscribe is handled in the IPC server thread, not here.
            // If we get here, something is wrong.
            IpcResponse::Error {
                message: "subscribe should be handled by IPC server".to_string(),
            }
        }
    }
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
