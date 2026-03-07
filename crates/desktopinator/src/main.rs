use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::solid::{SolidColorBuffer, SolidColorRenderElement};
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::element::RenderElement;
use smithay::backend::renderer::gles::{GlesFrame, GlesRenderer};
use smithay::backend::winit::{self, WinitEvent};
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

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    info!("starting desktopinator");

    // Initialize winit backend BEFORE creating our wayland socket.
    // Winit connects to the host compositor as a client -- if we set
    // WAYLAND_DISPLAY to our own socket first, winit would deadlock
    // trying to connect to ourselves.
    let (mut backend, winit_evt_loop) = winit::init::<GlesRenderer>()
        .map_err(|e| anyhow::anyhow!("winit init failed: {e:?}"))?;

    info!("winit backend initialized");

    let mut event_loop: EventLoop<DinatorState> =
        EventLoop::try_new().context("failed to create event loop")?;

    let display = Display::new().context("failed to create wayland display")?;

    // Create the listening socket for our clients
    let listening_socket =
        ListeningSocket::bind_auto("wayland", 0..33).context("failed to bind wayland socket")?;
    let socket_name = listening_socket
        .socket_name()
        .context("no socket name")?
        .to_string_lossy()
        .into_owned();
    info!(socket = %socket_name, "wayland socket listening");

    // Set WAYLAND_DISPLAY so child processes connect to us, not the host compositor
    std::env::set_var("WAYLAND_DISPLAY", &socket_name);

    // Create an output matching the winit window
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

    // Register the output as a Wayland global so clients can see it
    output.create_global::<DinatorState>(&display_handle);

    // Add output to the space
    state.space.map_output(&output, (0, 0));

    // Initialize keyboard (US layout as default) and pointer
    state.seat.add_keyboard(Default::default(), 200, 25)?;
    state.seat.add_pointer();

    // Insert the listening socket into the event loop to accept new clients
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

    // Insert the winit event source
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

                // Scope the renderer borrow so backend.submit() can borrow mutably after
                {
                    let Ok((renderer, mut framebuffer)) = backend.bind() else {
                        return;
                    };

                    let space_elements: Vec<
                        SpaceRenderElements<
                            GlesRenderer,
                            WaylandSurfaceRenderElement<GlesRenderer>,
                        >,
                    > = match state.space.render_elements_for_output(renderer, &output, 1.0) {
                        Ok(elements) => elements,
                        Err(_) => return,
                    };

                    // Build combined element list: space elements on top, focus border behind
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
                                SolidColorRenderElement::from_buffer(
                                    &buf,
                                    loc,
                                    1.0,
                                    1.0,
                                    Kind::Unspecified,
                                ),
                            ));
                        }
                    }

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

                // Request another frame so we continuously redraw
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
            // SAFETY: dispatch_clients borrows Display mutably and passes state
            // to the Wayland protocol handlers. We know state won't be dropped
            // and the display won't be accessed reentrantly during dispatch.
            let display_ptr = &mut state.display as *mut Display<DinatorState>;
            unsafe { &mut *display_ptr }.dispatch_clients(state).unwrap();
            state.display.flush_clients().unwrap();
        })
        .context("event loop error")?;

    info!("shutting down");
    Ok(())
}
