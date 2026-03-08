use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use extism::{Manifest, Plugin, Wasm};
use tracing::{info, warn};

use dinator_plugin_api::{
    KeybindingRequest, PluginAction, PluginEvent, PluginInfo, PluginRuntime, RawPluginManifest,
    WindowRule,
};
use dinator_tiling::{Layout, Placement, Rect, WindowId};

/// A loaded WASM plugin instance.
struct WasmPluginState {
    plugin: Rc<RefCell<Plugin>>,
    manifest: RawPluginManifest,
    /// Layout names this plugin provides.
    layouts: Vec<String>,
    /// Path to the .wasm file (for reload).
    wasm_path: String,
    /// Path to the .toml manifest (for reload).
    #[allow(dead_code)]
    manifest_path: String,
}

/// The WASM plugin runtime. Manages Extism plugin instances.
pub struct WasmRuntime {
    plugins: Vec<WasmPluginState>,
    /// Plugin directory for reload.
    plugin_dir: Option<String>,
}

impl WasmRuntime {
    pub fn new() -> Self {
        Self {
            plugins: Vec::new(),
            plugin_dir: None,
        }
    }

    fn load_single_plugin(
        &mut self,
        manifest: RawPluginManifest,
        wasm_bytes: &[u8],
        wasm_path: String,
        manifest_path: String,
    ) -> anyhow::Result<()> {
        let extism_manifest = Manifest::new([Wasm::data(wasm_bytes)]);
        let mut plugin = Plugin::new(&extism_manifest, [], true)
            .map_err(|e| anyhow::anyhow!("failed to create WASM plugin: {e}"))?;

        let plugin_id = manifest.id.clone();

        // Query the plugin for its layout names
        let layouts = query_layouts(&mut plugin, &plugin_id);

        info!(
            plugin = %plugin_id,
            layouts = ?layouts,
            "loaded WASM plugin"
        );

        self.plugins.push(WasmPluginState {
            plugin: Rc::new(RefCell::new(plugin)),
            manifest,
            layouts,
            wasm_path,
            manifest_path,
        });

        Ok(())
    }
}

/// Call the plugin's `get_layouts` export to discover layout names.
fn query_layouts(plugin: &mut Plugin, plugin_id: &str) -> Vec<String> {
    match plugin.call::<&str, String>("get_layouts", "") {
        Ok(result) => match serde_json::from_str::<Vec<String>>(&result) {
            Ok(names) => names,
            Err(e) => {
                warn!(plugin = %plugin_id, error = %e, "failed to parse get_layouts result");
                Vec::new()
            }
        },
        Err(_) => Vec::new(),
    }
}

impl PluginRuntime for WasmRuntime {
    fn load_plugins(&mut self, plugin_dir: &Path) -> anyhow::Result<()> {
        self.plugin_dir = Some(plugin_dir.to_string_lossy().to_string());

        if !plugin_dir.exists() {
            return Ok(());
        }

        let entries = std::fs::read_dir(plugin_dir)?;
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "wasm") {
                let toml_path = path.with_extension("toml");
                if !toml_path.exists() {
                    warn!(path = %path.display(), "skipping WASM file without .toml manifest");
                    continue;
                }

                let manifest_str = std::fs::read_to_string(&toml_path)?;
                let manifest: RawPluginManifest = match toml::from_str(&manifest_str) {
                    Ok(m) => m,
                    Err(e) => {
                        warn!(path = %toml_path.display(), error = %e, "bad WASM plugin manifest");
                        continue;
                    }
                };

                let wasm_bytes = std::fs::read(&path)?;
                if let Err(e) = self.load_single_plugin(
                    manifest,
                    &wasm_bytes,
                    path.to_string_lossy().to_string(),
                    toml_path.to_string_lossy().to_string(),
                ) {
                    warn!(path = %path.display(), error = %e, "failed to load WASM plugin");
                }
            }
        }

        Ok(())
    }

    fn load_plugin_source(&mut self, _name: &str, _source: &str) -> anyhow::Result<()> {
        anyhow::bail!("WASM plugins cannot be loaded from source strings")
    }

    fn layout_names(&self) -> Vec<String> {
        self.plugins
            .iter()
            .flat_map(|p| p.layouts.iter().cloned())
            .collect()
    }

    fn create_layout(&mut self, name: &str) -> Option<Box<dyn Layout>> {
        for plugin in &self.plugins {
            if plugin.layouts.contains(&name.to_string()) {
                return Some(Box::new(WasmLayout {
                    plugin: Rc::clone(&plugin.plugin),
                    layout_name: name.to_string(),
                }));
            }
        }
        None
    }

    fn on_event(&mut self, event: &PluginEvent) {
        let event_json = match serde_json::to_string(event) {
            Ok(j) => j,
            Err(_) => return,
        };

        for plugin in &self.plugins {
            let mut p = plugin.plugin.borrow_mut();
            if let Err(e) = p.call::<&str, &str>("on_event", &event_json) {
                let err_str = e.to_string();
                if !err_str.contains("not found") && !err_str.contains("does not exist") {
                    warn!(plugin = %plugin.manifest.id, error = %e, "WASM on_event error");
                }
            }
        }
    }

    fn drain_keybinding_requests(&mut self) -> Vec<KeybindingRequest> {
        let mut all = Vec::new();
        for plugin in &self.plugins {
            let mut p = plugin.plugin.borrow_mut();
            match p.call::<&str, String>("get_keybindings", "") {
                Ok(result) => {
                    if let Ok(requests) = serde_json::from_str::<Vec<KeybindingRequest>>(&result) {
                        all.extend(requests);
                    }
                }
                Err(_) => {}
            }
        }
        all
    }

    fn invoke_callback(&mut self, callback_id: &str) {
        for plugin in &self.plugins {
            let mut p = plugin.plugin.borrow_mut();
            match p.call::<&str, &str>("invoke_callback", callback_id) {
                Ok(_) => return,
                Err(_) => continue,
            }
        }
    }

    fn drain_window_rules(&mut self) -> Vec<WindowRule> {
        let mut all = Vec::new();
        for plugin in &self.plugins {
            let mut p = plugin.plugin.borrow_mut();
            match p.call::<&str, String>("get_window_rules", "") {
                Ok(result) => {
                    if let Ok(rules) = serde_json::from_str::<Vec<WindowRule>>(&result) {
                        all.extend(rules);
                    }
                }
                Err(_) => {}
            }
        }
        all
    }

    fn drain_actions(&mut self) -> Vec<PluginAction> {
        let mut all = Vec::new();
        for plugin in &self.plugins {
            let mut p = plugin.plugin.borrow_mut();
            match p.call::<&str, String>("get_actions", "") {
                Ok(result) => {
                    if let Ok(actions) = serde_json::from_str::<Vec<PluginAction>>(&result) {
                        all.extend(actions);
                    }
                }
                Err(_) => {}
            }
        }
        all
    }

    fn plugin_info(&self) -> Vec<PluginInfo> {
        self.plugins
            .iter()
            .map(|p| PluginInfo {
                id: p.manifest.id.clone(),
                name: p.manifest.name.clone(),
                version: p.manifest.version.clone(),
                source: p.wasm_path.clone(),
                layouts: p.layouts.clone(),
            })
            .collect()
    }

    fn reload(&mut self) -> anyhow::Result<()> {
        let plugin_dir = self.plugin_dir.clone();
        self.plugins.clear();
        if let Some(dir) = plugin_dir {
            self.load_plugins(Path::new(&dir))?;
        }
        Ok(())
    }
}

/// A layout backed by a WASM plugin.
struct WasmLayout {
    plugin: Rc<RefCell<Plugin>>,
    layout_name: String,
}

#[derive(serde::Serialize)]
struct ArrangeInput {
    layout: String,
    windows: Vec<u64>,
    area: AreaJson,
}

#[derive(serde::Serialize)]
struct AreaJson {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

#[derive(serde::Deserialize)]
struct PlacementJson {
    id: u64,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

impl Layout for WasmLayout {
    fn name(&self) -> &str {
        &self.layout_name
    }

    fn arrange(&self, windows: &[WindowId], area: Rect) -> Vec<Placement> {
        let input = ArrangeInput {
            layout: self.layout_name.clone(),
            windows: windows.iter().map(|w| w.0).collect(),
            area: AreaJson {
                x: area.x,
                y: area.y,
                width: area.width,
                height: area.height,
            },
        };

        let input_json = match serde_json::to_string(&input) {
            Ok(j) => j,
            Err(e) => {
                warn!(error = %e, "failed to serialize arrange input");
                return Vec::new();
            }
        };

        let mut plugin = self.plugin.borrow_mut();
        let result: String = match plugin.call("arrange", &input_json) {
            Ok(r) => r,
            Err(e) => {
                warn!(layout = %self.layout_name, error = %e, "WASM arrange error");
                return Vec::new();
            }
        };

        let placements: Vec<PlacementJson> = match serde_json::from_str(&result) {
            Ok(p) => p,
            Err(e) => {
                warn!(layout = %self.layout_name, error = %e, "failed to parse arrange result");
                return Vec::new();
            }
        };

        placements
            .into_iter()
            .map(|p| Placement {
                id: WindowId(p.id),
                rect: Rect {
                    x: p.x,
                    y: p.y,
                    width: p.width,
                    height: p.height,
                },
            })
            .collect()
    }
}
