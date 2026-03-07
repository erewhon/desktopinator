# Desktopinator Architecture

## Core Insight

A compositor's "output" is just a rectangle that gets rendered to. Whether that
rectangle is a physical monitor, a VNC framebuffer, an RDP session, or a game
stream shouldn't matter to the rest of the compositor. Desktopinator treats all
outputs uniformly -- local and remote are first-class citizens.

## Layer Diagram

```
+---------------------------------------------------------------+
|                        Configuration                          |
|                         (KDL / CLI)                            |
+---------------------------------------------------------------+
|                        Plugin Runtime                         |
|                     (WASM via wasmtime)                       |
+---------------------------------------------------------------+
|                                                               |
|                     Compositor Core (Smithay)                 |
|                                                               |
|  +------------------+  +---------------+  +-----------------+ |
|  |  Window Manager  |  | Input Router  |  |  Output Manager | |
|  |  / Tiling Engine |  | (local+remote)|  |  (unified)      | |
|  +------------------+  +---------------+  +-----------------+ |
|                                                               |
+---------------------------------------------------------------+
|                      Renderer                                 |
|          GPU (EGL/GLES2, wgpu) | Software fallback           |
+---------------------------------------------------------------+
|                      Encoder                                  |
|     Raw/Tight (VNC) | H.264/H.265 (RDP, Stream)              |
|     VAAPI / NVENC when available | Software fallback          |
+---------------------------------------------------------------+
|                   Protocol Servers                            |
|  +-------+  +-------+  +----------+  +-----------+           |
|  |  VNC  |  |  RDP  |  | Sunshine |  |  (future) |           |
|  +-------+  +-------+  +----------+  +-----------+           |
+---------------------------------------------------------------+
|                   Network Layer                               |
|          TCP/TLS | Tailscale | mDNS discovery                 |
+---------------------------------------------------------------+
```

## Key Abstraction: Unified Outputs

The central design decision. An `Output` represents any rendering target:

```
Output
  +-- PhysicalOutput    (DRM/KMS -- local monitor)
  +-- VirtualOutput     (headless framebuffer -- remote sessions)
  +-- NestedOutput      (winit window -- development/testing)
```

Every output has: resolution, refresh rate, scale factor, position in the
global layout, and a damage tracker. The tiling engine, input routing, and
window management don't know or care which kind they're talking to.

### Output Lifecycle for Remote Connections

```
Client connects
  -> Protocol server negotiates resolution + capabilities
  -> Output Manager creates a VirtualOutput
  -> Tiling engine incorporates it into the layout
  -> Renderer starts producing frames for it
  -> Encoder encodes frames per protocol requirements
  -> Protocol server streams to client, routes input back

Client disconnects
  -> Windows migrate to remaining outputs (configurable policy)
  -> VirtualOutput is destroyed
```

### Usage Modes (all the same code path)

| Mode | Physical Outputs | Virtual Outputs |
|------|-----------------|-----------------|
| Normal desktop | 1+ monitors | 0 |
| Desktop + remote viewer | 1+ monitors | 1+ remote sessions |
| Headless remote server | 0 | 1+ remote sessions |
| Screen sharing | 1+ monitors (shared) | 0 (direct capture) |

## Component Details

### Compositor Core (Smithay)

Smithay provides the building blocks. We use:

- **Wayland protocol handling** -- client connection, surface management,
  xdg-shell, layer-shell, etc.
- **Backend abstraction** -- DRM for physical outputs, headless for virtual
- **Seat management** -- input devices, pointer/keyboard focus
- **Damage tracking** -- only re-render what changed
- **calloop** -- Smithay's event loop (we integrate network I/O into it)

Smithay does NOT give us a compositor -- it gives us the toolkit to build one.
We are building the compositor, the output orchestration, and the remote
protocol integration on top of it.

### Tiling Engine

Per-output layout management:

- Each output has an independent workspace with its own layout
- Layouts: column, row, spiral, monocle, floating (at minimum)
- Windows can be moved between outputs (including between physical and virtual)
- Layout is a trait -- plugins can provide custom layouts

### Input Router

Multiplexes input from multiple sources:

- **Local**: libinput (keyboard, mouse, touch, tablet)
- **Remote**: each protocol server translates client input into compositor
  input events
- Every `InputEvent` carries a `SeatId` (future-proofing for multi-seat)

### Session Model

Three modes, configurable per-connection:

**Session takeover** (default, like Windows RDP): remote client connects, local
screen locks, virtual outputs replace physical ones at the client's requested
resolution. Windows rearrange to the new output layout. On disconnect, local
screen unlocks and windows migrate back to physical outputs. Only one user
drives the session at a time -- no multi-seat complexity.

```
Remote connects (2x 1080p)
  -> Lock local display (show lock screen / blank)
  -> Create VirtualOutputs at client's resolution
  -> Migrate windows to virtual outputs, re-tile
  -> Remote user works normally

Remote disconnects
  -> Destroy virtual outputs
  -> Unlock local display
  -> Migrate windows back to physical outputs, re-tile
```

**Screen sharing**: remote client sees and optionally controls the existing
physical outputs. No virtual outputs created. Shared seat -- both local and
remote input go to the same pointer/keyboard.

**Independent session** (future, requires multi-seat): remote client gets its
own virtual outputs and its own seat with independent cursor/keyboard. Local
user continues working on physical outputs simultaneously. This is the most
complex mode and will be implemented later.

Session takeover covers the most common remote desktop use case and sidesteps
the multi-seat problem entirely. The `SeatId` on input events is there so that
adding independent sessions later doesn't require rearchitecting input routing.

### Renderer

Produces frames for each output. Must support:

- **GPU path**: EGL/GLES2 (Smithay's default), or wgpu for Vulkan. Used when a
  GPU is available. Physical outputs always use this.
- **Software path**: pixman or softbuffer. For headless servers with no GPU.
- **Capture**: after rendering a frame, hand the buffer to the encoder.
  For physical outputs being shared, this uses screencopy/dmabuf export.
  For virtual outputs, the renderer writes directly to an encoder-accessible
  buffer.

### Encoder

Bridges rendered frames to protocol-specific formats:

| Protocol | Encoding | Notes |
|----------|----------|-------|
| VNC | Raw, Tight, ZRLE | Low latency for LAN, high compat |
| RDP | H.264 (AVC) / H.265 (HEVC) via RemoteFX/GFX | Modern RDP uses video codec |
| Sunshine | H.264 / H.265 / AV1 | Moonlight client compat |

Hardware acceleration:
- **VAAPI** (Intel/AMD) -- preferred on Linux
- **NVENC** (NVIDIA) -- when proprietary driver available
- **Software fallback** -- openh264 or x264 via ffmpeg

The encoder is output-specific: a VNC session and an RDP session on different
virtual outputs use different encoding pipelines simultaneously.

### Protocol Servers

Each protocol is a separate crate implementing a common trait:

```rust
trait RemoteProtocol {
    /// Accept and negotiate a new connection.
    /// Returns the desired output configuration.
    fn accept(&mut self, stream: TlsStream) -> OutputRequest;

    /// Deliver an encoded frame to the client.
    fn send_frame(&mut self, frame: EncodedFrame) -> Result<()>;

    /// Poll for input events from the client.
    fn recv_input(&mut self) -> Vec<InputEvent>;

    /// Send clipboard data to the client.
    fn clipboard_send(&mut self, data: ClipboardData) -> Result<()>;

    /// Receive clipboard data from the client.
    fn clipboard_recv(&mut self) -> Option<ClipboardData>;

    /// Send an audio frame to the client.
    fn audio_send(&mut self, frame: AudioFrame) -> Result<()>;

    /// Handle client disconnect / session teardown.
    fn disconnect(&mut self);
}
```

**VNC**: There are some Rust VNC crates, but quality varies. May need to wrap a
C library or build from the RFB spec (it's relatively simple).

**RDP**: [IronRDP](https://github.com/Devolutions/IronRDP) by Devolutions is a
pure-Rust RDP implementation. It handles the protocol; we provide the frames
and consume the input.

**Sunshine**: The Sunshine protocol (based on NVIDIA GameStream / Moonlight) is
more complex. Options are: wrap Sunshine's C++ code, implement the protocol in
Rust, or contribute to Sunshine to make it usable as a library. This is likely
the hardest integration.

### Network Layer

- **Direct TCP/TLS**: standard socket listeners for each protocol
- **Tailscale**: detect if `tailscale` is available, optionally bind to
  tailscale addresses, use tailscale's MagicDNS for discovery. Could also use
  tsnet (Tailscale's embeddable library) via FFI if deeper integration is
  wanted.
- **mDNS/DNS-SD**: advertise available services on LAN (Avahi/zeroconf)
- **Authentication**: per-protocol auth (VNC password, RDP NLA/CredSSP) plus
  optional compositor-level auth (e.g., require Tailscale identity)

### Plugin System

WASM plugins via wasmtime, sandboxed by default:

**Plugin capabilities** (granted explicitly):
- `layout` -- provide custom tiling layouts
- `keybindings` -- register key bindings and actions
- `status` -- provide status bar / overlay content
- `decoration` -- custom window decorations
- `protocol` -- (advanced) add new remote protocols

**Plugin API**: a stable ABI exposed via WASM imports. Plugins cannot access
the filesystem, network, or compositor internals unless explicitly granted.

**Configuration**: plugins are declared in the config file with their
capability grants.

### Configuration

**Single static binary with sensible defaults.** No config file required. The
compositor works out of the box with reasonable keybindings, default tiling
layout, and standard protocol ports.

For customization, in order of complexity:
1. **CLI flags** -- override defaults (e.g., `--vnc-port 5901`, `--layout spiral`)
2. **Optional KDL config file** -- for complex setups (custom keybindings,
   output policies, plugin declarations)
3. **Runtime IPC** -- change settings on the fly (like `swaymsg`)

Config file locations (checked in order): `$XDG_CONFIG_HOME/desktopinator/config.kdl`,
`~/.config/desktopinator/config.kdl`. If no config exists, defaults are used.

What the config covers (when used):
- Output policies (what happens when remote connects/disconnects)
- Tiling layout defaults
- Keybindings
- Protocol server settings (ports, auth, encoding preferences)
- Network settings (tailscale, mDNS, TLS certs)
- Plugin declarations and capability grants

## Crate Structure

```
desktopinator/
  Cargo.toml                    (workspace)
  crates/
    desktopinator/              Main binary -- wires everything together
    dinator-core/               Compositor core, output manager, seat management
    dinator-tiling/             Layout engine, workspace management
    dinator-render/             Renderer abstraction (GPU + software)
    dinator-encode/             Frame encoding (hw + sw)
    dinator-vnc/                VNC (RFB) protocol server
    dinator-rdp/                RDP protocol server (wraps IronRDP)
    dinator-stream/             Sunshine/Moonlight protocol server
    dinator-net/                Network, TLS, Tailscale, mDNS
    dinator-plugins/            WASM plugin runtime
    dinator-config/             Configuration parsing and validation
    dinator-proto/              Shared types: OutputRequest, InputEvent, etc.
```

Short crate names (`dinator-*`) to keep `use` statements readable.

## Key Dependencies

| Crate | Purpose |
|-------|---------|
| `smithay` | Compositor framework |
| `ironrdp` | RDP protocol |
| `wasmtime` | WASM plugin runtime |
| `calloop` | Event loop (Smithay's choice, we extend it) |
| `gstreamer-rs` | Encoding pipeline (VAAPI/NVENC/software) |
| `rustls` | TLS for protocol servers |
| `tokio` | Async I/O for network (bridged into calloop) |
| `mdns-sd` | mDNS/DNS-SD for discovery |

## What to Build First

Suggested incremental path:

**Phase 1 -- Local compositor**
1. Minimal compositor: Smithay + DRM backend + single output + basic tiling.
   Get windows rendering on a physical display.
2. Add headless backend: create virtual outputs programmatically. Render to
   them. Prove the output abstraction works.

**Phase 2 -- First remote protocol**
3. VNC server: attach a VNC server to a virtual output. Remote client sees
   the desktop. Input flows back. First "it works remotely" milestone.
4. Session takeover: remote connect locks local screen, creates virtual
   outputs at client resolution, windows migrate. Disconnect reverses it.

**Phase 3 -- Fast-follows**
5. XWayland: X11 app compatibility. Needed before daily driving.
6. Clipboard forwarding: wl_data_device <-> VNC extended clipboard.
7. Audio forwarding: PipeWire capture -> Opus -> VNC audio extension.

**Phase 4 -- RDP**
8. RDP server: IronRDP integration. Video encoding via gstreamer-rs
   (software first).
9. Hardware encoding: VAAPI/NVENC via gstreamer-rs for RDP.
10. RDP clipboard (CLIPRDR) and audio (RDPSND) channels.

**Phase 5 -- Network and polish**
11. Tailscale integration + mDNS discovery.
12. KDL configuration file.
13. Multi-monitor support (multiple physical + multiple virtual).

**Phase 6 -- Extensibility**
14. Plugin system: WASM runtime, layout plugins, keybinding plugins.

**Future**
- Sunshine/Moonlight streaming protocol.
- Independent multi-seat sessions.
- Dynamic quality adaptation for slow connections.

## Decisions

- **Encoding**: gstreamer-rs. Pipeline model maps naturally to per-output
  encoding. Handles hardware detection and fallback (VAAPI -> NVENC ->
  software) automatically. COSMIC uses it for screen recording.

- **Event loop**: Hybrid calloop + tokio. Compositor core runs on the main
  thread with calloop (Smithay requires this). Protocol servers and encoding
  run on tokio worker threads. Bridged via channels (crossbeam or
  tokio::sync::mpsc fed into calloop's channel source).

  ```
  Main thread (calloop)              Worker threads (tokio)
    Smithay compositor  --frame-->     Encoder
    Output manager      --frame-->     Protocol server --> network
    Input router       <--input--     Protocol server <-- network
  ```

- **Config format**: KDL. Natural fit for compositor config (nodes with
  children), and what modern Wayland compositors are converging on (niri,
  zellij).

- **Session model**: Session takeover as default (lock local screen on remote
  connect, like Windows RDP). Shared-seat for screen sharing. Multi-seat
  independent sessions deferred to future work. SeatId on all input events
  from day one so the architecture supports multi-seat later.

- **XWayland**: Deferred, but fast-follow after VNC works. Smithay has
  built-in support, so the integration is bounded. Needed before daily
  driving (Electron apps, games, some IDEs still need X11).

- **Clipboard and audio**: Placeholder hooks in RemoteProtocol trait from
  the start. Implementation is a fast-follow after basic remote rendering
  works. Clipboard bridges wl_data_device <-> protocol-specific channels
  (CLIPRDR for RDP, extended clipboard for VNC). Audio captures PipeWire
  output, encodes via Opus, forwards over protocol channels.

- **Sunshine**: Deferred. Hardest integration (poorly documented protocol,
  C++ codebase). Revisit after VNC and RDP are solid. Pragmatic path may be
  contributing upstream to make Sunshine usable as a library.

## Open Questions

- **VNC implementation**: Rust VNC crate quality varies. Options are: use an
  existing crate (e.g., `vnc-rs`), wrap a C library (libvncserver), or
  implement RFB directly (the protocol is relatively simple). Need to
  evaluate what exists.

- **Lock screen integration**: session takeover needs to lock/unlock the
  local display. How does this interact with system lock (logind, PAM)?
  Do we implement our own lock screen, or delegate to an external locker?

- **Audio capture**: PipeWire virtual sink per remote session vs capturing
  the default output. Per-session sinks are cleaner (remote user gets their
  own audio) but more complex.

- **TLS certificates**: auto-generate self-signed? Integrate with Let's
  Encrypt via Tailscale certs? Accept user-provided certs? Probably all
  three, with Tailscale certs as the happy path.

- **Graceful degradation**: what happens when a remote connection is slow?
  Dynamic quality/resolution reduction? Frame skipping? This matters a lot
  for usability but can be iterated on.
