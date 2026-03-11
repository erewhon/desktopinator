use serde::{Deserialize, Serialize};

/// Default socket path: $XDG_RUNTIME_DIR/desktopinator.sock
pub fn socket_path() -> std::path::PathBuf {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
        .unwrap_or_else(|_| "/tmp".to_string());
    std::path::PathBuf::from(runtime_dir).join("desktopinator.sock")
}

/// A command sent from dinatorctl to the compositor.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "kebab-case")]
pub enum IpcCommand {
    /// Change the output resolution.
    Resize { width: u16, height: u16 },
    /// Focus the next window.
    FocusNext,
    /// Focus the previous window.
    FocusPrev,
    /// Close the focused window.
    Close,
    /// Swap the focused window with master.
    SwapMaster,
    /// Spawn a process.
    Spawn { cmd: String, args: Vec<String> },
    /// Quit the compositor.
    Quit,
    /// List all windows.
    ListWindows,
    /// Grow the master area.
    MasterGrow,
    /// Shrink the master area.
    MasterShrink,
    /// Set the tiling layout (e.g. "column", "monocle").
    SetLayout { name: String },
    /// Toggle the focused window between floating and tiled.
    ToggleFloat,
    /// Toggle fullscreen for the focused window.
    ToggleFullscreen,
    /// List all available layouts (built-in + plugin-provided).
    ListLayouts,
    /// List loaded plugins.
    ListPlugins,
    /// Reload plugins from disk.
    ReloadPlugins,
    /// Switch to a workspace (1-9).
    SwitchWorkspace { workspace: usize },
    /// Move the focused window to a workspace (1-9).
    MoveToWorkspace { workspace: usize },
    /// List all workspaces with window counts.
    ListWorkspaces,
    /// Set background to a solid color: "r,g,b" (0.0-1.0 or 0-255) or hex "#RRGGBB".
    SetBackground { spec: String },
    /// Set the gap (gutter) between windows in pixels.
    SetGap { pixels: i32 },
    /// Create a new headless output with the given dimensions.
    CreateOutput {
        name: String,
        width: u16,
        height: u16,
    },
    /// Remove a headless output by name.
    RemoveOutput { name: String },
    /// List all outputs.
    ListOutputs,
    /// Focus an output by name (it receives keyboard input).
    FocusOutput { name: String },
    /// Move the focused window to a different output.
    MoveWindowToOutput { name: String },
    /// Subscribe to compositor events. The connection switches to
    /// streaming mode: one JSON line per event, no further commands accepted.
    Subscribe,
    /// Get compositor status (connected clients, outputs, etc.).
    Status,
}

/// An event pushed from the compositor to subscribed IPC clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "kebab-case")]
pub enum IpcEvent {
    /// A new window was opened.
    WindowOpened {
        id: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        app_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    /// A window was closed.
    WindowClosed { id: u64 },
    /// A window gained focus.
    WindowFocused { id: u64 },
    /// The output resolution changed.
    ResolutionChanged { width: u16, height: u16 },
    /// The tiling layout changed.
    LayoutChanged { name: String },
    /// The active workspace changed.
    WorkspaceChanged { workspace: usize },
    /// A window was moved to a different workspace.
    WindowMovedWorkspace { id: u64, workspace: usize },
    /// A new output was created.
    OutputCreated { name: String, width: u16, height: u16 },
    /// An output was removed.
    OutputRemoved { name: String },
    /// An output gained focus.
    OutputFocused { name: String },
    /// A remote client connected.
    ClientConnected { protocol: String },
    /// A remote client disconnected.
    ClientDisconnected { protocol: String },
}

/// A response from the compositor back to dinatorctl.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum IpcResponse {
    /// Command executed successfully.
    Ok {
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    /// Command returned data.
    Data { data: serde_json::Value },
    /// Command failed.
    Error { message: String },
}
