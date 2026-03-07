# Use Cases

## 1. Headless Remote Desktop in a Container

**Scenario**: A containerized work environment (systemd-nspawn, Docker, etc.)
runs on a headless server. No physical display. Users connect remotely via
Moonlight, RDP, or VNC to get a full desktop.

**Today's pain (sway + Sunshine stack)**:
- Sway refuses to run with proprietary NVIDIA drivers without `--unsupported-gpu`
- Sway's gles2 renderer fails inside nspawn because `DRM_IOCTL_MODE_CREATE_DUMB`
  is denied by cgroup restrictions
- Falling back to `WLR_RENDERER=pixman` works but then NVENC can't initialize
  (needs an EGL/GL context that pixman doesn't create)
- Result: forced into software encoding even when NVENC hardware is available
- `nvidia_drm modeset=1` is required for DRM dumb buffers, but that's a kernel
  module parameter requiring a host reboot
- Systemd services inside nspawn can't access GPU devices (cgroup restrictions),
  requiring ugly nohup/nsenter workarounds
- NVIDIA EGL vendor ICD JSON must be manually placed inside the container
- Device group GIDs (input, video, render) differ between host and container,
  causing permission failures on bind-mounted devices like `/dev/uinput`
- Sway creates its Wayland socket with unpredictable names (`wayland-0` vs
  `wayland-1`), requiring detection scripts

**What Desktopinator should do**:
- Detect the environment (container, bare metal, GPU available, cgroup
  restrictions) and automatically choose the best renderer + encoder combination
- Create headless outputs without requiring DRM access when DRM is unavailable
- Use NVENC/VAAPI when a GPU is accessible, degrade gracefully to software
  encoding when it isn't, without user intervention
- Run as a single process (compositor + streaming server), eliminating the need
  to coordinate separate sway and Sunshine processes
- Predictable IPC socket path (no wayland-0/wayland-1 guessing)
- Work inside containers without special kernel module parameters or host reboots


## 2. Session Takeover from Physical to Remote

**Scenario**: Developer is working at their desk on a physical monitor. They
close the laptop and walk to the couch / leave for a trip. They connect with
Moonlight from another machine and pick up exactly where they left off.

**Requirements**:
- On remote connect: lock or blank the physical display, create virtual output(s)
  at the client's resolution, migrate windows to virtual outputs, re-tile
- On remote disconnect: destroy virtual outputs, unlock physical display,
  migrate windows back to physical outputs, re-tile
- Window state (positions, workspaces, focus) should feel continuous -- no
  jarring rearrangement
- Support asymmetric setups: physical might be a single 4K monitor, remote
  might be two 1080p displays, or vice versa


## 3. Multi-Architecture GPU Encoding

**Scenario**: The same compositor binary runs on x86_64 (Intel/AMD) and aarch64
(NVIDIA Grace/GB10, Apple Silicon via Asahi). Hardware encoding capabilities
vary wildly.

**Requirements**:
- Probe available encoders at startup (VAAPI, NVENC, V4L2 M2M, VideoToolbox)
- Select the best available encoder per-output (different remote clients might
  get different encoders)
- Handle NVENC quirks: needs CUDA context, may need modeset, doesn't work
  without GL context in some configurations
- Handle VAAPI quirks: different entry points for Intel vs AMD
- Software fallback (libx264 / openh264) must always be available and functional
- Report encoding method to the user (status bar, IPC query, logs)


## 4. Streaming to Multiple Clients Simultaneously

**Scenario**: One machine hosts multiple isolated work environments. Alice
connects from her laptop and gets workspace 1-3. Bob connects from his tablet
and gets workspace 4. Or: a demo machine streams to several viewers at once.

**Requirements**:
- Each remote connection gets its own virtual output(s) and encoding pipeline
- Independent resolution, refresh rate, and encoding per client
- Input routing: each client controls their own virtual outputs only (unless in
  shared-seat mode)
- Resource budgeting: don't let 5 simultaneous clients each demanding 4K NVENC
  starve the GPU


## 5. Container-Isolated Work Domains

**Scenario**: Security-conscious user runs different work domains in separate
containers (e.g., client-A work in one container, personal browsing in another,
research in a third). Each container runs its own compositor instance. The user
switches between them from a single Moonlight/RDP client.

**Requirements**:
- Each instance runs independently inside its own container
- Each instance can advertise itself on the LAN (mDNS) with a distinct name
- Distinct visual identity per instance (e.g., status bar color, border color)
  so the user always knows which domain they're in
- Minimal resource footprint per instance (one compositor handles one domain,
  not a heavyweight VM)
- No host-side compositor needed -- each container runs headless


## 6. Development and Testing Without a Monitor

**Scenario**: Developer is building a Wayland app or compositor plugin. They
want to test on a remote headless machine (CI server, cloud VM, beefy build
box) without a physical display attached.

**Requirements**:
- `desktopinator --headless` starts cleanly with no DRM, no GPU, pure software
- Connect via VNC for quick visual feedback (lowest barrier to entry)
- IPC for scripted testing (create outputs, move windows, take screenshots)
- Nested mode (`desktopinator --nested`) for running inside another compositor
  during development


## 7. Dynamic Resolution Matching

**Scenario**: User connects from a laptop (1920x1200), disconnects, then
reconnects from a phone (2400x1080 scaled). Later reconnects from a 4K TV.

**Requirements**:
- Compositor creates virtual outputs matching the client's reported resolution
  and scale factor
- Windows re-tile smoothly on resolution change
- Encoder adjusts bitrate and quality based on resolution and available bandwidth
- Optional: remember per-client preferences (client X always gets 2560x1440)


## 8. Audio Forwarding

**Scenario**: User is streaming their desktop to a Moonlight client and wants
to hear application audio (video calls, music, notifications).

**Requirements**:
- Compositor manages a PipeWire virtual sink per remote session
- Applications inside the session output audio to the virtual sink
- Audio is encoded (Opus) and forwarded to the remote client
- On disconnect, the virtual sink is cleaned up
- No manual PipeWire configuration needed (today requires writing pipewire.conf.d
  files and hoping wireplumber picks them up)


## 9. Clipboard Forwarding

**Scenario**: User copies text or an image on their local machine, connects to
remote desktop, and wants to paste. And vice versa.

**Requirements**:
- Bidirectional clipboard sync between local Wayland clipboard and remote
  protocol's clipboard channel
- Support text, images, and file URIs where the protocol allows
- Security: configurable per-connection (allow clipboard, deny clipboard,
  one-direction only)


## 10. Reconnection Without State Loss

**Scenario**: Network blip drops the remote connection for 30 seconds. User
reconnects. They should see their windows exactly as they left them, not a
fresh session.

**Requirements**:
- Virtual outputs persist for a configurable grace period after disconnect
  (e.g., 5 minutes)
- During grace period, windows stay in place, applications keep running
- Reconnecting within the grace period resumes the session seamlessly
- After grace period expires, follow the normal disconnect policy (migrate
  windows to remaining outputs, or keep them on now-orphaned virtual outputs)


## 11. Input Device Passthrough for Remote Sessions

**Scenario**: Remote user needs keyboard, mouse, and possibly gamepad input
to work in the remote desktop. Today, Sunshine needs `/dev/uinput` access and
`/dev/input/*` devices, which have permission and cgroup issues inside
containers.

**Today's pain**:
- Sunshine creates virtual input devices via uinput, which requires specific
  device permissions
- Container cgroups block access to input devices by default
- Host and container disagree on group GIDs for the `input` group, so
  bind-mounted devices have wrong permissions
- DeviceAllow overrides in systemd service files are needed but fragile

**What Desktopinator should do**:
- Handle input injection internally (no uinput dependency for remote input)
- Remote client input is translated directly to compositor input events, not
  routed through kernel input devices
- Physical input devices (libinput) are only needed for local sessions
- This eliminates an entire class of container permission problems


## 12. LAN Discovery and Zero-Config Pairing

**Scenario**: User has Moonlight on their laptop. They start Desktopinator on
their server. Moonlight should discover it automatically on the LAN, and
pairing should be simple.

**Requirements**:
- mDNS/DNS-SD advertisement of available protocol servers
- Configurable instance name (e.g., "work-cfa", "personal", "research")
- PIN-based pairing (like Sunshine/Moonlight) or Tailscale identity-based auth
- TLS with auto-generated certs (or Tailscale certs when available)


## 13. Graceful Degradation on Constrained Hardware

**Scenario**: Raspberry Pi or low-power ARM SBC used as a thin remote desktop
server. No GPU, limited CPU. Or: a cloud VM with no GPU.

**Requirements**:
- Software rendering (pixman/softbuffer) works without GPU at all
- Software encoding at reduced resolution/framerate when CPU is limited
- Adaptive frame rate: reduce to 30fps or lower if encoding can't keep up
- Report performance metrics via IPC so the user can make informed tradeoffs
