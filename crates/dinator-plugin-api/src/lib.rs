use std::path::Path;

use dinator_tiling::Layout;
use serde::Deserialize;

/// Events the compositor sends to the plugin runtime.
#[derive(Debug, Clone)]
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

/// A keybinding a plugin wants to register.
#[derive(Debug, Clone)]
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

    /// Get information about all loaded plugins.
    fn plugin_info(&self) -> Vec<PluginInfo>;

    /// Reload all plugins from their sources.
    fn reload(&mut self) -> anyhow::Result<()>;
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
