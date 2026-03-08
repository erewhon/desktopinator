use std::path::Path;

use dinator_tiling::Layout;
use serde::{Deserialize, Serialize};

/// Events the compositor sends to the plugin runtime.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PluginEvent {
    WindowOpened {
        id: u64,
        app_id: Option<String>,
        title: Option<String>,
    },
    WindowClosed {
        id: u64,
    },
    WindowFocused {
        id: u64,
    },
    LayoutChanged {
        name: String,
    },
}

/// Actions a plugin can request the compositor to perform.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum PluginAction {
    Spawn { cmd: String, #[serde(default)] args: Vec<String> },
    SetLayout { name: String },
    FocusNext,
    FocusPrev,
    CloseWindow,
    SwapMaster,
    ToggleFloat,
    ToggleFullscreen,
    /// Log a message from the plugin.
    Log { message: String },
}

/// A window rule that matches windows by criteria and applies actions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowRule {
    /// Match by app_id (exact match).
    #[serde(default)]
    pub app_id: Option<String>,
    /// Match by title (substring match).
    #[serde(default)]
    pub title: Option<String>,
    /// Auto-float matched windows.
    #[serde(default)]
    pub float: bool,
    /// Auto-fullscreen matched windows.
    #[serde(default)]
    pub fullscreen: bool,
}

/// A keybinding a plugin wants to register.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeybindingRequest {
    pub modifiers: Vec<String>,
    pub key: String,
    pub callback_id: String,
}

/// Information about a loaded plugin.
#[derive(Debug, Clone)]
pub struct PluginInfo {
    pub id: String,
    pub name: String,
    pub version: String,
    pub source: String,
    pub layouts: Vec<String>,
}

/// The plugin runtime trait. Both Lua and (future) WASM runtimes implement this.
pub trait PluginRuntime {
    /// Load all plugin files from a directory.
    fn load_plugins(&mut self, plugin_dir: &Path) -> anyhow::Result<()>;

    /// Load a single plugin from source code (for built-in plugins).
    fn load_plugin_source(&mut self, name: &str, source: &str) -> anyhow::Result<()>;

    /// Get all layout names registered by plugins.
    fn layout_names(&self) -> Vec<String>;

    /// Create a layout instance by name.
    fn create_layout(&mut self, name: &str) -> Option<Box<dyn Layout>>;

    /// Notify plugins of a compositor event.
    fn on_event(&mut self, event: &PluginEvent);

    /// Drain any pending keybinding registration requests.
    fn drain_keybinding_requests(&mut self) -> Vec<KeybindingRequest>;

    /// Invoke a plugin callback by ID (from a keybinding).
    fn invoke_callback(&mut self, callback_id: &str);

    /// Drain any pending actions queued by plugins.
    fn drain_actions(&mut self) -> Vec<PluginAction>;

    /// Drain any window rules registered by plugins.
    fn drain_window_rules(&mut self) -> Vec<WindowRule>;

    /// Get information about all loaded plugins.
    fn plugin_info(&self) -> Vec<PluginInfo>;

    /// Reload all plugins from their sources.
    fn reload(&mut self) -> anyhow::Result<()>;
}

/// Composite runtime that delegates to multiple PluginRuntime implementations.
/// This allows Lua and WASM plugins to coexist in the same plugin directory.
pub struct CompositeRuntime {
    runtimes: Vec<Box<dyn PluginRuntime>>,
}

impl CompositeRuntime {
    pub fn new(runtimes: Vec<Box<dyn PluginRuntime>>) -> Self {
        Self { runtimes }
    }
}

impl PluginRuntime for CompositeRuntime {
    fn load_plugins(&mut self, plugin_dir: &Path) -> anyhow::Result<()> {
        for runtime in &mut self.runtimes {
            runtime.load_plugins(plugin_dir)?;
        }
        Ok(())
    }

    fn load_plugin_source(&mut self, name: &str, source: &str) -> anyhow::Result<()> {
        // Try each runtime until one succeeds
        for runtime in &mut self.runtimes {
            if runtime.load_plugin_source(name, source).is_ok() {
                return Ok(());
            }
        }
        anyhow::bail!("no runtime could load plugin '{name}'")
    }

    fn layout_names(&self) -> Vec<String> {
        self.runtimes
            .iter()
            .flat_map(|r| r.layout_names())
            .collect()
    }

    fn create_layout(&mut self, name: &str) -> Option<Box<dyn Layout>> {
        for runtime in &mut self.runtimes {
            if let Some(layout) = runtime.create_layout(name) {
                return Some(layout);
            }
        }
        None
    }

    fn on_event(&mut self, event: &PluginEvent) {
        for runtime in &mut self.runtimes {
            runtime.on_event(event);
        }
    }

    fn drain_keybinding_requests(&mut self) -> Vec<KeybindingRequest> {
        self.runtimes
            .iter_mut()
            .flat_map(|r| r.drain_keybinding_requests())
            .collect()
    }

    fn invoke_callback(&mut self, callback_id: &str) {
        for runtime in &mut self.runtimes {
            runtime.invoke_callback(callback_id);
        }
    }

    fn drain_actions(&mut self) -> Vec<PluginAction> {
        self.runtimes
            .iter_mut()
            .flat_map(|r| r.drain_actions())
            .collect()
    }

    fn drain_window_rules(&mut self) -> Vec<WindowRule> {
        self.runtimes
            .iter_mut()
            .flat_map(|r| r.drain_window_rules())
            .collect()
    }

    fn plugin_info(&self) -> Vec<PluginInfo> {
        self.runtimes
            .iter()
            .flat_map(|r| r.plugin_info())
            .collect()
    }

    fn reload(&mut self) -> anyhow::Result<()> {
        for runtime in &mut self.runtimes {
            runtime.reload()?;
        }
        Ok(())
    }
}

/// Raw TOML manifest parsed from Lua plugin header.
#[derive(Debug, Deserialize)]
pub struct RawPluginManifest {
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub hooks: Vec<String>,
}

/// Parse the `--[[ [manifest] ... ]]` TOML header from a Lua plugin file.
pub fn parse_lua_manifest(source: &str) -> Option<RawPluginManifest> {
    let start_marker = "--[[ [manifest]";
    let end_marker = "]]";

    let start = source.find(start_marker)?;
    let toml_start = start + start_marker.len();
    let toml_end = source[toml_start..].find(end_marker)?;
    let toml_str = &source[toml_start..toml_start + toml_end];

    toml::from_str(toml_str).ok()
}
