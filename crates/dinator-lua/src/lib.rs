use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;

use mlua::prelude::*;
use tracing::{info, warn};

use dinator_plugin_api::{
    KeybindingRequest, PluginAction, PluginEvent, PluginInfo, PluginRuntime, RawPluginManifest,
    WindowRule, parse_lua_manifest,
};
use dinator_tiling::{Layout, Placement, Rect, WindowId};

/// Convert mlua::Error to anyhow::Error.
/// mlua 0.10's Error contains Arc<dyn Error> (not Send+Sync),
/// so we can't use `?` directly with anyhow.
fn lua_err(e: mlua::Error) -> anyhow::Error {
    anyhow::anyhow!("{e}")
}

/// Plugin state: Lua VM is NOT behind RefCell (it's borrowed during exec),
/// but the mutable data IS (so callbacks can modify it during exec).
struct LuaPluginState {
    lua: Lua,
    data: RefCell<LuaPluginData>,
}

struct LuaPluginData {
    manifest: RawPluginManifest,
    /// Layout arrange functions stored by name (registry keys).
    layout_fns: HashMap<String, LuaRegistryKey>,
    /// Event handler functions stored by event name.
    event_handlers: HashMap<String, Vec<LuaRegistryKey>>,
    /// Keybinding requests accumulated during plugin load.
    keybinding_requests: Vec<KeybindingRequest>,
    /// Callback functions stored by ID.
    callbacks: HashMap<String, LuaRegistryKey>,
    /// Source path or name (for reload).
    source_path: Option<String>,
    /// Queued actions to be drained by the compositor.
    action_queue: Vec<PluginAction>,
    /// Window rules registered by the plugin.
    window_rules: Vec<WindowRule>,
}

/// The Lua plugin runtime. Manages one Lua VM per plugin.
pub struct LuaRuntime {
    plugins: Vec<Rc<LuaPluginState>>,
    /// Plugin directory for reload.
    plugin_dir: Option<String>,
}

impl LuaRuntime {
    pub fn new() -> Self {
        Self {
            plugins: Vec::new(),
            plugin_dir: None,
        }
    }

    fn load_single_plugin(
        &mut self,
        manifest: RawPluginManifest,
        source: &str,
        source_path: Option<String>,
    ) -> anyhow::Result<()> {
        let lua = Lua::new_with(
            LuaStdLib::TABLE | LuaStdLib::STRING | LuaStdLib::MATH | LuaStdLib::UTF8,
            LuaOptions::default(),
        ).map_err(lua_err)?;

        // 64 MB memory limit
        let _ = lua.set_memory_limit(64 * 1024 * 1024);

        let plugin_id = manifest.id.clone();

        let state = Rc::new(LuaPluginState {
            lua,
            data: RefCell::new(LuaPluginData {
                manifest,
                layout_fns: HashMap::new(),
                event_handlers: HashMap::new(),
                keybinding_requests: Vec::new(),
                callbacks: HashMap::new(),
                source_path,
                action_queue: Vec::new(),
                window_rules: Vec::new(),
            }),
        });

        // Register the `desktopinator` API table before loading plugin source
        Self::register_api(&state)?;

        // Execute the plugin source.
        // The Lua VM is NOT behind RefCell, so callbacks can borrow_mut the data
        // while exec() is running without hitting a borrow conflict.
        state.lua.load(source).set_name(&plugin_id).exec().map_err(lua_err)?;

        info!(plugin = %plugin_id, "loaded Lua plugin");
        self.plugins.push(state);
        Ok(())
    }

    fn register_api(state: &Rc<LuaPluginState>) -> anyhow::Result<()> {
        let dt = state.lua.create_table().map_err(lua_err)?;

        // desktopinator.register_layout(name, arrange_fn)
        {
            let state_ref = Rc::clone(state);
            let register_layout = state.lua.create_function(
                move |_lua, (name, func): (String, LuaFunction)| {
                    let mut data = state_ref.data.borrow_mut();
                    let key = state_ref.lua.create_registry_value(func)?;
                    data.layout_fns.insert(name, key);
                    Ok(())
                },
            ).map_err(lua_err)?;
            dt.set("register_layout", register_layout).map_err(lua_err)?;
        }

        // desktopinator.on(event_name, handler_fn)
        {
            let state_ref = Rc::clone(state);
            let on_event = state.lua.create_function(
                move |_lua, (event_name, func): (String, LuaFunction)| {
                    let mut data = state_ref.data.borrow_mut();
                    let key = state_ref.lua.create_registry_value(func)?;
                    data.event_handlers
                        .entry(event_name)
                        .or_default()
                        .push(key);
                    Ok(())
                },
            ).map_err(lua_err)?;
            dt.set("on", on_event).map_err(lua_err)?;
        }

        // desktopinator.bind(mods_table, key, callback_fn)
        {
            let state_ref = Rc::clone(state);
            let bind = state.lua.create_function(
                move |_lua, (mods, key, func): (LuaTable, String, LuaFunction)| {
                    let mut data = state_ref.data.borrow_mut();
                    let modifiers: Vec<String> = mods
                        .sequence_values::<String>()
                        .collect::<LuaResult<Vec<_>>>()?;

                    let callback_id =
                        format!("{}:{}+{}", data.manifest.id, modifiers.join("+"), key);
                    let reg_key = state_ref.lua.create_registry_value(func)?;
                    data.callbacks.insert(callback_id.clone(), reg_key);
                    data.keybinding_requests.push(KeybindingRequest {
                        modifiers,
                        key,
                        callback_id,
                    });
                    Ok(())
                },
            ).map_err(lua_err)?;
            dt.set("bind", bind).map_err(lua_err)?;
        }

        // desktopinator.log(msg)
        {
            let log_fn = state.lua.create_function(|_lua, msg: String| {
                info!(plugin_log = %msg);
                Ok(())
            }).map_err(lua_err)?;
            dt.set("log", log_fn).map_err(lua_err)?;
        }

        // desktopinator.spawn(cmd, args?)
        {
            let state_ref = Rc::clone(state);
            let spawn_fn = state.lua.create_function(
                move |_lua, (cmd, args): (String, Option<LuaTable>)| {
                    let args_vec = if let Some(tbl) = args {
                        tbl.sequence_values::<String>()
                            .collect::<LuaResult<Vec<_>>>()?
                    } else {
                        Vec::new()
                    };
                    state_ref.data.borrow_mut().action_queue.push(
                        PluginAction::Spawn { cmd, args: args_vec },
                    );
                    Ok(())
                },
            ).map_err(lua_err)?;
            dt.set("spawn", spawn_fn).map_err(lua_err)?;
        }

        // desktopinator.set_layout(name)
        {
            let state_ref = Rc::clone(state);
            let set_layout_fn = state.lua.create_function(
                move |_lua, name: String| {
                    state_ref.data.borrow_mut().action_queue.push(
                        PluginAction::SetLayout { name },
                    );
                    Ok(())
                },
            ).map_err(lua_err)?;
            dt.set("set_layout", set_layout_fn).map_err(lua_err)?;
        }

        // desktopinator.focus_next()
        {
            let state_ref = Rc::clone(state);
            let fn_ = state.lua.create_function(move |_lua, ()| {
                state_ref.data.borrow_mut().action_queue.push(PluginAction::FocusNext);
                Ok(())
            }).map_err(lua_err)?;
            dt.set("focus_next", fn_).map_err(lua_err)?;
        }

        // desktopinator.focus_prev()
        {
            let state_ref = Rc::clone(state);
            let fn_ = state.lua.create_function(move |_lua, ()| {
                state_ref.data.borrow_mut().action_queue.push(PluginAction::FocusPrev);
                Ok(())
            }).map_err(lua_err)?;
            dt.set("focus_prev", fn_).map_err(lua_err)?;
        }

        // desktopinator.close_window()
        {
            let state_ref = Rc::clone(state);
            let fn_ = state.lua.create_function(move |_lua, ()| {
                state_ref.data.borrow_mut().action_queue.push(PluginAction::CloseWindow);
                Ok(())
            }).map_err(lua_err)?;
            dt.set("close_window", fn_).map_err(lua_err)?;
        }

        // desktopinator.swap_master()
        {
            let state_ref = Rc::clone(state);
            let fn_ = state.lua.create_function(move |_lua, ()| {
                state_ref.data.borrow_mut().action_queue.push(PluginAction::SwapMaster);
                Ok(())
            }).map_err(lua_err)?;
            dt.set("swap_master", fn_).map_err(lua_err)?;
        }

        // desktopinator.toggle_float()
        {
            let state_ref = Rc::clone(state);
            let fn_ = state.lua.create_function(move |_lua, ()| {
                state_ref.data.borrow_mut().action_queue.push(PluginAction::ToggleFloat);
                Ok(())
            }).map_err(lua_err)?;
            dt.set("toggle_float", fn_).map_err(lua_err)?;
        }

        // desktopinator.toggle_fullscreen()
        {
            let state_ref = Rc::clone(state);
            let fn_ = state.lua.create_function(move |_lua, ()| {
                state_ref.data.borrow_mut().action_queue.push(PluginAction::ToggleFullscreen);
                Ok(())
            }).map_err(lua_err)?;
            dt.set("toggle_fullscreen", fn_).map_err(lua_err)?;
        }

        // desktopinator.window_rule({ app_id = "...", title = "...", float = true, fullscreen = false })
        {
            let state_ref = Rc::clone(state);
            let window_rule_fn = state.lua.create_function(
                move |_lua, tbl: LuaTable| {
                    let app_id: Option<String> = tbl.get("app_id").ok();
                    let title: Option<String> = tbl.get("title").ok();
                    let float: bool = tbl.get("float").unwrap_or(false);
                    let fullscreen: bool = tbl.get("fullscreen").unwrap_or(false);
                    state_ref.data.borrow_mut().window_rules.push(WindowRule {
                        app_id,
                        title,
                        float,
                        fullscreen,
                    });
                    Ok(())
                },
            ).map_err(lua_err)?;
            dt.set("window_rule", window_rule_fn).map_err(lua_err)?;
        }

        state.lua.globals().set("desktopinator", dt).map_err(lua_err)?;

        Ok(())
    }
}

impl PluginRuntime for LuaRuntime {
    fn load_plugins(&mut self, plugin_dir: &Path) -> anyhow::Result<()> {
        self.plugin_dir = Some(plugin_dir.to_string_lossy().to_string());

        if !plugin_dir.exists() {
            info!(dir = %plugin_dir.display(), "plugin directory does not exist, skipping");
            return Ok(());
        }

        let entries = std::fs::read_dir(plugin_dir)?;
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "lua") {
                let source = std::fs::read_to_string(&path)?;
                let Some(manifest) = parse_lua_manifest(&source) else {
                    warn!(path = %path.display(), "skipping Lua file without manifest header");
                    continue;
                };
                if let Err(e) = self.load_single_plugin(
                    manifest,
                    &source,
                    Some(path.to_string_lossy().to_string()),
                ) {
                    warn!(path = %path.display(), error = %e, "failed to load plugin");
                }
            }
        }

        Ok(())
    }

    fn load_plugin_source(&mut self, name: &str, source: &str) -> anyhow::Result<()> {
        let Some(manifest) = parse_lua_manifest(source) else {
            anyhow::bail!("plugin '{name}' has no manifest header");
        };
        self.load_single_plugin(manifest, source, None)
    }

    fn layout_names(&self) -> Vec<String> {
        let mut names = Vec::new();
        for plugin in &self.plugins {
            let data = plugin.data.borrow();
            for name in data.layout_fns.keys() {
                names.push(name.clone());
            }
        }
        names
    }

    fn create_layout(&mut self, name: &str) -> Option<Box<dyn Layout>> {
        for plugin in &self.plugins {
            let data = plugin.data.borrow();
            if data.layout_fns.contains_key(name) {
                return Some(Box::new(LuaLayout {
                    state: Rc::clone(plugin),
                    layout_name: name.to_string(),
                }));
            }
        }
        None
    }

    fn on_event(&mut self, event: &PluginEvent) {
        let event_name = match event {
            PluginEvent::WindowOpened { .. } => "window_opened",
            PluginEvent::WindowClosed { .. } => "window_closed",
            PluginEvent::WindowFocused { .. } => "window_focused",
            PluginEvent::LayoutChanged { .. } => "layout_changed",
            PluginEvent::WorkspaceChanged { .. } => "workspace_changed",
            PluginEvent::WindowMovedWorkspace { .. } => "window_moved_workspace",
        };

        for plugin in &self.plugins {
            let data = plugin.data.borrow();
            let Some(handlers) = data.event_handlers.get(event_name) else {
                continue;
            };

            for key in handlers {
                let func: LuaFunction = match plugin.lua.registry_value(key) {
                    Ok(f) => f,
                    Err(e) => {
                        warn!(error = %e, "failed to get event handler from registry");
                        continue;
                    }
                };

                let arg = match event {
                    PluginEvent::WindowOpened { id, app_id, title } => {
                        let tbl = plugin.lua.create_table().ok();
                        if let Some(tbl) = &tbl {
                            let _ = tbl.set("id", *id);
                            if let Some(app_id) = app_id {
                                let _ = tbl.set("app_id", app_id.as_str());
                            }
                            if let Some(title) = title {
                                let _ = tbl.set("title", title.as_str());
                            }
                        }
                        tbl.map(LuaValue::Table)
                            .unwrap_or(LuaValue::Nil)
                    }
                    PluginEvent::WindowClosed { id } | PluginEvent::WindowFocused { id } => {
                        let tbl = plugin.lua.create_table().ok();
                        if let Some(tbl) = &tbl {
                            let _ = tbl.set("id", *id);
                        }
                        tbl.map(LuaValue::Table)
                            .unwrap_or(LuaValue::Nil)
                    }
                    PluginEvent::LayoutChanged { name } => {
                        let tbl = plugin.lua.create_table().ok();
                        if let Some(tbl) = &tbl {
                            let _ = tbl.set("name", name.as_str());
                        }
                        tbl.map(LuaValue::Table)
                            .unwrap_or(LuaValue::Nil)
                    }
                    PluginEvent::WorkspaceChanged { workspace } => {
                        let tbl = plugin.lua.create_table().ok();
                        if let Some(tbl) = &tbl {
                            let _ = tbl.set("workspace", *workspace);
                        }
                        tbl.map(LuaValue::Table)
                            .unwrap_or(LuaValue::Nil)
                    }
                    PluginEvent::WindowMovedWorkspace { id, workspace } => {
                        let tbl = plugin.lua.create_table().ok();
                        if let Some(tbl) = &tbl {
                            let _ = tbl.set("id", *id);
                            let _ = tbl.set("workspace", *workspace);
                        }
                        tbl.map(LuaValue::Table)
                            .unwrap_or(LuaValue::Nil)
                    }
                };

                // Drop the data borrow before calling into Lua,
                // since the callback might need to borrow_mut data
                drop(data);

                if let Err(e) = func.call::<()>(arg) {
                    let data = plugin.data.borrow();
                    warn!(
                        plugin = %data.manifest.id,
                        event = event_name,
                        error = %e,
                        "plugin event handler error"
                    );
                }

                // Re-borrow and break — we can't continue the loop
                // since we dropped the borrow. For multiple handlers,
                // we'd need to collect keys first.
                break;
            }
        }
    }

    fn drain_keybinding_requests(&mut self) -> Vec<KeybindingRequest> {
        let mut all = Vec::new();
        for plugin in &self.plugins {
            let mut data = plugin.data.borrow_mut();
            all.append(&mut data.keybinding_requests);
        }
        all
    }

    fn invoke_callback(&mut self, callback_id: &str) {
        for plugin in &self.plugins {
            let data = plugin.data.borrow();
            if let Some(key) = data.callbacks.get(callback_id) {
                let func: LuaFunction = match plugin.lua.registry_value(key) {
                    Ok(f) => f,
                    Err(e) => {
                        warn!(error = %e, "failed to get callback from registry");
                        return;
                    }
                };
                drop(data);
                if let Err(e) = func.call::<()>(()) {
                    warn!(
                        callback = callback_id,
                        error = %e,
                        "plugin callback error"
                    );
                }
                return;
            }
        }
    }

    fn drain_actions(&mut self) -> Vec<PluginAction> {
        let mut all = Vec::new();
        for plugin in &self.plugins {
            let mut data = plugin.data.borrow_mut();
            all.append(&mut data.action_queue);
        }
        all
    }

    fn drain_window_rules(&mut self) -> Vec<WindowRule> {
        let mut all = Vec::new();
        for plugin in &self.plugins {
            let mut data = plugin.data.borrow_mut();
            all.append(&mut data.window_rules);
        }
        all
    }

    fn plugin_info(&self) -> Vec<PluginInfo> {
        self.plugins
            .iter()
            .map(|p| {
                let data = p.data.borrow();
                PluginInfo {
                    id: data.manifest.id.clone(),
                    name: data.manifest.name.clone(),
                    version: data.manifest.version.clone(),
                    source: data
                        .source_path
                        .clone()
                        .unwrap_or_else(|| "builtin".to_string()),
                    layouts: data.layout_fns.keys().cloned().collect(),
                }
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

/// A layout backed by a Lua function.
struct LuaLayout {
    state: Rc<LuaPluginState>,
    layout_name: String,
}

impl Layout for LuaLayout {
    fn name(&self) -> &str {
        &self.layout_name
    }

    fn arrange(&self, windows: &[WindowId], area: Rect) -> Vec<Placement> {
        let data = self.state.data.borrow();
        let Some(key) = data.layout_fns.get(&self.layout_name) else {
            return Vec::new();
        };

        let func: LuaFunction = match self.state.lua.registry_value(key) {
            Ok(f) => f,
            Err(e) => {
                warn!(layout = %self.layout_name, error = %e, "failed to get layout fn");
                return Vec::new();
            }
        };

        // Drop data borrow before calling Lua (callback might need mutable access)
        drop(data);

        // Convert windows to Lua table
        let win_table = match self.state.lua.create_table() {
            Ok(t) => t,
            Err(_) => return Vec::new(),
        };
        for (i, w) in windows.iter().enumerate() {
            let _ = win_table.set(i + 1, w.0);
        }

        // Convert area to Lua table
        let area_table = match self.state.lua.create_table() {
            Ok(t) => t,
            Err(_) => return Vec::new(),
        };
        let _ = area_table.set("x", area.x);
        let _ = area_table.set("y", area.y);
        let _ = area_table.set("width", area.width);
        let _ = area_table.set("height", area.height);

        // Call the Lua function
        let result: LuaTable = match func.call((win_table, area_table)) {
            Ok(r) => r,
            Err(e) => {
                warn!(layout = %self.layout_name, error = %e, "layout arrange error");
                return Vec::new();
            }
        };

        // Parse placements from the returned table
        let mut placements = Vec::with_capacity(windows.len());
        for i in 1..=windows.len() {
            let Ok(entry): Result<LuaTable, _> = result.get(i) else {
                break;
            };
            let id: u64 = entry.get("id").unwrap_or(0);
            let x: i32 = entry.get("x").unwrap_or(0);
            let y: i32 = entry.get("y").unwrap_or(0);
            let width: i32 = entry.get("width").unwrap_or(0);
            let height: i32 = entry.get("height").unwrap_or(0);

            placements.push(Placement {
                id: WindowId(id),
                rect: Rect {
                    x,
                    y,
                    width,
                    height,
                },
            });
        }

        placements
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_grid_layout_plugin() {
        let source = r#"
--[[ [manifest]
id = "test-grid"
name = "Test Grid Layout"
version = "0.1.0"
]]

desktopinator.register_layout("grid", function(windows, area)
    local n = #windows
    if n == 0 then return {} end

    local cols = math.ceil(math.sqrt(n))
    local rows = math.ceil(n / cols)
    local cell_w = math.floor(area.width / cols)
    local cell_h = math.floor(area.height / rows)

    local placements = {}
    for i, id in ipairs(windows) do
        local col = (i - 1) % cols
        local row = math.floor((i - 1) / cols)
        placements[i] = {
            id = id,
            x = area.x + col * cell_w,
            y = area.y + row * cell_h,
            width = cell_w,
            height = cell_h,
        }
    end
    return placements
end)
"#;

        let mut runtime = LuaRuntime::new();
        runtime.load_plugin_source("test-grid", source).unwrap();

        assert_eq!(runtime.layout_names(), vec!["grid"]);

        let layout = runtime.create_layout("grid").unwrap();
        assert_eq!(layout.name(), "grid");

        // 4 windows should produce 2x2 grid
        let area = Rect {
            x: 0,
            y: 0,
            width: 1000,
            height: 1000,
        };
        let windows = vec![WindowId(1), WindowId(2), WindowId(3), WindowId(4)];
        let placements = layout.arrange(&windows, area);

        assert_eq!(placements.len(), 4);
        assert_eq!(placements[0].rect, Rect { x: 0, y: 0, width: 500, height: 500 });
        assert_eq!(placements[1].rect, Rect { x: 500, y: 0, width: 500, height: 500 });
        assert_eq!(placements[2].rect, Rect { x: 0, y: 500, width: 500, height: 500 });
        assert_eq!(placements[3].rect, Rect { x: 500, y: 500, width: 500, height: 500 });
    }

    #[test]
    fn test_event_handler() {
        let source = r#"
--[[ [manifest]
id = "test-events"
name = "Test Events"
version = "0.1.0"
]]

last_event = nil

desktopinator.on("window_opened", function(e)
    last_event = e.app_id
end)
"#;

        let mut runtime = LuaRuntime::new();
        runtime.load_plugin_source("test-events", source).unwrap();

        runtime.on_event(&PluginEvent::WindowOpened {
            id: 1,
            app_id: Some("foot".to_string()),
            title: Some("Terminal".to_string()),
        });

        // Verify the handler ran by checking the global
        let val: String = runtime.plugins[0].lua.globals().get("last_event").unwrap();
        assert_eq!(val, "foot");
    }

    #[test]
    fn test_single_window_grid() {
        let source = r#"
--[[ [manifest]
id = "test-grid-single"
name = "Grid Single"
version = "0.1.0"
]]

desktopinator.register_layout("grid", function(windows, area)
    local n = #windows
    if n == 0 then return {} end
    local cols = math.ceil(math.sqrt(n))
    local rows = math.ceil(n / cols)
    local cell_w = math.floor(area.width / cols)
    local cell_h = math.floor(area.height / rows)
    local placements = {}
    for i, id in ipairs(windows) do
        local col = (i - 1) % cols
        local row = math.floor((i - 1) / cols)
        placements[i] = {
            id = id, x = area.x + col * cell_w, y = area.y + row * cell_h,
            width = cell_w, height = cell_h,
        }
    end
    return placements
end)
"#;

        let mut runtime = LuaRuntime::new();
        runtime.load_plugin_source("test", source).unwrap();
        let layout = runtime.create_layout("grid").unwrap();

        let area = Rect { x: 0, y: 0, width: 1920, height: 1080 };
        let placements = layout.arrange(&[WindowId(1)], area);

        assert_eq!(placements.len(), 1);
        assert_eq!(placements[0].rect, area);
    }

    #[test]
    fn test_plugin_actions() {
        let source = r#"
--[[ [manifest]
id = "test-actions"
name = "Test Actions"
version = "0.1.0"
]]

desktopinator.on("window_opened", function(e)
    desktopinator.spawn("echo", {"hello"})
    desktopinator.set_layout("grid")
    desktopinator.focus_next()
end)
"#;

        let mut runtime = LuaRuntime::new();
        runtime.load_plugin_source("test-actions", source).unwrap();

        // No actions yet
        assert!(runtime.drain_actions().is_empty());

        // Trigger event that queues actions
        runtime.on_event(&PluginEvent::WindowOpened {
            id: 1,
            app_id: Some("test".to_string()),
            title: None,
        });

        let actions = runtime.drain_actions();
        assert_eq!(actions.len(), 3);
        assert!(matches!(&actions[0], PluginAction::Spawn { cmd, .. } if cmd == "echo"));
        assert!(matches!(&actions[1], PluginAction::SetLayout { name } if name == "grid"));
        assert!(matches!(&actions[2], PluginAction::FocusNext));

        // Queue should be empty after drain
        assert!(runtime.drain_actions().is_empty());
    }

    #[test]
    fn test_window_rules() {
        let source = r#"
--[[ [manifest]
id = "test-rules"
name = "Test Rules"
version = "0.1.0"
]]

desktopinator.window_rule({ app_id = "mpv", float = true })
desktopinator.window_rule({ title = "Settings", fullscreen = true })
"#;

        let mut runtime = LuaRuntime::new();
        runtime.load_plugin_source("test-rules", source).unwrap();

        let rules = runtime.drain_window_rules();
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].app_id.as_deref(), Some("mpv"));
        assert!(rules[0].float);
        assert!(!rules[0].fullscreen);
        assert_eq!(rules[1].title.as_deref(), Some("Settings"));
        assert!(!rules[1].float);
        assert!(rules[1].fullscreen);
    }
}
