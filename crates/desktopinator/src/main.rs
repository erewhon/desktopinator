mod adaptive;
mod audio;
mod clipboard;
mod config;
mod displaycontrol;
mod drm_backend;
mod text;
mod gfx;
mod ipc;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use dinator_ipc::{IpcCommand, IpcResponse};
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::memory::MemoryRenderBufferRenderElement;
use smithay::backend::renderer::element::solid::{SolidColorBuffer, SolidColorRenderElement};
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::element::RenderElement;
use smithay::backend::renderer::gles::{GlesFrame, GlesRenderer};
use smithay::desktop::layer_map_for_output;
use smithay::desktop::space::SpaceRenderElements;
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{EventLoop, Interest, PostAction};
use smithay::reexports::wayland_server::{Display, ListeningSocket};
use smithay::utils::{Physical, Point, Rectangle, Transform};
use smithay::wayland::compositor;
use smithay::wayland::shell::xdg::XdgToplevelSurfaceData;
use tracing::info;

use dinator_core::DinatorState;
use dinator_plugin_api::PluginRuntime;

type SpaceElements = SpaceRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>;

enum OutputRenderElements {
    Space(SpaceElements),
    Border(SolidColorRenderElement),
    Memory(MemoryRenderBufferRenderElement<GlesRenderer>),
}

impl smithay::backend::renderer::element::Element for OutputRenderElements {
    fn id(&self) -> &smithay::backend::renderer::element::Id {
        match self {
            Self::Space(e) => e.id(),
            Self::Border(e) => e.id(),
            Self::Memory(e) => e.id(),
        }
    }
    fn current_commit(&self) -> smithay::backend::renderer::utils::CommitCounter {
        match self {
            Self::Space(e) => e.current_commit(),
            Self::Border(e) => e.current_commit(),
            Self::Memory(e) => e.current_commit(),
        }
    }
    fn geometry(&self, scale: smithay::utils::Scale<f64>) -> Rectangle<i32, Physical> {
        match self {
            Self::Space(e) => e.geometry(scale),
            Self::Border(e) => e.geometry(scale),
            Self::Memory(e) => e.geometry(scale),
        }
    }
    fn src(&self) -> Rectangle<f64, smithay::utils::Buffer> {
        match self {
            Self::Space(e) => e.src(),
            Self::Border(e) => e.src(),
            Self::Memory(e) => e.src(),
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
            Self::Border(e) => <SolidColorRenderElement as RenderElement<GlesRenderer>>::draw(
                e,
                frame,
                src,
                dst,
                damage,
                opaque_regions,
            ),
            Self::Memory(e) => RenderElement::<GlesRenderer>::draw(
                e,
                frame,
                src,
                dst,
                damage,
                opaque_regions,
            ),
        }
    }
}

/// Build the render element list: space elements + focus border.
/// `render_cursor`: if false, skip the software cursor (RDP sends cursor separately).
pub(crate) fn build_render_elements(
    renderer: &mut GlesRenderer,
    state: &DinatorState,
    output: &Output,
) -> Option<Vec<OutputRenderElements>> {
    build_render_elements_inner(renderer, state, output, true)
}

/// Build render elements, optionally skipping the software cursor.
pub(crate) fn build_render_elements_no_cursor(
    renderer: &mut GlesRenderer,
    state: &DinatorState,
    output: &Output,
) -> Option<Vec<OutputRenderElements>> {
    build_render_elements_inner(renderer, state, output, false)
}

fn build_render_elements_inner(
    renderer: &mut GlesRenderer,
    state: &DinatorState,
    output: &Output,
    render_cursor: bool,
) -> Option<Vec<OutputRenderElements>> {
    let space_elements: Vec<SpaceElements> = match state
        .space
        .render_elements_for_output(renderer, output, 1.0)
    {
        Ok(elements) => elements,
        Err(_) => return None,
    };

    let mut elements: Vec<OutputRenderElements> = space_elements
        .into_iter()
        .map(OutputRenderElements::Space)
        .collect();

    // Output geometry in global compositor coordinates — used to offset custom
    // elements (border, cursor) which use global positions but must be rendered
    // relative to this output's origin.
    let output_geo = state.space.output_geometry(output).unwrap_or_default();

    // Add focus indicator border behind the focused window (only if window is on this output)
    let border_width = 2;
    let border_color = [0.4f32, 0.6, 0.9, 1.0]; // blue
    if let Some(focused) = state.focused_window() {
        if let Some(geo) = state.space.element_geometry(focused) {
            // Only render border on the output that contains the window
            if output_geo.overlaps(geo) {
                let buf = SolidColorBuffer::new(
                    (geo.size.w + 2 * border_width, geo.size.h + 2 * border_width),
                    border_color,
                );
                let loc: Point<i32, Physical> = (
                    geo.loc.x - border_width - output_geo.loc.x,
                    geo.loc.y - border_width - output_geo.loc.y,
                )
                    .into();
                elements.push(OutputRenderElements::Border(
                    SolidColorRenderElement::from_buffer(&buf, loc, 1.0, 1.0, Kind::Unspecified),
                ));
            }
        }
    }

    // Draw borders around floating windows as edge strips ON TOP of window content
    for &id in &state.floating {
        if let Some(window) = state.window_map.get(&id) {
            if let Some(geo) = state.space.element_geometry(window) {
                if output_geo.overlaps(geo) && geo.size.w > 10 && geo.size.h > 10 {
                    let bw = 3;
                    let rx = geo.loc.x - output_geo.loc.x;
                    let ry = geo.loc.y - output_geo.loc.y;
                    let rw = geo.size.w;
                    let rh = geo.size.h;
                    let c = [0.35f32, 0.4, 0.65, 1.0];

                    let mk = |w, h, x, y| {
                        let buf = SolidColorBuffer::new((w, h), c);
                        let loc: Point<i32, Physical> = (x, y).into();
                        OutputRenderElements::Border(
                            SolidColorRenderElement::from_buffer(&buf, loc, 1.0, 1.0, Kind::Unspecified),
                        )
                    };
                    elements.insert(0, mk(rw, bw, rx, ry));            // top
                    elements.insert(0, mk(rw, bw, rx, ry + rh - bw));  // bottom
                    elements.insert(0, mk(bw, rh, rx, ry));            // left
                    elements.insert(0, mk(bw, rh, rx + rw - bw, ry));  // right
                }
            }
        }
    }

    // Stacked layout: render tab bar above the content area
    if let Some((tabs, tab_h, gap)) = state.stacked_tabs(output) {
        let mode = output.current_mode();
        let output_w = mode.map(|m| m.size.w).unwrap_or(1920);

        for (i, (app_id, title, is_focused)) in tabs.iter().enumerate() {
            let tab_x = gap;
            let tab_y = gap + i as i32 * tab_h;
            let tab_w = output_w - 2 * gap;

            // Tab background — inactive tabs slightly translucent
            let bg_color = if *is_focused {
                [0.25f32, 0.28, 0.38, 1.0]
            } else {
                [0.16, 0.17, 0.21, 0.88]
            };
            let bg = SolidColorBuffer::new((tab_w, tab_h), bg_color);
            let loc: Point<i32, Physical> = (tab_x, tab_y).into();
            elements.insert(
                0,
                OutputRenderElements::Border(SolidColorRenderElement::from_buffer(
                    &bg, loc, 1.0, 1.0, Kind::Unspecified,
                )),
            );

            // Left accent stripe
            let accent = if *is_focused {
                [0.4f32, 0.6, 0.9, 1.0]
            } else {
                [0.3, 0.35, 0.5, 0.5]
            };
            let stripe = SolidColorBuffer::new((3, tab_h), accent);
            let stripe_loc: Point<i32, Physical> = (tab_x, tab_y).into();
            elements.insert(
                0,
                OutputRenderElements::Border(SolidColorRenderElement::from_buffer(
                    &stripe, stripe_loc, 1.0, 1.0, Kind::Unspecified,
                )),
            );

            // Bottom separator
            let sep = SolidColorBuffer::new((tab_w, 1), [0.1f32, 0.1, 0.12, 1.0]);
            let sep_loc: Point<i32, Physical> = (tab_x, tab_y + tab_h - 1).into();
            elements.insert(
                0,
                OutputRenderElements::Border(SolidColorRenderElement::from_buffer(
                    &sep, sep_loc, 1.0, 1.0, Kind::Unspecified,
                )),
            );

            // Title text
            let label = if title.is_empty() {
                app_id.clone()
            } else if app_id.is_empty() {
                title.clone()
            } else {
                format!("{title}  —  {app_id}")
            };
            if !label.is_empty() {
                let text_color = if *is_focused {
                    [230u8, 235, 245]
                } else {
                    [170, 175, 190]
                };
                let text_x = tab_x + 12; // padding after accent stripe
                let text_max_w = tab_w - 20;
                let (pixels, tw, th) = text::render_text(&label, 17.0, text_color, text_max_w, tab_h);
                if tw > 0 && th > 0 {
                    use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
                    let buf = MemoryRenderBuffer::from_slice(
                        &pixels,
                        smithay::backend::allocator::Fourcc::Argb8888,
                        (tw, th),
                        1,
                        Transform::Normal,
                        None,
                    );
                    let text_loc: Point<f64, Physical> = (text_x as f64, tab_y as f64).into();
                    if let Ok(el) = MemoryRenderBufferRenderElement::from_buffer(
                        renderer,
                        text_loc,
                        &buf,
                        None,
                        None,
                        None,
                        Kind::Unspecified,
                    ) {
                        elements.insert(0, OutputRenderElements::Memory(el));
                    }
                }
            }
        }
    }

    // Software cursor — skipped when RDP sends cursor position separately
    if render_cursor {
    if let Some(pointer) = state.seat.get_pointer() {
        let pos = pointer.current_location();
        let cursor_rect = Rectangle::new((pos.x as i32, pos.y as i32).into(), (8, 8).into());
        if output_geo.overlaps(cursor_rect) {
            let cursor_size = 8;
            let cursor_buf =
                SolidColorBuffer::new((cursor_size, cursor_size), [1.0, 1.0, 1.0, 1.0]);
            let cursor_loc: Point<i32, Physical> = (
                pos.x as i32 - output_geo.loc.x,
                pos.y as i32 - output_geo.loc.y,
            )
                .into();
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
    }
    } // if render_cursor

    // Render gradient background as horizontal bands (behind everything else)
    if let dinator_core::Background::Gradient { top, bottom } = state.background_for_output(output)
    {
        if let Some(mode) = output.current_mode() {
            let height = mode.size.h;
            let num_bands = 64; // balance between smoothness and element count
            let band_h = (height + num_bands - 1) / num_bands;
            for i in 0..num_bands {
                let t = i as f32 / (num_bands - 1) as f32;
                let color = [
                    top[0] + (bottom[0] - top[0]) * t,
                    top[1] + (bottom[1] - top[1]) * t,
                    top[2] + (bottom[2] - top[2]) * t,
                    1.0,
                ];
                let y = i * band_h;
                if y >= height {
                    break;
                }
                let h = band_h.min(height - y);
                let buf = SolidColorBuffer::new((mode.size.w, h), color);
                let loc: Point<i32, Physical> = (0, y).into();
                elements.push(OutputRenderElements::Border(
                    SolidColorRenderElement::from_buffer(&buf, loc, 1.0, 1.0, Kind::Unspecified),
                ));
            }
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

    let args: Vec<String> = std::env::args().collect();
    let headless = args.iter().any(|a| a == "--headless");
    let drm = args.iter().any(|a| a == "--drm");
    let cfg = config::load_config();

    if drm {
        drm_backend::run_drm(&cfg)
    } else if headless {
        // Parse --vnc-port PORT (CLI overrides config, default 5900)
        let vnc_port = args
            .windows(2)
            .find(|w| w[0] == "--vnc-port")
            .and_then(|w| w[1].parse::<u16>().ok())
            .unwrap_or(cfg.vnc.port);

        // Parse --rdp-port PORT (CLI overrides config, default 3389)
        let rdp_port = args
            .windows(2)
            .find(|w| w[0] == "--rdp-port")
            .and_then(|w| w[1].parse::<u16>().ok())
            .unwrap_or(cfg.rdp.port);

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

        // Parse --encoder auto|vaapi|nvenc|x264|openh264 (default auto)
        let encoder_pref = args
            .windows(2)
            .find(|w| w[0] == "--encoder")
            .map(|w| w[1].as_str())
            .unwrap_or("auto");

        // --one-shot: exit after the first RDP client disconnects
        let one_shot = args.iter().any(|a| a == "--one-shot");

        // Parse --fps N (default 60)
        let fps: u32 = args
            .windows(2)
            .find(|w| w[0] == "--fps")
            .and_then(|w| w[1].parse().ok())
            .unwrap_or(60);

        info!("starting desktopinator (headless)");
        run_headless(
            width,
            height,
            vnc_port,
            rdp_port,
            encoder_pref,
            one_shot,
            fps,
            &cfg,
        )
    } else {
        info!("starting desktopinator (winit)");
        run_winit(&cfg)
    }
}

fn run_winit(cfg: &config::Config) -> anyhow::Result<()> {
    use smithay::backend::winit::{self, WinitEvent};

    let (mut backend, winit_evt_loop) =
        winit::init::<GlesRenderer>().map_err(|e| anyhow::anyhow!("winit init failed: {e:?}"))?;

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
    let plugin_keybindings = init_plugins(&mut state);
    state.plugin_keybindings = plugin_keybindings
        .into_iter()
        .map(|kb| {
            (
                kb.keysym,
                kb.mods.0,
                kb.mods.1,
                kb.mods.2,
                kb.mods.3,
                kb.callback_id,
            )
        })
        .collect();

    output.create_global::<DinatorState>(&display_handle);
    state.space.map_output(&output, (0, 0));
    state.register_output(&output);

    // Apply config file settings
    if let Some(ref bg_spec) = cfg.background {
        if let Some(bg) = dinator_core::parse_background(bg_spec) {
            state.set_background(bg);
        }
    }
    if let Some(gap) = cfg.gap {
        state.set_layout_gap(gap);
        state.retile(&output);
    }
    if let Some(ref layout_name) = cfg.layout {
        if state.set_layout(layout_name) {
            state.retile(&output);
        }
    }

    state.seat.add_keyboard(Default::default(), 200, 25)?;
    state.seat.add_pointer();

    event_loop
        .handle()
        .insert_source(
            Generic::new(listening_socket, Interest::READ, calloop::Mode::Level),
            |_, socket, state| {
                if let Some(stream) = socket.accept()? {
                    let client_state = Arc::new(dinator_core::ClientState::default());
                    if let Err(e) = state.display_handle.insert_client(stream, client_state) {
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

    // Spawn XWayland
    if let Err(e) = spawn_xwayland(&event_loop.handle(), &display_handle) {
        tracing::warn!(error = %e, "XWayland not available — X11 apps won't work");
    }

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
                        background_clear_color(state.background_for_output(&output)),
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

                // Send frame callbacks to layer surfaces
                {
                    let layer_map = layer_map_for_output(&output);
                    for layer in layer_map.layers() {
                        layer.send_frame(&output, state.start_time.elapsed(), None, |_, _| {
                            Some(output.clone())
                        });
                    }
                }

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
    ClientConnected,
    ClientDisconnected,
}

/// RDP input events bridged from the RDP server thread to the compositor event loop.
enum RdpInputEvent {
    MouseMove {
        x: u16,
        y: u16,
    },
    MouseButton {
        button: u32,
        pressed: bool,
    },
    MouseScroll {
        value: i16,
    },
    /// Already converted to XKB keycode (evdev + 8)
    Key {
        keycode: u32,
        pressed: bool,
    },
    /// An RDP client connected (session takeover: new client gets existing session).
    ClientConnected,
    /// An RDP client disconnected. Used to release stuck keys and emit IPC events.
    ClientDisconnected,
    /// Compositor should quit (used with --one-shot to exit after first disconnect).
    Quit,
}

fn run_headless(
    width: u16,
    height: u16,
    vnc_port: u16,
    rdp_port: u16,
    encoder_pref: &str,
    one_shot: bool,
    fps: u32,
    cfg: &config::Config,
) -> anyhow::Result<()> {
    use smithay::backend::allocator::Fourcc;
    use smithay::backend::egl::context::EGLContext;
    use smithay::backend::egl::native::EGLSurfacelessDisplay;
    use smithay::backend::egl::EGLDisplay;
    use smithay::backend::input::KeyState;
    use smithay::backend::renderer::gles::GlesRenderbuffer;
    use smithay::backend::renderer::{Bind, ExportMem, Offscreen};
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

    let renderbuffer: GlesRenderbuffer = renderer
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
                            let _ = tx.send(VncInputEvent::ClientConnected);
                        }
                        ServerEvent::ClientDisconnected { client_id } => {
                            info!(client_id, "VNC client disconnected");
                            let _ = tx.send(VncInputEvent::ClientDisconnected);
                        }
                        ServerEvent::PointerMove {
                            x, y, button_mask, ..
                        } => {
                            let _ = tx.send(VncInputEvent::PointerMove { x, y, button_mask });
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

    // --- RDP Server ---
    let (rdp_pixel_tx, rdp_pixel_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(2);
    let (rdp_input_tx, rdp_input_rx): (channel::Sender<RdpInputEvent>, Channel<RdpInputEvent>) =
        channel::channel();
    // Shared sender for display updates — swapped each time a new RDP client connects
    let rdp_update_tx: Arc<
        tokio::sync::Mutex<Option<tokio::sync::mpsc::Sender<ironrdp_server::DisplayUpdate>>>,
    > = Arc::new(tokio::sync::Mutex::new(None));

    // Clipboard shared state for CLIPRDR
    let clipboard_state = Arc::new(std::sync::Mutex::new(clipboard::ClipboardState::default()));
    let clipboard_state_render = clipboard_state.clone();

    // GFX shared state for H.264 over RDP
    let gfx_state = Arc::new(std::sync::Mutex::new(gfx::GfxSharedState::new(
        width, height,
    )));
    let gfx_state_handler = gfx_state.clone();
    let gfx_state_render = gfx_state.clone();

    // DisplayControl shared state for client-driven monitor layout
    let dc_state = Arc::new(std::sync::Mutex::new(
        displaycontrol::DisplayControlState::default(),
    ));
    let dc_state_handler = dc_state.clone();
    let dc_state_render = dc_state.clone();
    // ServerEvent sender — set once the RDP server is built
    let rdp_event_tx: Arc<
        std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<ironrdp_server::ServerEvent>>>,
    > = Arc::new(std::sync::Mutex::new(None));
    let rdp_event_tx_render = rdp_event_tx.clone();
    let rdp_event_tx_clipboard = rdp_event_tx.clone();
    let rdp_update_tx_adapter = rdp_update_tx.clone();
    let rdp_width = Arc::new(std::sync::atomic::AtomicU16::new(width));
    let rdp_height = Arc::new(std::sync::atomic::AtomicU16::new(height));
    let rdp_width_adapter = rdp_width.clone();
    let rdp_height_adapter = rdp_height.clone();
    let rdp_width_render = rdp_width.clone();
    let rdp_height_render = rdp_height.clone();
    let rdp_width_input = rdp_width.clone();
    let rdp_height_input = rdp_height.clone();
    let dc_state_input = dc_state.clone();
    let rdp_update_tx_render = rdp_update_tx.clone();

    // Spawn adapter task: receives raw pixels, wraps in BitmapUpdate, sends to RDP display
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("failed to create RDP tokio runtime");
        rt.block_on(async {
            // Pixel adapter task
            let update_tx = rdp_update_tx_adapter.clone();
            let w_ref = rdp_width_adapter;
            let h_ref = rdp_height_adapter;
            let mut pixel_rx = rdp_pixel_rx;
            tokio::spawn(async move {
                while let Some(pixels) = pixel_rx.recv().await {
                    let w = w_ref.load(std::sync::atomic::Ordering::Relaxed);
                    let h = h_ref.load(std::sync::atomic::Ordering::Relaxed);
                    let guard = update_tx.lock().await;
                    if let Some(ref tx) = *guard {
                        let bitmap = ironrdp_server::BitmapUpdate {
                            x: 0,
                            y: 0,
                            width: std::num::NonZeroU16::new(w).unwrap(),
                            height: std::num::NonZeroU16::new(h).unwrap(),
                            format: ironrdp_server::PixelFormat::RgbA32,
                            data: bytes::Bytes::from(pixels),
                            stride: std::num::NonZeroUsize::new(w as usize * 4).unwrap(),
                        };
                        let _ = tx.try_send(ironrdp_server::DisplayUpdate::Bitmap(bitmap));
                    }
                }
            });

            // RDP server — generate self-signed TLS cert for RDP clients (mstsc requires TLS)
            let display = DinatorRdpDisplay {
                width: rdp_width.clone(),
                height: rdp_height.clone(),
                update_tx: rdp_update_tx.clone(),
                input_tx: rdp_input_tx.clone(),
            };
            let quit_tx = rdp_input_tx.clone();
            let input_handler = DinatorRdpInputHandler { tx: rdp_input_tx };
            let tls_acceptor = match make_rdp_tls_acceptor() {
                Ok(acceptor) => acceptor,
                Err(e) => {
                    tracing::error!(error = %e, "failed to create RDP TLS acceptor");
                    return;
                }
            };
            let cliprdr_factory = clipboard::DinatorCliprdrFactory::new(clipboard_state.clone());
            let sound_factory = audio::DinatorSoundFactory::new();
            let mut server = ironrdp_server::RdpServer::builder()
                .with_addr(([0, 0, 0, 0], rdp_port))
                .with_tls(tls_acceptor)
                .with_input_handler(input_handler)
                .with_display_handler(display)
                .with_cliprdr_factory(Some(Box::new(cliprdr_factory)))
                .with_sound_factory(Some(Box::new(sound_factory)))
                .build();

            // Use our custom DisplayControl handler (supports multi-monitor)
            server.skip_builtin_display_control();

            // Register GFX + DisplayControl DVC channels — creates new handlers per connection
            let gfx_state_for_builder = gfx_state_handler.clone();
            let dc_state_for_builder = dc_state_handler.clone();
            server.set_dvc_builder(move |dvc| {
                let gfx_handler = gfx::GfxHandler::new(gfx_state_for_builder.clone());
                let dc_handler =
                    displaycontrol::DisplayControlDvc::new(dc_state_for_builder.clone());
                dvc.with_dynamic_channel(gfx_handler)
                    .with_dynamic_channel(dc_handler)
            });

            // Share the event sender so the compositor can send GFX frames
            {
                let mut tx = rdp_event_tx.lock().unwrap();
                *tx = Some(server.event_sender().clone());
            }

            if one_shot {
                info!(
                    port = rdp_port,
                    "RDP server listening (one-shot, TLS, no auth, GFX/AVC420)"
                );
                // One-shot: accept a single connection, then signal compositor to quit
                let addr = std::net::SocketAddr::from(([0, 0, 0, 0], rdp_port));
                let listener = tokio::net::TcpListener::bind(addr)
                    .await
                    .expect("failed to bind RDP port");
                let (stream, peer) = listener
                    .accept()
                    .await
                    .expect("failed to accept RDP connection");
                info!(?peer, "RDP one-shot: client connected");
                if let Err(e) = server.run_connection(stream).await {
                    tracing::error!(error = %e, "RDP connection error");
                }
                info!("RDP one-shot: client disconnected, shutting down compositor");
                let _ = quit_tx.send(RdpInputEvent::Quit);
            } else {
                info!(
                    port = rdp_port,
                    "RDP server listening (TLS, no auth, GFX/AVC420)"
                );
                if let Err(e) = server.run().await {
                    tracing::error!(error = %e, "RDP server error");
                }
            }
        });
    });

    info!(port = rdp_port, "RDP server started");

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
    let plugin_keybindings = init_plugins(&mut state);
    state.plugin_keybindings = plugin_keybindings
        .into_iter()
        .map(|kb| {
            (
                kb.keysym,
                kb.mods.0,
                kb.mods.1,
                kb.mods.2,
                kb.mods.3,
                kb.callback_id,
            )
        })
        .collect();

    // Set up clipboard sync callback (Wayland → RDP)
    {
        let clipboard_state_cb = clipboard_state_render.clone();
        let rdp_event_tx_cb = rdp_event_tx_clipboard.clone();
        state.on_clipboard_change = Some(Box::new(move |text: String| {
            // Store the text in shared state
            {
                let mut cs = clipboard_state_cb.lock().unwrap();
                cs.wayland_text = Some(text);
                cs.rdp_owns_clipboard = false;
            }
            // Notify RDP client that clipboard changed
            if let Some(ref tx) = *rdp_event_tx_cb.lock().unwrap() {
                let formats = vec![ironrdp_cliprdr::pdu::ClipboardFormat {
                    id: ironrdp_cliprdr::pdu::ClipboardFormatId(13), // CF_UNICODETEXT
                    name: None,
                }];
                let _ = tx.send(ironrdp_server::ServerEvent::Clipboard(
                    ironrdp_cliprdr::backend::ClipboardMessage::SendInitiateCopy(formats),
                ));
            }
        }));
    }

    output.create_global::<DinatorState>(&display_handle);
    state.space.map_output(&output, (0, 0));
    state.register_output(&output);

    // Apply config file settings
    if let Some(ref bg_spec) = cfg.background {
        if let Some(bg) = dinator_core::parse_background(bg_spec) {
            state.set_background(bg);
            info!(background = %bg_spec, "config: applied background");
        } else {
            tracing::warn!(background = %bg_spec, "config: invalid background spec");
        }
    }
    if let Some(gap) = cfg.gap {
        state.set_layout_gap(gap);
        state.retile(&output);
        info!(gap, "config: applied gap");
    }
    if let Some(ref layout_name) = cfg.layout {
        if state.set_layout(layout_name) {
            state.retile(&output);
            info!(layout = %layout_name, "config: applied layout");
        } else {
            tracing::warn!(layout = %layout_name, "config: unknown layout");
        }
    }

    // Disable compositor-side key repeat for headless mode — the RDP/VNC
    // client handles its own key repeat. Using delay=0, rate=0 disables it.
    state.seat.add_keyboard(Default::default(), 0, 0)?;
    state.seat.add_pointer();

    // Accept new Wayland clients
    event_loop
        .handle()
        .insert_source(
            Generic::new(listening_socket, Interest::READ, calloop::Mode::Level),
            |_, socket, state| {
                if let Some(stream) = socket.accept()? {
                    let client_state = Arc::new(dinator_core::ClientState::default());
                    if let Err(e) = state.display_handle.insert_client(stream, client_state) {
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

    // Spawn XWayland
    if let Err(e) = spawn_xwayland(&event_loop.handle(), &display_handle) {
        tracing::warn!(error = %e, "XWayland not available — X11 apps won't work");
    }

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
                VncInputEvent::PointerMove { x, y, button_mask } => {
                    let Some(pointer) = state.seat.get_pointer() else {
                        return;
                    };
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
                                        let Some(keyboard) = state.seat.get_keyboard() else {
                                            return;
                                        };
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
                    let Some(keyboard) = state.seat.get_keyboard() else {
                        return;
                    };
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
                        let plugin_bindings = state.plugin_keybindings.clone();
                        let action = keyboard.input::<Option<KeyAction>, _>(
                            state,
                            keycode.into(),
                            key_state,
                            serial,
                            0,
                            |_state, modifiers, ksym| {
                                let sym = ksym.modified_sym();

                                // Check built-in keybindings (Alt+key)
                                if modifiers.alt {
                                    // Workspace switching: Alt+1-9, Alt+Shift+1-9
                                    let ws = keysym_to_workspace(sym.raw());
                                    if let Some(n) = ws {
                                        if key_state == KeyState::Pressed {
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
                                        keysyms::KEY_Return
                                        | keysyms::KEY_d
                                        | keysyms::KEY_j
                                        | keysyms::KEY_k
                                        | keysyms::KEY_q
                                        | keysyms::KEY_Q
                                        | keysyms::KEY_space
                                        | keysyms::KEY_h
                                        | keysyms::KEY_l
                                        | keysyms::KEY_f
                                        | keysyms::KEY_v
                                        | keysyms::KEY_m
                                        | keysyms::KEY_M
                                        | keysyms::KEY_plus
                                        | keysyms::KEY_equal
                                        | keysyms::KEY_minus
                                        | keysyms::KEY_comma
                                        | keysyms::KEY_period
                                        | keysyms::KEY_less
                                        | keysyms::KEY_greater => {
                                            if key_state == KeyState::Pressed {
                                                let action = match sym.raw() {
                                                    keysyms::KEY_Return => {
                                                        KeyAction::LaunchTerminal
                                                    }
                                                    keysyms::KEY_d => KeyAction::LaunchLauncher,
                                                    keysyms::KEY_q => KeyAction::CloseWindow,
                                                    keysyms::KEY_Q => KeyAction::Quit,
                                                    keysyms::KEY_j => KeyAction::FocusNext,
                                                    keysyms::KEY_k => KeyAction::FocusPrev,
                                                    keysyms::KEY_space => KeyAction::SwapMaster,
                                                    keysyms::KEY_h => KeyAction::MasterShrink,
                                                    keysyms::KEY_l => KeyAction::MasterGrow,
                                                    keysyms::KEY_f => KeyAction::ToggleFullscreen,
                                                    keysyms::KEY_v => KeyAction::ToggleFloat,
                                                    keysyms::KEY_m => KeyAction::CycleLayoutForward,
                                                    keysyms::KEY_M => {
                                                        KeyAction::CycleLayoutBackward
                                                    }
                                                    keysyms::KEY_plus | keysyms::KEY_equal => {
                                                        KeyAction::ResolutionUp
                                                    }
                                                    keysyms::KEY_minus => KeyAction::ResolutionDown,
                                                    keysyms::KEY_comma => {
                                                        KeyAction::FocusOutputLeft
                                                    }
                                                    keysyms::KEY_period => {
                                                        KeyAction::FocusOutputRight
                                                    }
                                                    keysyms::KEY_less => {
                                                        KeyAction::MoveToOutputLeft
                                                    }
                                                    keysyms::KEY_greater => {
                                                        KeyAction::MoveToOutputRight
                                                    }
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
                                        if key_state == KeyState::Pressed {
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
                                    state.close_focused_window();
                                }
                                KeyAction::FocusNext => state.focus_next(),
                                KeyAction::FocusPrev => state.focus_prev(),
                                KeyAction::FocusOutputLeft => state.focus_output_direction(-1),
                                KeyAction::FocusOutputRight => state.focus_output_direction(1),
                                KeyAction::MoveToOutputLeft => {
                                    state.move_window_to_output_direction(-1);
                                }
                                KeyAction::MoveToOutputRight => {
                                    state.move_window_to_output_direction(1);
                                }
                                KeyAction::SwapMaster => state.swap_master(),
                                KeyAction::MasterGrow | KeyAction::MasterShrink => {
                                    let changed = if matches!(action, KeyAction::MasterGrow) {
                                        state.grow_master()
                                    } else {
                                        state.shrink_master()
                                    };
                                    if changed {
                                        let output = state.get_focused_output();
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
                                KeyAction::CycleLayoutForward | KeyAction::CycleLayoutBackward => {
                                    let dir = if matches!(action, KeyAction::CycleLayoutForward) {
                                        1
                                    } else {
                                        -1
                                    };
                                    if let Some(name) = state.cycle_layout(dir) {
                                        let output = state.get_focused_output();
                                        if let Some(output) = output {
                                            state.retile(&output);
                                        }
                                        state.emit_event(dinator_ipc::IpcEvent::LayoutChanged {
                                            name,
                                        });
                                    }
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
                                KeyAction::PluginCallback(ref callback_id) => {
                                    info!(callback = %callback_id, "plugin keybinding");
                                    if let Some(ref mut runtime) = state.plugin_runtime {
                                        runtime.invoke_callback(callback_id);
                                    }
                                    state.execute_plugin_actions();
                                }
                                KeyAction::SwitchWorkspace(n) => {
                                    state.switch_workspace(n);
                                }
                                KeyAction::MoveToWorkspace(n) => {
                                    state.move_to_workspace(n);
                                }
                                KeyAction::Quit => {
                                    info!("keybinding: quit");
                                    state.loop_signal.stop();
                                }
                            }
                        }
                    }
                }
                VncInputEvent::ClientConnected => {
                    state.vnc_clients += 1;
                    info!(
                        clients = state.vnc_clients,
                        "VNC: client connected (session takeover)"
                    );
                    pressed_keys.clear();
                    state.emit_event(dinator_ipc::IpcEvent::ClientConnected {
                        protocol: "vnc".to_string(),
                    });
                }
                VncInputEvent::ClientDisconnected => {
                    state.vnc_clients = state.vnc_clients.saturating_sub(1);
                    info!(clients = state.vnc_clients, "VNC: client disconnected");
                    // Release all held keys to prevent stuck modifiers
                    if let Some(keyboard) = state.seat.get_keyboard() {
                        let serial = SERIAL_COUNTER.next_serial();
                        for &keycode in pressed_keys.iter() {
                            keyboard.input::<(), _>(
                                state,
                                keycode.into(),
                                KeyState::Released,
                                serial,
                                0,
                                |_, _, _| FilterResult::Forward,
                            );
                        }
                    }
                    pressed_keys.clear();
                    state.emit_event(dinator_ipc::IpcEvent::ClientDisconnected {
                        protocol: "vnc".to_string(),
                    });
                }
            }
        })
        .map_err(|e| anyhow::anyhow!("failed to insert VNC input source: {e}"))?;

    // Handle RDP input events in the compositor event loop
    let output_for_rdp = output.clone();
    let pending_resize_rdp = pending_resize.clone();
    event_loop
        .handle()
        .insert_source(rdp_input_rx, {
            let mut rdp_pressed_keys = std::collections::HashSet::<u32>::new();
            move |event, _, state| {
                let channel::Event::Msg(event) = event else {
                    return;
                };
                match event {
                    RdpInputEvent::MouseMove { x, y } => {
                        let Some(pointer) = state.seat.get_pointer() else {
                            return;
                        };
                        let serial = SERIAL_COUNTER.next_serial();
                        let pos = (x as f64, y as f64);
                        let under = state.space.element_under(pos);
                        let surface_under =
                            under.and_then(|(window, loc)| {
                                use smithay::desktop::WindowSurfaceType;
                                let rel = (pos.0 - loc.x as f64, pos.1 - loc.y as f64);
                                window.surface_under(rel, WindowSurfaceType::ALL).map(
                                    |(s, offset)| {
                                        (
                                    s,
                                    smithay::utils::Point::<f64, smithay::utils::Logical>::from((
                                        loc.x as f64 + offset.x as f64,
                                        loc.y as f64 + offset.y as f64,
                                    )),
                                )
                                    },
                                )
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
                    }
                    RdpInputEvent::MouseButton { button, pressed } => {
                        let Some(pointer) = state.seat.get_pointer() else {
                            return;
                        };
                        let serial = SERIAL_COUNTER.next_serial();
                        let btn_state = if pressed {
                            smithay::backend::input::ButtonState::Pressed
                        } else {
                            smithay::backend::input::ButtonState::Released
                        };
                        pointer.button(
                            state,
                            &ButtonEvent {
                                button,
                                state: btn_state,
                                serial,
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
                                    let Some(keyboard) = state.seat.get_keyboard() else {
                                        return;
                                    };
                                    keyboard.set_focus(
                                        state,
                                        Some(toplevel.wl_surface().clone()),
                                        SERIAL_COUNTER.next_serial(),
                                    );
                                }
                            }
                        }
                    }
                    RdpInputEvent::MouseScroll { value } => {
                        let Some(pointer) = state.seat.get_pointer() else {
                            return;
                        };
                        use smithay::backend::input::Axis;
                        use smithay::input::pointer::AxisFrame;
                        // RDP scroll values: positive = scroll down, negative = scroll up
                        // Each RDP unit is 120 per notch; convert to reasonable pixel amounts
                        let amount = value as f64 * 15.0 / 120.0;
                        let mut frame = AxisFrame::new(0);
                        frame = frame.value(Axis::Vertical, amount);
                        pointer.axis(state, frame);
                        pointer.frame(state);
                    }
                    RdpInputEvent::Key { keycode, pressed } => {
                        // Filter out RDP client-side key repeats (duplicate presses)
                        // The compositor handles its own key repeat via XKB
                        if pressed {
                            if !rdp_pressed_keys.insert(keycode) {
                                return; // already pressed, skip repeat
                            }
                        } else {
                            rdp_pressed_keys.remove(&keycode);
                        }

                        let Some(keyboard) = state.seat.get_keyboard() else {
                            return;
                        };
                        let serial = SERIAL_COUNTER.next_serial();
                        let key_state = if pressed {
                            KeyState::Pressed
                        } else {
                            KeyState::Released
                        };
                        // Check for compositor keybindings (Alt+key)
                        let plugin_bindings = state.plugin_keybindings.clone();
                        let action = keyboard.input::<Option<KeyAction>, _>(
                            state,
                            keycode.into(),
                            key_state,
                            serial,
                            0,
                            |_state, modifiers, ksym| {
                                let sym = ksym.modified_sym();
                                if modifiers.alt {
                                    let ws = keysym_to_workspace(sym.raw());
                                    if let Some(n) = ws {
                                        if key_state == KeyState::Pressed {
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
                                        keysyms::KEY_Return
                                        | keysyms::KEY_d
                                        | keysyms::KEY_j
                                        | keysyms::KEY_k
                                        | keysyms::KEY_q
                                        | keysyms::KEY_Q
                                        | keysyms::KEY_space
                                        | keysyms::KEY_h
                                        | keysyms::KEY_l
                                        | keysyms::KEY_f
                                        | keysyms::KEY_v
                                        | keysyms::KEY_m
                                        | keysyms::KEY_M
                                        | keysyms::KEY_plus
                                        | keysyms::KEY_equal
                                        | keysyms::KEY_minus
                                        | keysyms::KEY_comma
                                        | keysyms::KEY_period
                                        | keysyms::KEY_less
                                        | keysyms::KEY_greater => {
                                            if key_state == KeyState::Pressed {
                                                let action = match sym.raw() {
                                                    keysyms::KEY_Return => {
                                                        KeyAction::LaunchTerminal
                                                    }
                                                    keysyms::KEY_d => KeyAction::LaunchLauncher,
                                                    keysyms::KEY_q => KeyAction::CloseWindow,
                                                    keysyms::KEY_Q => KeyAction::Quit,
                                                    keysyms::KEY_j => KeyAction::FocusNext,
                                                    keysyms::KEY_k => KeyAction::FocusPrev,
                                                    keysyms::KEY_space => KeyAction::SwapMaster,
                                                    keysyms::KEY_h => KeyAction::MasterShrink,
                                                    keysyms::KEY_l => KeyAction::MasterGrow,
                                                    keysyms::KEY_f => KeyAction::ToggleFullscreen,
                                                    keysyms::KEY_v => KeyAction::ToggleFloat,
                                                    keysyms::KEY_m => KeyAction::CycleLayoutForward,
                                                    keysyms::KEY_M => {
                                                        KeyAction::CycleLayoutBackward
                                                    }
                                                    keysyms::KEY_plus | keysyms::KEY_equal => {
                                                        KeyAction::ResolutionUp
                                                    }
                                                    keysyms::KEY_minus => KeyAction::ResolutionDown,
                                                    keysyms::KEY_comma => {
                                                        KeyAction::FocusOutputLeft
                                                    }
                                                    keysyms::KEY_period => {
                                                        KeyAction::FocusOutputRight
                                                    }
                                                    keysyms::KEY_less => {
                                                        KeyAction::MoveToOutputLeft
                                                    }
                                                    keysyms::KEY_greater => {
                                                        KeyAction::MoveToOutputRight
                                                    }
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
                                        if key_state == KeyState::Pressed {
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
                                    state.close_focused_window();
                                }
                                KeyAction::FocusNext => state.focus_next(),
                                KeyAction::FocusPrev => state.focus_prev(),
                                KeyAction::FocusOutputLeft => state.focus_output_direction(-1),
                                KeyAction::FocusOutputRight => state.focus_output_direction(1),
                                KeyAction::MoveToOutputLeft => {
                                    state.move_window_to_output_direction(-1);
                                }
                                KeyAction::MoveToOutputRight => {
                                    state.move_window_to_output_direction(1);
                                }
                                KeyAction::SwapMaster => state.swap_master(),
                                KeyAction::MasterGrow | KeyAction::MasterShrink => {
                                    let changed = if matches!(action, KeyAction::MasterGrow) {
                                        state.grow_master()
                                    } else {
                                        state.shrink_master()
                                    };
                                    if changed {
                                        let output = state.get_focused_output();
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
                                KeyAction::CycleLayoutForward | KeyAction::CycleLayoutBackward => {
                                    let dir = if matches!(action, KeyAction::CycleLayoutForward) {
                                        1
                                    } else {
                                        -1
                                    };
                                    if let Some(name) = state.cycle_layout(dir) {
                                        let output = state.get_focused_output();
                                        if let Some(output) = output {
                                            state.retile(&output);
                                        }
                                        state.emit_event(dinator_ipc::IpcEvent::LayoutChanged {
                                            name,
                                        });
                                    }
                                }
                                KeyAction::ResolutionUp | KeyAction::ResolutionDown => {
                                    let dir = if matches!(action, KeyAction::ResolutionUp) {
                                        1
                                    } else {
                                        -1
                                    };
                                    if let Some(mode) = output_for_rdp.current_mode() {
                                        let (new_w, new_h) = next_resolution(
                                            mode.size.w as u16,
                                            mode.size.h as u16,
                                            dir,
                                        );
                                        *pending_resize_rdp.lock().unwrap() = Some((new_w, new_h));
                                    }
                                }
                                KeyAction::PluginCallback(ref callback_id) => {
                                    info!(callback = %callback_id, "plugin keybinding");
                                    if let Some(ref mut runtime) = state.plugin_runtime {
                                        runtime.invoke_callback(callback_id);
                                    }
                                    state.execute_plugin_actions();
                                }
                                KeyAction::SwitchWorkspace(n) => state.switch_workspace(n),
                                KeyAction::MoveToWorkspace(n) => state.move_to_workspace(n),
                                KeyAction::Quit => {
                                    info!("keybinding: quit");
                                    state.loop_signal.stop();
                                }
                            }
                        }
                    }
                    RdpInputEvent::ClientConnected => {
                        state.rdp_clients += 1;
                        info!(
                            clients = state.rdp_clients,
                            "RDP: client connected (session takeover)"
                        );
                        rdp_pressed_keys.clear();
                        state.emit_event(dinator_ipc::IpcEvent::ClientConnected {
                            protocol: "rdp".to_string(),
                        });
                    }
                    RdpInputEvent::ClientDisconnected => {
                        state.rdp_clients = state.rdp_clients.saturating_sub(1);
                        info!(clients = state.rdp_clients, "RDP: client disconnected");
                        // Release all held keys to prevent stuck modifiers
                        if let Some(keyboard) = state.seat.get_keyboard() {
                            let serial = SERIAL_COUNTER.next_serial();
                            for &keycode in rdp_pressed_keys.iter() {
                                keyboard.input::<(), _>(
                                    state,
                                    keycode.into(),
                                    KeyState::Released,
                                    serial,
                                    0,
                                    |_, _, _| FilterResult::Forward,
                                );
                            }
                        }
                        rdp_pressed_keys.clear();

                        // Reset RDP desktop size to primary output dimensions so the
                        // next client gets a sane initial size (GFX/DisplayControl
                        // handle multi-monitor setup after connection).
                        if let Some(primary) = state.space.outputs().next() {
                            if let Some(mode) = primary.current_mode() {
                                let pw = mode.size.w as u16;
                                let ph = mode.size.h as u16;
                                info!(
                                    width = pw,
                                    height = ph,
                                    "RDP: reset desktop size to primary output"
                                );
                                rdp_width_input.store(pw, std::sync::atomic::Ordering::Relaxed);
                                rdp_height_input.store(ph, std::sync::atomic::Ordering::Relaxed);
                            }
                        }

                        // Clear any pending DisplayControl layout from the old client
                        if let Ok(mut dc) = dc_state_input.lock() {
                            dc.pending_layout = None;
                        }

                        state.emit_event(dinator_ipc::IpcEvent::ClientDisconnected {
                            protocol: "rdp".to_string(),
                        });
                    }
                    RdpInputEvent::Quit => {
                        info!("RDP one-shot: received quit signal");
                        state.loop_signal.stop();
                    }
                }
            }
        })
        .map_err(|e| anyhow::anyhow!("failed to insert RDP input source: {e}"))?;

    // Per-output render state: renderbuffer + damage tracker
    let mut output_render_states: HashMap<String, (GlesRenderbuffer, OutputDamageTracker)> =
        HashMap::new();
    output_render_states.insert(
        output.name(),
        (renderbuffer, OutputDamageTracker::from_output(&output)),
    );
    let mut composite_width = width;
    let mut composite_height = height;
    let mut composite_buffer: Vec<u8> = vec![0u8; width as usize * height as usize * 4];
    let pending_resize_render = pending_resize.clone();

    // Per-output H.264 encoders — created on demand when outputs appear
    let mut h264_encoders: HashMap<String, Box<dyn dinator_encode::Encoder>> = HashMap::new();
    // Adaptive tile grids — used when config.rdp.adaptive is true
    let mut tile_grids: HashMap<String, adaptive::TileGrid> = HashMap::new();
    let use_adaptive = cfg.rdp.adaptive;
    let tile_cols = cfg.rdp.tile_cols;
    let tile_rows = cfg.rdp.tile_rows;
    let encoder_pref_owned = encoder_pref.to_string();
    let mut encode_frame_count: u64 = 0;
    let mut gfx_frames_dropped: u64 = 0;
    let mut last_keyframe_time = std::time::Instant::now();
    let keyframe_cooldown = Duration::from_secs(2);
    // Track last sent cursor position to avoid redundant updates
    let mut last_cursor_pos: (u16, u16) = (0, 0);
    let mut cursor_hidden = true; // start hidden, send default on first frame
    // Bandwidth throttle: track bytes sent in a rolling window to avoid
    // overwhelming the DVC channel during rapid window switching
    let mut bytes_sent_window: u64 = 0;
    let mut window_start = std::time::Instant::now();
    const MAX_BYTES_PER_SECOND: u64 = 5_000_000; // 5MB/s limit
    // Second encoder for AVC444 chroma stream (per output)
    let mut chroma_encoders: HashMap<String, Box<dyn dinator_encode::Encoder>> = HashMap::new();
    // Reusable buffers for AVC444 YUV444 conversion
    let mut yuv444_y: Vec<u8> = Vec::new();
    let mut yuv444_u: Vec<u8> = Vec::new();
    let mut yuv444_v: Vec<u8> = Vec::new();
    let mut chroma_yuv420: Vec<u8> = Vec::new();
    if use_adaptive {
        info!(tile_cols, tile_rows, "adaptive tile encoding enabled");
    }

    let frame_interval = Duration::from_micros(1_000_000 / fps as u64);
    info!(
        fps,
        interval_ms = frame_interval.as_secs_f64() * 1000.0,
        "render loop configured"
    );

    // Timer-based redraw
    let timer = Timer::immediate();
    event_loop
        .handle()
        .insert_source(timer, move |_, _, state| {
            // Collect current outputs
            let mut outputs: Vec<Output> = state.space.outputs().cloned().collect();

            // Check for new RDP clipboard text → set as Wayland clipboard
            {
                let mut cs = clipboard_state_render.lock().unwrap();
                if cs.rdp_owns_clipboard {
                    if let Some(text) = cs.rdp_text.take() {
                        cs.rdp_owns_clipboard = false;
                        state.rdp_clipboard_text = Some(text);
                        smithay::wayland::selection::data_device::set_data_device_selection(
                            &state.display.handle(),
                            &state.seat,
                            vec![
                                "text/plain;charset=utf-8".to_string(),
                                "text/plain".to_string(),
                                "UTF8_STRING".to_string(),
                                "STRING".to_string(),
                            ],
                            (),
                        );
                        info!("clipboard: set Wayland selection from RDP text");
                    }
                }
            }

            // Sync per-output render state with current outputs
            for o in &outputs {
                let name = o.name();
                if !output_render_states.contains_key(&name) {
                    if let Some(geo) = state.space.output_geometry(o) {
                        match renderer.create_buffer(Fourcc::Abgr8888, Size::from((geo.size.w, geo.size.h))) {
                            Ok(rb) => {
                                let dt = OutputDamageTracker::from_output(o);
                                output_render_states.insert(name.clone(), (rb, dt));
                                info!(output = %name, w = geo.size.w, h = geo.size.h, "created renderbuffer for new output");
                            }
                            Err(e) => {
                                tracing::error!(output = %name, "failed to create renderbuffer: {e:?}");
                            }
                        }
                    }
                }
            }
            // Remove render state and encoders for removed outputs
            let live_names: Vec<String> = outputs.iter().map(|o| o.name()).collect();
            output_render_states.retain(|name, _| live_names.contains(name));
            h264_encoders.retain(|name, _| live_names.contains(name));

            // Ensure per-output encoders exist
            for o in &outputs {
                let name = o.name();
                if !h264_encoders.contains_key(&name) {
                    if let Some(geo) = state.space.output_geometry(o) {
                        let w = geo.size.w as u32;
                        let h = geo.size.h as u32;
                        if let Some(enc) = create_encoder(w, h, &encoder_pref_owned) {
                            info!(output = %name, w, h, encoder = enc.name(), "created H.264 encoder for output");
                            h264_encoders.insert(name, enc);
                        }
                    }
                }
            }

            // Check for pending resolution change (applies to focused output)
            if let Some((new_w, new_h)) = pending_resize_render.lock().unwrap().take() {
                if let Some(focused) = state.get_focused_output() {
                    let name = focused.name();
                    info!(output = %name, new_w, new_h, "applying resolution change");

                    let mode = Mode {
                        size: (new_w as i32, new_h as i32).into(),
                        refresh: 60_000,
                    };
                    focused.change_current_state(Some(mode), None, None, None);
                    focused.set_preferred(mode);

                    // Recreate renderbuffer for this output
                    match renderer.create_buffer(Fourcc::Abgr8888, Size::from((new_w as i32, new_h as i32))) {
                        Ok(new_rb) => {
                            let dt = OutputDamageTracker::from_output(&focused);
                            output_render_states.insert(name.clone(), (new_rb, dt));
                        }
                        Err(e) => {
                            tracing::error!("failed to create renderbuffer for resize: {e:?}");
                        }
                    }

                    // Recreate encoder for this output at new size
                    h264_encoders.remove(&name);
                    if let Some(enc) = create_encoder(new_w as u32, new_h as u32, &encoder_pref_owned) {
                        h264_encoders.insert(name, enc);
                    }

                    state.retile(&focused);
                }
            }

            // Check for pending DisplayControl monitor layout from RDP client
            if let Some(monitors) = dc_state_render.lock().unwrap().pending_layout.take() {
                apply_monitor_layout(
                    &monitors,
                    state,
                    &mut output_render_states,
                    &mut h264_encoders,
                    &encoder_pref_owned,
                    &mut renderer,
                );
                // Re-collect outputs after layout change
                outputs = state.space.outputs().cloned().collect();
            }

            // Calculate composite dimensions (bounding box of all outputs)
            let (new_cw, new_ch) = {
                let mut max_x = 0i32;
                let mut max_y = 0i32;
                for o in &outputs {
                    if let Some(geo) = state.space.output_geometry(o) {
                        max_x = max_x.max(geo.loc.x + geo.size.w);
                        max_y = max_y.max(geo.loc.y + geo.size.h);
                    }
                }
                (max_x.max(1) as u16, max_y.max(1) as u16)
            };

            // Build GFX output info from current outputs
            let gfx_outputs: Vec<gfx::GfxOutputInfo> = outputs
                .iter()
                .enumerate()
                .filter_map(|(i, o)| {
                    state.space.output_geometry(o).map(|geo| gfx::GfxOutputInfo {
                        name: o.name(),
                        surface_id: i as u16,
                        x: geo.loc.x as u32,
                        y: geo.loc.y as u32,
                        width: geo.size.w as u16,
                        height: geo.size.h as u16,
                    })
                })
                .collect();

            // Update GFX shared state with current output layout
            {
                let mut gfx = gfx_state_render.lock().unwrap();
                gfx.outputs = gfx_outputs.clone();
                gfx.composite_width = new_cw;
                gfx.composite_height = new_ch;
            }

            // Resize VNC/RDP if composite dimensions changed
            if new_cw != composite_width || new_ch != composite_height {
                info!(
                    old_w = composite_width, old_h = composite_height,
                    new_w = new_cw, new_h = new_ch,
                    "composite dimensions changed"
                );
                composite_width = new_cw;
                composite_height = new_ch;
                composite_buffer = vec![0u8; composite_width as usize * composite_height as usize * 4];

                let _ = vnc_resize_tx.try_send((composite_width, composite_height));

                // RDP desktop size tracks PRIMARY output only (not composite).
                // GFX handles multi-monitor via per-surface rendering.
                // Bitmap fallback clients only see the primary output.
                if let Some(primary) = state.space.outputs().next() {
                    if let Some(mode) = primary.current_mode() {
                        let pw = mode.size.w as u16;
                        let ph = mode.size.h as u16;
                        let old_rw = rdp_width_render.load(std::sync::atomic::Ordering::Relaxed);
                        let old_rh = rdp_height_render.load(std::sync::atomic::Ordering::Relaxed);
                        if pw != old_rw || ph != old_rh {
                            rdp_width_render.store(pw, std::sync::atomic::Ordering::Relaxed);
                            rdp_height_render.store(ph, std::sync::atomic::Ordering::Relaxed);
                            if let Ok(guard) = rdp_update_tx_render.try_lock() {
                                if let Some(ref tx) = *guard {
                                    let _ = tx.try_send(ironrdp_server::DisplayUpdate::Resize(
                                        ironrdp_server::DesktopSize {
                                            width: pw,
                                            height: ph,
                                        },
                                    ));
                                }
                            }
                        }
                    }
                }

                // Reset GFX surfaces for new layout
                {
                    let gfx = gfx_state_render.lock().unwrap();
                    if gfx.ready {
                        let old_outputs = gfx.outputs.clone();
                        if let Some(channel_id) = gfx.channel_id {
                            match gfx::build_reset_surface_pdus(&old_outputs, &gfx_outputs, composite_width, composite_height) {
                                Ok(data) => {
                                    if let Some(ref tx) = *rdp_event_tx_render.lock().unwrap() {
                                        let _ = tx.send(ironrdp_server::ServerEvent::Dvc {
                                            channel_id,
                                            data,
                                        });
                                        info!(
                                            width = composite_width, height = composite_height,
                                            outputs = gfx_outputs.len(),
                                            "GFX: sent multi-surface reset"
                                        );
                                    }
                                }
                                Err(e) => tracing::warn!(error = %e, "GFX: failed to build reset PDUs"),
                            }
                        }
                        // Force keyframe on all encoders
                        for enc in h264_encoders.values_mut() {
                            enc.force_keyframe();
                        }
                        for enc in chroma_encoders.values_mut() {
                            enc.force_keyframe();
                        }
                        for grid in tile_grids.values_mut() {
                            grid.force_all_keyframes();
                        }
                    }
                }

                state.emit_event(dinator_ipc::IpcEvent::ResolutionChanged {
                    width: composite_width,
                    height: composite_height,
                });
            }

            // Render each output and collect per-output pixel data
            struct DirtyOutput {
                name: String,
                x: usize,
                y: usize,
                width: u16,
                height: u16,
                pixels: Vec<u8>, // RGBA from GL readback
                damage_rects: Vec<Rectangle<i32, Physical>>,
            }
            let mut dirty_outputs: Vec<DirtyOutput> = Vec::new();

            for o in &outputs {
                let name = o.name();
                let Some((ref mut rb, ref mut dt)) = output_render_states.get_mut(&name) else { continue };
                let Some(geo) = state.space.output_geometry(o) else { continue };
                let ow = geo.size.w as u16;
                let oh = geo.size.h as u16;

                match renderer.bind(rb) {
                    Ok(mut target) => {
                        let damage_rects: Vec<Rectangle<i32, Physical>> = if let Some(elements) = build_render_elements_no_cursor(&mut renderer, state, o) {
                            match dt.render_output(
                                &mut renderer,
                                &mut target,
                                0,
                                &elements,
                                background_clear_color(state.background_for_output(o)),
                            ) {
                                Ok(result) => result.damage
                                    .map(|d| d.to_vec())
                                    .unwrap_or_default(),
                                Err(_) => Vec::new(),
                            }
                        } else {
                            Vec::new()
                        };

                        if !damage_rects.is_empty() {
                            let region = Rectangle::from_size(
                                Size::from((ow as i32, oh as i32)),
                            );
                            if let Ok(mapping) = renderer.copy_framebuffer(&target, region, Fourcc::Abgr8888) {
                                if let Ok(pixels) = renderer.map_texture(&mapping) {
                                    dirty_outputs.push(DirtyOutput {
                                        name: name.clone(),
                                        x: geo.loc.x as usize,
                                        y: geo.loc.y as usize,
                                        width: ow,
                                        height: oh,
                                        pixels: pixels.to_vec(),
                                        damage_rects,
                                    });
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!(output = %name, "failed to bind renderbuffer: {e:?}");
                    }
                }
            }

            // Check for pending GFX response (runs every tick for negotiation)
            {
                let mut gfx = gfx_state_render.lock().unwrap();
                if let Some(resp) = gfx.pending_response.take() {
                    if let Some(ref tx) = *rdp_event_tx_render.lock().unwrap() {
                        let _ = tx.send(ironrdp_server::ServerEvent::Dvc {
                            channel_id: resp.channel_id,
                            data: resp.data,
                        });
                        gfx.ready = true;
                        info!("GFX: sent negotiation response via ServerEvent::Dvc");
                        // Force full damage on next render — reset all damage trackers
                        for o in &outputs {
                            if let Some((_, ref mut dt)) = output_render_states.get_mut(&o.name()) {
                                *dt = OutputDamageTracker::from_output(o);
                            }
                        }
                        // Force keyframe on all per-output encoders
                        for grid in tile_grids.values_mut() {
                            grid.force_all_keyframes();
                        }
                        for enc in h264_encoders.values_mut() {
                            enc.force_keyframe();
                        }
                        for enc in chroma_encoders.values_mut() {
                            enc.force_keyframe();
                        }
                        info!("GFX: forced H.264 keyframes on {} encoders", h264_encoders.len());
                    }
                }
            }

            if !dirty_outputs.is_empty() {
                // Composite into combined buffer for VNC and RDP bitmap fallback
                for d in &dirty_outputs {
                    let row_bytes = d.width as usize * 4;
                    let stride = composite_width as usize * 4;
                    for row in 0..d.height as usize {
                        let src_start = row * row_bytes;
                        let dst_start = (d.y + row) * stride + d.x * 4;
                        if src_start + row_bytes <= d.pixels.len()
                            && dst_start + row_bytes <= composite_buffer.len()
                        {
                            composite_buffer[dst_start..dst_start + row_bytes]
                                .copy_from_slice(&d.pixels[src_start..src_start + row_bytes]);
                        }
                    }
                }

                // R/B swap composite for VNC (RGBA→BGRA)
                let mut vnc_frame = composite_buffer.clone();
                for chunk in vnc_frame.chunks_exact_mut(4) {
                    chunk.swap(0, 2);
                }
                let _ = pixel_tx.try_send(vnc_frame.clone());

                // Per-surface GFX encoding (each output gets its own H.264 stream)
                let gfx_ready = gfx_state_render.lock().unwrap().ready;
                if gfx_ready {
                    for d in &dirty_outputs {
                        let surface_id = gfx_state_render.lock().unwrap()
                            .surface_id_for_output(&d.name)
                            .unwrap_or(0);

                        // R/B swap per-output pixels for H.264 (RGBA→BGRA)
                        let mut bgra = d.pixels.clone();
                        for chunk in bgra.chunks_exact_mut(4) {
                            chunk.swap(0, 2);
                        }

                        if use_adaptive {
                            // --- Adaptive tile path ---
                            // Use tile grid for damage-aware encoding, but send
                            // each tile as its own full GFX frame for compatibility
                            let grid = tile_grids.entry(d.name.clone()).or_insert_with(|| {
                                adaptive::TileGrid::new(
                                    d.name.clone(),
                                    d.width,
                                    d.height,
                                    tile_cols,
                                    tile_rows,
                                    &encoder_pref_owned,
                                )
                                .expect("failed to create tile grid")
                            });

                            grid.mark_damage(&d.damage_rects);
                            grid.stagger_keyframe(encode_frame_count);
                            let tile_frames = grid.encode_dirty_tiles(&bgra);

                            encode_frame_count += 1;

                            if !tile_frames.is_empty() {
                                let frame_id = gfx_state_render.lock().unwrap().next_frame_id;
                                let total_bytes: usize = tile_frames.iter().map(|t| t.data.len()).sum();

                                if encode_frame_count % 60 == 1 {
                                    info!(
                                        output = %d.name,
                                        frame_id,
                                        tiles = tile_frames.len(),
                                        total_bytes,
                                        "GFX: adaptive tile batch"
                                    );
                                }

                                // Batch all tiles into one StartFrame/EndFrame
                                match gfx::encode_gfx_avc420_tiles(&tile_frames, surface_id, frame_id) {
                                    Ok(gfx_data) if !gfx_data.is_empty() => {
                                        let channel_id = gfx_state_render.lock().unwrap().channel_id;
                                        if let Some(channel_id) = channel_id {
                                            if let Some(ref tx) = *rdp_event_tx_render.lock().unwrap() {
                                                let _ = tx.send(ironrdp_server::ServerEvent::Dvc {
                                                    channel_id,
                                                    data: gfx_data,
                                                });
                                                if tile_frames.iter().any(|t| t.is_keyframe) {
                                                    last_keyframe_time = std::time::Instant::now();
                                                }
                                            }
                                        }
                                        gfx_state_render.lock().unwrap().next_frame_id = frame_id + 1;
                                    }
                                    Ok(_) => {}
                                    Err(e) => {
                                        tracing::warn!(output = %d.name, error = %e, "GFX tile batch failed");
                                    }
                                }
                            }
                        } else {
                            // --- Full-frame path (original) ---
                            let Some(encoder) = h264_encoders.get_mut(&d.name) else { continue };

                            match encoder.encode(&bgra, d.width as u32, d.height as u32) {
                                Ok(Some(encoded)) => {
                                    encode_frame_count += 1;
                                    if encode_frame_count % 60 == 1 {
                                        info!(
                                            output = %d.name,
                                            encoder = encoder.name(),
                                            frame = encode_frame_count,
                                            bytes = encoded.data.len(),
                                            keyframe = encoded.is_keyframe,
                                            "H.264 encode"
                                        );
                                    }

                                    let h264_len = encoded.data.len();
                                    const MAX_GFX_FRAME_BYTES: usize = 512_000;

                                    // Reset bandwidth window every second
                                    if window_start.elapsed() >= Duration::from_secs(1) {
                                        bytes_sent_window = 0;
                                        window_start = std::time::Instant::now();
                                    }

                                    // Bandwidth throttle: skip non-keyframe if over limit
                                    if bytes_sent_window > MAX_BYTES_PER_SECOND && !encoded.is_keyframe {
                                        gfx_state_render.lock().unwrap().next_frame_id += 1;
                                    } else if h264_len < 50 && !encoded.is_keyframe {
                                        // No visual change — skip
                                    } else if h264_len > MAX_GFX_FRAME_BYTES && !encoded.is_keyframe {
                                        gfx_frames_dropped += 1;
                                        if gfx_frames_dropped <= 3 || gfx_frames_dropped % 30 == 0 {
                                            info!(
                                                output = %d.name,
                                                h264_bytes = h264_len,
                                                dropped = gfx_frames_dropped,
                                                "GFX: dropping oversized P-frame"
                                            );
                                        }
                                        if last_keyframe_time.elapsed() > keyframe_cooldown {
                                            encoder.force_keyframe();
                                        }
                                        gfx_state_render.lock().unwrap().next_frame_id += 1;
                                    } else {
                                        let gfx_lock = gfx_state_render.lock().unwrap();
                                        let frame_id = gfx_lock.next_frame_id;
                                        let use_avc444 = gfx_lock.avc444_supported;
                                        drop(gfx_lock);

                                        // AVC444 dual-stream disabled — see below
                                        let avc444_dual_stream = false;
                                        let gfx_result = if use_avc444 && avc444_dual_stream {
                                            // AVC444v2 with SEPARATE chroma encoder
                                            // (not shared — shared encoder corrupts reference chain)
                                            let w = d.width as u32;
                                            let h = d.height as u32;
                                            let px_count = (w * h) as usize;
                                            let i420_size = px_count * 3 / 2;

                                            yuv444_y.resize(px_count, 0);
                                            yuv444_u.resize(px_count, 0);
                                            yuv444_v.resize(px_count, 0);
                                            chroma_yuv420.resize(i420_size, 0);

                                            dinator_encode::bgra_to_yuv444(
                                                &bgra, w, h,
                                                &mut yuv444_y, &mut yuv444_u, &mut yuv444_v,
                                            );
                                            dinator_encode::pack_avc444v2_chroma(
                                                &yuv444_u, &yuv444_v, w, h,
                                                &mut chroma_yuv420,
                                            );

                                            // Use SEPARATE encoder for chroma stream
                                            let chroma_enc = chroma_encoders
                                                .entry(d.name.clone())
                                                .or_insert_with(|| {
                                                    create_encoder(w, h, &encoder_pref_owned)
                                                        .expect("chroma encoder")
                                                });
                                            if encoded.is_keyframe {
                                                chroma_enc.force_keyframe();
                                            }
                                            let chroma_h264 = chroma_enc
                                                .encode_i420(&chroma_yuv420, w, h)
                                                .ok()
                                                .flatten();

                                            // Build stream1 (luma)
                                            let stream1 = {
                                                use ironrdp_pdu::rdp::vc::dvc::gfx::Avc420BitmapStream;
                                                let s = Avc420BitmapStream {
                                                    rectangles: vec![ironrdp_pdu::geometry::InclusiveRectangle {
                                                        left: 0, top: 0,
                                                        right: d.width.saturating_sub(1),
                                                        bottom: d.height.saturating_sub(1),
                                                    }],
                                                    quant_qual_vals: vec![ironrdp_pdu::rdp::vc::dvc::gfx::QuantQuality {
                                                        quantization_parameter: 22,
                                                        progressive: false,
                                                        quality: 100,
                                                    }],
                                                    data: &encoded.data,
                                                };
                                                ironrdp_core::encode_vec(&s).unwrap()
                                            };

                                            // Build stream2 (chroma) with full region rects
                                            let stream2 = if let Some(ref s2) = chroma_h264 {
                                                use ironrdp_pdu::rdp::vc::dvc::gfx::Avc420BitmapStream;
                                                let s = Avc420BitmapStream {
                                                    rectangles: vec![ironrdp_pdu::geometry::InclusiveRectangle {
                                                        left: 0, top: 0,
                                                        right: d.width.saturating_sub(1),
                                                        bottom: d.height.saturating_sub(1),
                                                    }],
                                                    quant_qual_vals: vec![ironrdp_pdu::rdp::vc::dvc::gfx::QuantQuality {
                                                        quantization_parameter: 22,
                                                        progressive: false,
                                                        quality: 100,
                                                    }],
                                                    data: &s2.data,
                                                };
                                                ironrdp_core::encode_vec(&s).unwrap()
                                            } else {
                                                vec![0u8; 4]
                                            };

                                            let header: u32 = stream1.len() as u32 & 0x3FFFFFFF;
                                            let mut payload = Vec::with_capacity(4 + stream1.len() + stream2.len());
                                            payload.extend_from_slice(&header.to_le_bytes());
                                            payload.extend_from_slice(&stream1);
                                            payload.extend_from_slice(&stream2);

                                            gfx::encode_gfx_avc444_raw(
                                                &payload,
                                                surface_id,
                                                d.width,
                                                d.height,
                                                frame_id,
                                            )
                                        } else if use_avc444 {
                                            // AVC444 LC=1 (luma-only) — dual-stream disabled
                                            gfx::encode_gfx_avc444_frame(
                                                &encoded.data,
                                                None,
                                                surface_id,
                                                d.width,
                                                d.height,
                                                frame_id,
                                            )
                                        } else {
                                            gfx::encode_gfx_avc420_frame(
                                                &encoded.data,
                                                surface_id,
                                                d.width,
                                                d.height,
                                                frame_id,
                                            )
                                        };
                                        match gfx_result {
                                            Ok(gfx_data) => {
                                                let channel_id = gfx_state_render.lock().unwrap().channel_id;
                                                if let Some(channel_id) = channel_id {
                                                    if let Some(ref tx) = *rdp_event_tx_render.lock().unwrap() {
                                                        if frame_id % 60 == 0 || frame_id <= 5 || encoded.is_keyframe {
                                                            info!(
                                                                output = %d.name,
                                                                frame_id,
                                                                surface_id,
                                                                h264_bytes = h264_len,
                                                                gfx_bytes = gfx_data.len(),
                                                                keyframe = encoded.is_keyframe,
                                                                "GFX: sending per-surface frame"
                                                            );
                                                        }
                                                        bytes_sent_window += gfx_data.len() as u64;
                                                        let _ = tx.send(ironrdp_server::ServerEvent::Dvc {
                                                            channel_id,
                                                            data: gfx_data,
                                                        });
                                                        if encoded.is_keyframe {
                                                            last_keyframe_time = std::time::Instant::now();
                                                        }
                                                        gfx_frames_dropped = 0;
                                                    }
                                                }
                                                gfx_state_render.lock().unwrap().next_frame_id = frame_id + 1;
                                            }
                                            Err(e) => {
                                                tracing::warn!(output = %d.name, error = %e, "GFX frame encode failed");
                                            }
                                        }
                                    }
                                }
                                Ok(None) => {}
                                Err(e) => {
                                    tracing::warn!(output = %d.name, error = %e, "H.264 encode failed");
                                }
                            }
                        }
                    }
                }

                // Send to RDP bitmap pipeline only if GFX is NOT active
                if !gfx_ready {
                    let _ = rdp_pixel_tx.try_send(vnc_frame);
                }
            }

            // Send cursor shape + position via RDP pointer updates
            if state.rdp_clients > 0 {
                // Process pending cursor shape changes
                if let Some(cursor_status) = state.pending_cursor.take() {
                    use smithay::input::pointer::CursorImageStatus;

                    let update = match cursor_status {
                        CursorImageStatus::Hidden => {
                            cursor_hidden = true;
                            Some(ironrdp_server::DisplayUpdate::HidePointer)
                        }
                        CursorImageStatus::Named(_icon) => {
                            // Named cursor — send default system pointer
                            // TODO: render specific cursor shapes for different icons
                            cursor_hidden = false;
                            Some(ironrdp_server::DisplayUpdate::DefaultPointer)
                        }
                        CursorImageStatus::Surface(ref surface) => {
                            // Custom cursor from Wayland client — extract RGBA pixels
                            cursor_hidden = false;
                            extract_cursor_surface(surface)
                        }
                    };

                    if let Some(update) = update {
                        if let Ok(guard) = rdp_update_tx_render.try_lock() {
                            if let Some(ref tx) = *guard {
                                let _ = tx.try_send(update);
                            }
                        }
                    }
                }

                // Send cursor position
                if !cursor_hidden {
                    if let Some(pointer) = state.seat.get_pointer() {
                        let pos = pointer.current_location();
                        let cx = pos.x.max(0.0) as u16;
                        let cy = pos.y.max(0.0) as u16;

                        if (cx, cy) != last_cursor_pos {
                            last_cursor_pos = (cx, cy);
                            if let Ok(guard) = rdp_update_tx_render.try_lock() {
                                if let Some(ref tx) = *guard {
                                    let _ = tx.try_send(
                                        ironrdp_server::DisplayUpdate::PointerPosition(
                                            ironrdp_pdu::pointer::Point16 { x: cx, y: cy },
                                        ),
                                    );
                                }
                            }
                        }
                    }
                }
            }

            // Send frame callbacks to clients on all outputs
            let elapsed = state.start_time.elapsed();
            for o in &outputs {
                state.space.elements().for_each(|window| {
                    window.send_frame(o, elapsed, None, |_, _| {
                        Some(o.clone())
                    });
                });

                let layer_map = layer_map_for_output(o);
                for layer in layer_map.layers() {
                    layer.send_frame(o, elapsed, None, |_, _| {
                        Some(o.clone())
                    });
                }
            }

            state.space.refresh();

            TimeoutAction::ToDuration(frame_interval)
        })
        .map_err(|e| anyhow::anyhow!("failed to insert timer source: {e}"))?;

    info!(
        vnc_port,
        "entering event loop (headless) -- VNC on :{vnc_port}, RDP on :{rdp_port}, launch clients with WAYLAND_DISPLAY={socket_name}"
    );

    event_loop
        .run(frame_interval, &mut state, |state| {
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

/// Spawn XWayland and register it with the event loop.
pub(crate) fn spawn_xwayland(
    handle: &calloop::LoopHandle<'static, DinatorState>,
    display_handle: &smithay::reexports::wayland_server::DisplayHandle,
) -> anyhow::Result<()> {
    use smithay::xwayland::{X11Wm, XWayland, XWaylandEvent};
    use std::process::Stdio;

    let env_vars: Vec<(String, String)> = vec![];
    let (xwayland, xwayland_client) = XWayland::spawn(
        display_handle,
        None::<u32>,    // auto-pick display number
        env_vars,       // no extra env vars
        true,           // open abstract socket
        Stdio::piped(), // stdout
        Stdio::piped(), // stderr
        |_| {},         // user_data
    )
    .context("failed to spawn XWayland")?;

    let xwl_client = xwayland_client.clone();
    let loop_handle = handle.clone();
    handle
        .insert_source(xwayland, move |event, _, state| match event {
            XWaylandEvent::Ready {
                x11_socket,
                display_number,
            } => {
                info!(display = display_number, "XWayland ready");
                std::env::set_var("DISPLAY", format!(":{display_number}"));

                match X11Wm::start_wm(loop_handle.clone(), x11_socket, xwl_client.clone()) {
                    Ok(wm) => {
                        state.x11_wm = Some(wm);
                        info!("X11 window manager started");
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "failed to start X11 window manager");
                    }
                }
            }
            XWaylandEvent::Error => {
                tracing::error!("XWayland startup error");
            }
        })
        .map_err(|e| anyhow::anyhow!("failed to insert XWayland source: {e}"))?;

    info!("XWayland spawned");
    Ok(())
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

/// Extract RGBA cursor bitmap from a Wayland surface set via wl_pointer.set_cursor.
fn extract_cursor_surface(
    surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
) -> Option<ironrdp_server::DisplayUpdate> {
    use smithay::input::pointer::CursorImageSurfaceData;
    use smithay::wayland::compositor;
    use smithay::wayland::shm;

    let result: Option<(u16, u16, Vec<u8>, u16, u16)> =
        compositor::with_states(surface, |states| {
            let hotspot = states
                .data_map
                .get::<CursorImageSurfaceData>()
                .map(|d| {
                    let a = d.lock().unwrap();
                    (a.hotspot.x as u16, a.hotspot.y as u16)
                })
                .unwrap_or((0, 0));

            let mut cached = states
                .cached_state
                .get::<compositor::SurfaceAttributes>();
            let attrs = cached.current();

            let buf = match &attrs.buffer {
                Some(compositor::BufferAssignment::NewBuffer(b)) => b,
                _ => return None,
            };

            let pixels = shm::with_buffer_contents(buf, |ptr, len, info| {
                let data = unsafe { std::slice::from_raw_parts(ptr, len) };
                let w = info.width as usize;
                let h = info.height as usize;
                let stride = info.stride as usize;
                let mut rgba = vec![0u8; w * h * 4];
                for y in 0..h {
                    for x in 0..w {
                        let src = y * stride + x * 4;
                        let dst = (y * w + x) * 4;
                        if src + 4 <= data.len() {
                            rgba[dst] = data[src + 2];
                            rgba[dst + 1] = data[src + 1];
                            rgba[dst + 2] = data[src];
                            rgba[dst + 3] = data[src + 3];
                        }
                    }
                }
                (rgba, w as u16, h as u16)
            });

            match pixels {
                Ok((data, w, h)) if w > 0 && h > 0 => Some((hotspot.0, hotspot.1, data, w, h)),
                _ => None,
            }
        });

    match result {
        Some((hx, hy, data, w, h)) => Some(ironrdp_server::DisplayUpdate::RGBAPointer(
            ironrdp_server::RGBAPointer {
                width: w,
                height: h,
                hot_x: hx,
                hot_y: hy,
                data,
            },
        )),
        None => Some(ironrdp_server::DisplayUpdate::DefaultPointer),
    }
}

pub(crate) fn background_clear_color(bg: &dinator_core::Background) -> [f32; 4] {
    match bg {
        dinator_core::Background::Solid(c) => *c,
        // For gradients, use bottom color as clear (bands render on top)
        dinator_core::Background::Gradient { bottom, .. } => *bottom,
    }
}

enum KeyAction {
    LaunchTerminal,
    LaunchLauncher,
    CloseWindow,
    FocusNext,
    FocusPrev,
    SwapMaster,
    MasterGrow,
    MasterShrink,
    ToggleFullscreen,
    ToggleFloat,
    CycleLayoutForward,
    CycleLayoutBackward,
    ResolutionUp,
    ResolutionDown,
    FocusOutputLeft,
    FocusOutputRight,
    MoveToOutputLeft,
    MoveToOutputRight,
    Quit,
    PluginCallback(String),
    SwitchWorkspace(usize),
    MoveToWorkspace(usize),
}

/// A registered plugin keybinding.
pub(crate) struct PluginKeybinding {
    /// Required modifier flags: (alt, ctrl, shift, logo).
    pub mods: (bool, bool, bool, bool),
    /// The XKB keysym to match.
    pub keysym: u32,
    /// The callback ID to invoke.
    pub callback_id: String,
}

/// Create an H.264 encoder with the given preference.
/// Apply a monitor layout from the RDP DisplayControl channel.
///
/// Compares the requested layout with current outputs and creates/removes/resizes
/// as needed. Primary monitor keeps the existing primary output name; additional
/// monitors get names like "rdp-1", "rdp-2", etc.
fn apply_monitor_layout(
    monitors: &[displaycontrol::MonitorEntry],
    state: &mut DinatorState,
    output_render_states: &mut HashMap<
        String,
        (
            smithay::backend::renderer::gles::GlesRenderbuffer,
            OutputDamageTracker,
        ),
    >,
    h264_encoders: &mut HashMap<String, Box<dyn dinator_encode::Encoder>>,
    encoder_pref: &str,
    renderer: &mut smithay::backend::renderer::gles::GlesRenderer,
) {
    use smithay::backend::allocator::Fourcc;
    use smithay::backend::renderer::Offscreen;
    use smithay::utils::Size;

    if monitors.is_empty() {
        return;
    }

    info!(
        count = monitors.len(),
        "applying DisplayControl monitor layout"
    );

    // Sort: primary first, then by position (left, top)
    let mut sorted: Vec<(usize, &displaycontrol::MonitorEntry)> =
        monitors.iter().enumerate().collect();
    sorted.sort_by(|(_, a), (_, b)| {
        b.is_primary
            .cmp(&a.is_primary)
            .then(a.left.cmp(&b.left))
            .then(a.top.cmp(&b.top))
    });

    // Current output names
    let current_names: Vec<String> = state.space.outputs().map(|o| o.name()).collect();

    // Build target output list: name, position, size
    let mut target_outputs: Vec<(String, i32, i32, u16, u16)> = Vec::new();
    for (i, (_, m)) in sorted.iter().enumerate() {
        let name = if i == 0 {
            // Primary monitor keeps the first existing output's name
            current_names
                .first()
                .cloned()
                .unwrap_or_else(|| "headless-0".to_string())
        } else {
            format!("rdp-{i}")
        };
        let w = m.width.min(8192) as u16;
        let h = m.height.min(8192) as u16;
        target_outputs.push((name, m.left, m.top, w, h));
    }

    let target_names: Vec<String> = target_outputs
        .iter()
        .map(|(n, _, _, _, _)| n.clone())
        .collect();

    // Remove outputs not in target list
    let to_remove: Vec<Output> = state
        .space
        .outputs()
        .filter(|o| !target_names.contains(&o.name()))
        .cloned()
        .collect();
    for output in &to_remove {
        if state.output_states.len() <= 1 {
            break; // never remove the last output
        }
        let name = output.name();
        info!(output = %name, "DisplayControl: removing output");
        state.unregister_output(output);
        output_render_states.remove(&name);
        h264_encoders.remove(&name);
        state.emit_event(dinator_ipc::IpcEvent::OutputRemoved { name });
    }

    // Snapshot existing outputs so we can look them up without borrowing state
    let existing_outputs: HashMap<String, Output> = state
        .space
        .outputs()
        .map(|o| (o.name(), o.clone()))
        .collect();

    // Create or resize outputs
    for (name, left, top, w, h) in &target_outputs {
        let mode = Mode {
            size: (*w as i32, *h as i32).into(),
            refresh: 60_000,
        };

        if let Some(existing) = existing_outputs.get(name) {
            // Resize if dimensions changed
            let needs_resize = existing
                .current_mode()
                .map(|m| m.size.w != *w as i32 || m.size.h != *h as i32)
                .unwrap_or(true);

            if needs_resize {
                info!(output = %name, w, h, "DisplayControl: resizing output");
                existing.change_current_state(Some(mode), None, None, None);
                existing.set_preferred(mode);

                // Recreate renderbuffer
                match renderer.create_buffer(Fourcc::Abgr8888, Size::from((*w as i32, *h as i32))) {
                    Ok(new_rb) => {
                        let dt = OutputDamageTracker::from_output(existing);
                        output_render_states.insert(name.clone(), (new_rb, dt));
                    }
                    Err(e) => {
                        tracing::error!(output = %name, "failed to create renderbuffer: {e:?}")
                    }
                }

                // Recreate encoder
                h264_encoders.remove(name);
                if let Some(enc) = create_encoder(*w as u32, *h as u32, encoder_pref) {
                    h264_encoders.insert(name.clone(), enc);
                }

                state.retile(existing);
            }

            // Update position if changed
            let needs_move = state
                .space
                .output_geometry(existing)
                .map(|g| g.loc.x != *left || g.loc.y != *top)
                .unwrap_or(true);
            if needs_move {
                info!(output = %name, left, top, "DisplayControl: repositioning output");
                state.space.map_output(existing, (*left, *top));
            }
        } else {
            // Create new output
            info!(output = %name, w, h, left, top, "DisplayControl: creating output");
            let new_output = Output::new(
                name.clone(),
                PhysicalProperties {
                    size: (0, 0).into(),
                    subpixel: Subpixel::Unknown,
                    make: "desktopinator".into(),
                    model: "rdp".into(),
                },
            );
            new_output.change_current_state(Some(mode), None, None, None);
            new_output.set_preferred(mode);
            new_output.create_global::<DinatorState>(&state.display_handle);
            state.space.map_output(&new_output, (*left, *top));
            state.register_output(&new_output);

            // Create renderbuffer
            match renderer.create_buffer(Fourcc::Abgr8888, Size::from((*w as i32, *h as i32))) {
                Ok(rb) => {
                    let dt = OutputDamageTracker::from_output(&new_output);
                    output_render_states.insert(name.clone(), (rb, dt));
                }
                Err(e) => tracing::error!(output = %name, "failed to create renderbuffer: {e:?}"),
            }

            // Create encoder
            if let Some(enc) = create_encoder(*w as u32, *h as u32, encoder_pref) {
                h264_encoders.insert(name.clone(), enc);
            }

            state.emit_event(dinator_ipc::IpcEvent::OutputCreated {
                name: name.clone(),
                width: *w,
                height: *h,
            });
        }
    }
}

fn create_encoder(width: u32, height: u32, pref: &str) -> Option<Box<dyn dinator_encode::Encoder>> {
    if pref == "openh264" {
        match dinator_encode::OpenH264Encoder::new(width, height, 2_000_000) {
            Ok(enc) => return Some(Box::new(enc)),
            Err(e) => tracing::warn!(error = %e, "openh264 encoder failed"),
        }
    } else {
        let ffmpeg_pref = match pref {
            "vaapi" => dinator_encode::FfmpegEncoderPreference::Vaapi,
            "nvenc" => dinator_encode::FfmpegEncoderPreference::Nvenc,
            "x264" => dinator_encode::FfmpegEncoderPreference::Software,
            _ => dinator_encode::FfmpegEncoderPreference::Auto,
        };
        match dinator_encode::FfmpegEncoder::new(width, height, 2_000_000, ffmpeg_pref) {
            Ok(enc) => return Some(Box::new(enc)),
            Err(e) => {
                tracing::info!(error = %e, "FFmpeg encoder unavailable, trying openh264");
                match dinator_encode::OpenH264Encoder::new(width, height, 2_000_000) {
                    Ok(enc) => return Some(Box::new(enc)),
                    Err(e2) => tracing::warn!(error = %e2, "no H.264 encoder available"),
                }
            }
        }
    }
    None
}

/// Map number keysyms (both shifted and unshifted) to workspace numbers 1-9.
fn keysym_to_workspace(sym: u32) -> Option<usize> {
    use smithay::input::keyboard::keysyms;
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

/// Parse a key name string to an XKB keysym.
fn key_name_to_keysym(name: &str) -> Option<u32> {
    use smithay::input::keyboard::{keysyms, xkb};
    let sym = xkb::keysym_from_name(name, xkb::KEYSYM_NO_FLAGS);
    if sym.raw() == keysyms::KEY_NoSymbol {
        // Try with XKB_KEYSYM_CASE_INSENSITIVE
        let sym = xkb::keysym_from_name(name, xkb::KEYSYM_CASE_INSENSITIVE);
        if sym.raw() == keysyms::KEY_NoSymbol {
            None
        } else {
            Some(sym.raw())
        }
    } else {
        Some(sym.raw())
    }
}

/// Parse modifier strings like "Alt", "Ctrl", "Shift", "Super"
/// into a (alt, ctrl, shift, logo) tuple.
fn parse_modifiers(mods: &[String]) -> (bool, bool, bool, bool) {
    let mut alt = false;
    let mut ctrl = false;
    let mut shift = false;
    let mut logo = false;
    for m in mods {
        match m.to_lowercase().as_str() {
            "alt" | "mod1" => alt = true,
            "ctrl" | "control" => ctrl = true,
            "shift" => shift = true,
            "super" | "logo" | "mod4" => logo = true,
            _ => {}
        }
    }
    (alt, ctrl, shift, logo)
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

/// Initialize the plugin runtimes (Lua + WASM) and load plugins from the config directory.
/// Returns any plugin-registered keybindings.
pub(crate) fn init_plugins(state: &mut DinatorState) -> Vec<PluginKeybinding> {
    let plugin_dir = std::env::var("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string()))
                .join(".config")
        })
        .join("desktopinator/plugins");

    let mut lua_runtime = dinator_lua::LuaRuntime::new();
    if let Err(e) = lua_runtime.load_plugins(&plugin_dir) {
        tracing::warn!(error = %e, "failed to load Lua plugins");
    }

    let mut wasm_runtime = dinator_wasm::WasmRuntime::new();
    if let Err(e) = wasm_runtime.load_plugins(&plugin_dir) {
        tracing::warn!(error = %e, "failed to load WASM plugins");
    }

    let mut runtime = dinator_plugin_api::CompositeRuntime::new(vec![
        Box::new(lua_runtime),
        Box::new(wasm_runtime),
    ]);

    let plugin_count = runtime.plugin_info().len();
    let layout_count = runtime.layout_names().len();
    if plugin_count > 0 {
        info!(
            plugins = plugin_count,
            layouts = layout_count,
            "plugin system initialized"
        );
    }

    // Drain keybinding requests from plugins
    let keybindings: Vec<PluginKeybinding> = runtime
        .drain_keybinding_requests()
        .into_iter()
        .filter_map(|req| {
            let keysym = key_name_to_keysym(&req.key)?;
            let mods = parse_modifiers(&req.modifiers);
            info!(
                callback = %req.callback_id,
                key = %req.key,
                "registered plugin keybinding"
            );
            Some(PluginKeybinding {
                mods,
                keysym,
                callback_id: req.callback_id,
            })
        })
        .collect();

    // Drain window rules from plugins
    let window_rules = runtime.drain_window_rules();
    if !window_rules.is_empty() {
        info!(count = window_rules.len(), "registered plugin window rules");
    }
    state.window_rules = window_rules;

    state.plugin_runtime = Some(Box::new(runtime));
    keybindings
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
            if state.grow_master() {
                let output = state.get_focused_output();
                if let Some(output) = output {
                    state.retile(&output);
                }
                let ratio = state.master_ratio().unwrap_or(0.0);
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
            if state.shrink_master() {
                let output = state.get_focused_output();
                if let Some(output) = output {
                    state.retile(&output);
                }
                let ratio = state.master_ratio().unwrap_or(0.0);
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
            let focus_ws = state.focused_workspace();
            let ws_windows = state.ws_window_list(focus_ws).to_vec();
            let windows: Vec<serde_json::Value> = ws_windows
                .iter()
                .enumerate()
                .map(|(i, id)| {
                    let is_floating = state.floating.contains(id);
                    let is_fullscreen = state.fullscreen.contains(id);
                    let ws = state.window_workspace.get(id).copied().unwrap_or(focus_ws);
                    let mut entry = serde_json::json!({
                        "index": i,
                        "id": id.0,
                        "floating": is_floating,
                        "fullscreen": is_fullscreen,
                        "workspace": ws,
                    });
                    if let Some(window) = state.window_map.get(id) {
                        if let Some(geo) = state.space.element_geometry(window) {
                            entry["x"] = geo.loc.x.into();
                            entry["y"] = geo.loc.y.into();
                            entry["width"] = geo.size.w.into();
                            entry["height"] = geo.size.h.into();
                        }
                        if let Some(toplevel) = window.toplevel() {
                            let (app_id, title) =
                                compositor::with_states(toplevel.wl_surface(), |states| {
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
                let output = state.get_focused_output();
                if let Some(output) = output {
                    state.retile(&output);
                }
                state.emit_event(dinator_ipc::IpcEvent::LayoutChanged { name: name.clone() });
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
            let current = state.layout_name().to_string();
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
            let current_layout = state.layout_name().to_string();
            if let Some(ref mut runtime) = state.plugin_runtime {
                match runtime.reload() {
                    Ok(()) => {
                        let count = runtime.plugin_info().len();
                        info!(count, "plugins reloaded");
                        // Re-create active layout from new runtime, or fall back
                        if current_layout != "column" && current_layout != "monocle" {
                            if let Some(new_layout) = runtime.create_layout(&current_layout) {
                                state.set_focused_layout(new_layout);
                                info!("re-created plugin layout '{current_layout}' from reloaded plugin");
                            } else {
                                state.set_focused_layout(Box::new(
                                    dinator_tiling::ColumnLayout::default(),
                                ));
                                info!("active plugin layout '{current_layout}' no longer available, fell back to column");
                            }
                            let output = state.get_focused_output();
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
                info!(
                    id = id.0,
                    fullscreen = is_fullscreen,
                    "IPC: toggle-fullscreen"
                );
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
        IpcCommand::SwitchWorkspace { workspace } => {
            if *workspace < 1 || *workspace > 9 {
                IpcResponse::Error {
                    message: format!("invalid workspace: {workspace} (must be 1-9)"),
                }
            } else {
                state.switch_workspace(*workspace);
                IpcResponse::Ok {
                    message: Some(format!("workspace {workspace}")),
                }
            }
        }
        IpcCommand::MoveToWorkspace { workspace } => {
            if *workspace < 1 || *workspace > 9 {
                IpcResponse::Error {
                    message: format!("invalid workspace: {workspace} (must be 1-9)"),
                }
            } else {
                state.move_to_workspace(*workspace);
                IpcResponse::Ok {
                    message: Some(format!("moved to workspace {workspace}")),
                }
            }
        }
        IpcCommand::ListWorkspaces => {
            let active_ws = state.focused_workspace();
            let data: Vec<serde_json::Value> = (1..=9)
                .map(|ws| {
                    let count = state.workspace_order.get(&ws).map(|v| v.len()).unwrap_or(0);
                    serde_json::json!({
                        "workspace": ws,
                        "windows": count,
                        "active": ws == active_ws,
                    })
                })
                .collect();
            IpcResponse::Data {
                data: serde_json::Value::Array(data),
            }
        }
        IpcCommand::SetGap { pixels } => {
            info!(pixels, "IPC: set-gap");
            if state.set_layout_gap(*pixels) {
                let output = state.get_focused_output();
                if let Some(output) = output {
                    state.retile(&output);
                }
            }
            IpcResponse::Ok {
                message: Some(format!("gap: {pixels}px")),
            }
        }
        IpcCommand::SetBackground { spec } => {
            info!(spec = %spec, "IPC: set-background");
            match dinator_core::parse_background(spec) {
                Some(bg) => {
                    let desc = format!("{bg:?}");
                    state.set_background(bg);
                    IpcResponse::Ok {
                        message: Some(format!("background: {desc}")),
                    }
                }
                None => IpcResponse::Error {
                    message: format!("invalid background spec: {spec} (use #RRGGBB, r,g,b, or two colors separated by -)"),
                },
            }
        }
        IpcCommand::CreateOutput {
            name,
            width,
            height,
        } => {
            info!(name = %name, width, height, "IPC: create-output");
            // Check if output already exists
            if state.output_states.contains_key(name.as_str()) {
                return IpcResponse::Error {
                    message: format!("output '{name}' already exists"),
                };
            }

            let mode = Mode {
                size: (*width as i32, *height as i32).into(),
                refresh: 60_000,
            };
            let new_output = Output::new(
                name.clone(),
                PhysicalProperties {
                    size: (0, 0).into(),
                    subpixel: Subpixel::Unknown,
                    make: "desktopinator".into(),
                    model: "headless".into(),
                },
            );
            new_output.change_current_state(
                Some(mode),
                Some(Transform::Normal),
                None,
                None, // position will be set below
            );
            new_output.set_preferred(mode);

            // Position to the right of the last output
            let x_offset: i32 = state
                .space
                .outputs()
                .filter_map(|o| state.space.output_geometry(o))
                .map(|geo| geo.loc.x + geo.size.w)
                .max()
                .unwrap_or(0);

            new_output.create_global::<DinatorState>(&state.display_handle);
            state.space.map_output(&new_output, (x_offset, 0));
            state.register_output(&new_output);

            state.emit_event(dinator_ipc::IpcEvent::OutputCreated {
                name: name.clone(),
                width: *width,
                height: *height,
            });

            IpcResponse::Ok {
                message: Some(format!("created output '{name}' ({width}x{height})")),
            }
        }
        IpcCommand::RemoveOutput { name } => {
            info!(name = %name, "IPC: remove-output");

            if !state.output_states.contains_key(name.as_str()) {
                return IpcResponse::Error {
                    message: format!("output '{name}' not found"),
                };
            }

            // Don't allow removing the last output
            if state.output_states.len() <= 1 {
                return IpcResponse::Error {
                    message: "cannot remove the last output".to_string(),
                };
            }

            let output = state.space.outputs().find(|o| o.name() == *name).cloned();

            if let Some(output) = output {
                state.unregister_output(&output);
                state.emit_event(dinator_ipc::IpcEvent::OutputRemoved { name: name.clone() });
                IpcResponse::Ok {
                    message: Some(format!("removed output '{name}'")),
                }
            } else {
                IpcResponse::Error {
                    message: format!("output '{name}' not found in space"),
                }
            }
        }
        IpcCommand::ListOutputs => {
            let focused = state.focused_output.clone().unwrap_or_default();
            let data: Vec<serde_json::Value> = state
                .space
                .outputs()
                .map(|o| {
                    let name = o.name();
                    let geo = state.space.output_geometry(o);
                    let os = state.output_states.get(&name);
                    serde_json::json!({
                        "name": name,
                        "width": geo.map(|g| g.size.w).unwrap_or(0),
                        "height": geo.map(|g| g.size.h).unwrap_or(0),
                        "x": geo.map(|g| g.loc.x).unwrap_or(0),
                        "y": geo.map(|g| g.loc.y).unwrap_or(0),
                        "workspace": os.map(|s| s.active_workspace).unwrap_or(1),
                        "layout": os.map(|s| s.layout.name()).unwrap_or("column"),
                        "focused": name == focused,
                    })
                })
                .collect();
            IpcResponse::Data {
                data: serde_json::Value::Array(data),
            }
        }
        IpcCommand::FocusOutput { name } => {
            info!(name = %name, "IPC: focus-output");
            if !state.output_states.contains_key(name.as_str()) {
                return IpcResponse::Error {
                    message: format!("output '{name}' not found"),
                };
            }
            state.focus_output(name);
            state.emit_event(dinator_ipc::IpcEvent::OutputFocused { name: name.clone() });
            IpcResponse::Ok {
                message: Some(format!("focused output '{name}'")),
            }
        }
        IpcCommand::MoveWindowToOutput { name } => {
            info!(name = %name, "IPC: move-window-to-output");
            if state.move_window_to_output(name) {
                IpcResponse::Ok {
                    message: Some(format!("moved window to output '{name}'")),
                }
            } else {
                IpcResponse::Error {
                    message: format!("failed to move window to output '{name}' (no focused window or output not found)"),
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
        IpcCommand::Status => {
            let window_count = state.space.elements().count();
            let output_count = state.space.outputs().count();
            let focused_output = state.focused_output.clone().unwrap_or_default();
            let focused_ws = state.focused_workspace();
            IpcResponse::Data {
                data: serde_json::json!({
                    "rdp_clients": state.rdp_clients,
                    "vnc_clients": state.vnc_clients,
                    "windows": window_count,
                    "outputs": output_count,
                    "focused_output": focused_output,
                    "focused_workspace": focused_ws,
                }),
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

// --- RDP Server Integration ---

/// RDP display handler — provides desktop size and display update stream to ironrdp-server.
struct DinatorRdpDisplay {
    width: Arc<std::sync::atomic::AtomicU16>,
    height: Arc<std::sync::atomic::AtomicU16>,
    update_tx:
        Arc<tokio::sync::Mutex<Option<tokio::sync::mpsc::Sender<ironrdp_server::DisplayUpdate>>>>,
    input_tx: calloop::channel::Sender<RdpInputEvent>,
}

struct DinatorRdpDisplayUpdates {
    rx: tokio::sync::mpsc::Receiver<ironrdp_server::DisplayUpdate>,
    /// Sends ClientDisconnected when this struct is dropped (connection ended).
    disconnect_tx: Option<calloop::channel::Sender<RdpInputEvent>>,
}

impl Drop for DinatorRdpDisplayUpdates {
    fn drop(&mut self) {
        if let Some(ref tx) = self.disconnect_tx {
            let _ = tx.send(RdpInputEvent::ClientDisconnected);
        }
    }
}

#[async_trait::async_trait]
impl ironrdp_server::RdpServerDisplayUpdates for DinatorRdpDisplayUpdates {
    async fn next_update(&mut self) -> anyhow::Result<Option<ironrdp_server::DisplayUpdate>> {
        Ok(self.rx.recv().await)
    }
}

#[async_trait::async_trait]
impl ironrdp_server::RdpServerDisplay for DinatorRdpDisplay {
    async fn size(&mut self) -> ironrdp_server::DesktopSize {
        ironrdp_server::DesktopSize {
            width: self.width.load(std::sync::atomic::Ordering::Relaxed),
            height: self.height.load(std::sync::atomic::Ordering::Relaxed),
        }
    }

    async fn updates(
        &mut self,
    ) -> anyhow::Result<Box<dyn ironrdp_server::RdpServerDisplayUpdates>> {
        let (tx, rx) = tokio::sync::mpsc::channel(2);
        *self.update_tx.lock().await = Some(tx);
        let _ = self.input_tx.send(RdpInputEvent::ClientConnected);
        Ok(Box::new(DinatorRdpDisplayUpdates {
            rx,
            disconnect_tx: Some(self.input_tx.clone()),
        }))
    }

    // request_layout is handled by our custom DisplayControlDvc handler
    // (registered via set_dvc_builder), not through the display trait.
}

/// RDP input handler — receives keyboard/mouse events from RDP clients
/// and forwards them to the compositor event loop via calloop channel.
struct DinatorRdpInputHandler {
    tx: calloop::channel::Sender<RdpInputEvent>,
}

impl ironrdp_server::RdpServerInputHandler for DinatorRdpInputHandler {
    fn keyboard(&mut self, event: ironrdp_server::KeyboardEvent) {
        tracing::debug!(?event, "RDP input handler: keyboard event received");
        match event {
            ironrdp_server::KeyboardEvent::Pressed { code, extended } => {
                if let Some(keycode) = rdp_scancode_to_xkb(code, extended) {
                    let _ = self.tx.send(RdpInputEvent::Key {
                        keycode,
                        pressed: true,
                    });
                } else {
                    tracing::warn!(code, extended, "RDP: unknown scancode, no XKB mapping");
                }
            }
            ironrdp_server::KeyboardEvent::Released { code, extended } => {
                if let Some(keycode) = rdp_scancode_to_xkb(code, extended) {
                    let _ = self.tx.send(RdpInputEvent::Key {
                        keycode,
                        pressed: false,
                    });
                }
            }
            _ => {
                tracing::debug!(
                    ?event,
                    "RDP: unhandled keyboard event type (Unicode/Synchronize)"
                );
            }
        }
    }

    fn mouse(&mut self, event: ironrdp_server::MouseEvent) {
        tracing::debug!(?event, "RDP input handler: mouse event received");
        match event {
            ironrdp_server::MouseEvent::Move { x, y } => {
                let _ = self.tx.send(RdpInputEvent::MouseMove { x, y });
            }
            ironrdp_server::MouseEvent::LeftPressed => {
                let _ = self.tx.send(RdpInputEvent::MouseButton {
                    button: 0x110, // BTN_LEFT
                    pressed: true,
                });
            }
            ironrdp_server::MouseEvent::LeftReleased => {
                let _ = self.tx.send(RdpInputEvent::MouseButton {
                    button: 0x110,
                    pressed: false,
                });
            }
            ironrdp_server::MouseEvent::RightPressed => {
                let _ = self.tx.send(RdpInputEvent::MouseButton {
                    button: 0x111, // BTN_RIGHT
                    pressed: true,
                });
            }
            ironrdp_server::MouseEvent::RightReleased => {
                let _ = self.tx.send(RdpInputEvent::MouseButton {
                    button: 0x111,
                    pressed: false,
                });
            }
            ironrdp_server::MouseEvent::MiddlePressed => {
                let _ = self.tx.send(RdpInputEvent::MouseButton {
                    button: 0x112, // BTN_MIDDLE
                    pressed: true,
                });
            }
            ironrdp_server::MouseEvent::MiddleReleased => {
                let _ = self.tx.send(RdpInputEvent::MouseButton {
                    button: 0x112,
                    pressed: false,
                });
            }
            ironrdp_server::MouseEvent::VerticalScroll { value } => {
                let _ = self.tx.send(RdpInputEvent::MouseScroll { value });
            }
            _ => {} // Button4/5 and RelMove not handled yet
        }
    }
}

/// Generate a self-signed TLS certificate and create a TlsAcceptor for the RDP server.
///
/// Microsoft Remote Desktop (mstsc) requires TLS — the no-security mode won't work.
/// We generate a fresh self-signed cert at startup. No credentials are validated.
fn make_rdp_tls_acceptor() -> anyhow::Result<ironrdp_server::tokio_rustls::TlsAcceptor> {
    use ironrdp_server::tokio_rustls::rustls;
    use std::sync::Arc;

    // Ensure a crypto provider is installed (rustls requires this)
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
        .context("failed to generate self-signed certificate")?;
    let cert_der = rustls::pki_types::CertificateDer::from(cert.cert.der().to_vec());
    let key_der = rustls::pki_types::PrivateKeyDer::try_from(cert.signing_key.serialize_der())
        .map_err(|e| anyhow::anyhow!("failed to parse private key: {e}"))?;

    let mut server_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .context("bad certificate/key")?;
    server_config.key_log = Arc::new(rustls::KeyLogFile::new());

    Ok(ironrdp_server::tokio_rustls::TlsAcceptor::from(Arc::new(
        server_config,
    )))
}

/// Convert an RDP scan code (Windows/XT Set 1) to an XKB keycode (evdev + 8).
///
/// RDP keyboard events provide a scan code byte and an `extended` flag.
/// Non-extended scan codes map 1:1 to evdev keycodes.
/// Extended scan codes (0xE0 prefix keys) need a lookup table.
fn rdp_scancode_to_xkb(code: u8, extended: bool) -> Option<u32> {
    let evdev = if extended {
        match code {
            0x1C => 96,  // KP Enter
            0x1D => 97,  // Right Ctrl
            0x35 => 98,  // KP /
            0x37 => 99,  // Print Screen / SysRq
            0x38 => 100, // Right Alt
            0x46 => 119, // Pause
            0x47 => 102, // Home
            0x48 => 103, // Up
            0x49 => 104, // Page Up
            0x4B => 105, // Left
            0x4D => 106, // Right
            0x4F => 107, // End
            0x50 => 108, // Down
            0x51 => 109, // Page Down
            0x52 => 110, // Insert
            0x53 => 111, // Delete
            0x5B => 125, // Left Super / Win
            0x5C => 126, // Right Super / Win
            0x5D => 127, // Menu / Compose
            _ => return None,
        }
    } else {
        code as u32
    };
    // XKB keycodes are evdev keycodes + 8
    Some(evdev + 8)
}
