use std::collections::HashMap;
use std::sync::OnceLock;

/// Cached cursor RGBA data: (width, height, hot_x, hot_y, rgba_data).
type CursorData = (u16, u16, u16, u16, Vec<u8>);

static CURSOR_CACHE: OnceLock<HashMap<String, CursorData>> = OnceLock::new();

/// Load a named cursor from the system xcursor theme.
/// Returns (width, height, hot_x, hot_y, rgba_data) or None if not found.
pub fn load_named_cursor(name: &str) -> Option<&'static CursorData> {
    let cache = CURSOR_CACHE.get_or_init(|| {
        let mut map = HashMap::new();
        load_theme_cursors(&mut map);
        map
    });
    cache.get(name)
}

/// Map CursorIcon names to xcursor theme names.
pub fn cursor_icon_to_name(icon: &smithay::input::pointer::CursorIcon) -> &'static str {
    use smithay::input::pointer::CursorIcon;
    match icon {
        CursorIcon::Default => "default",
        CursorIcon::ContextMenu => "context-menu",
        CursorIcon::Help => "help",
        CursorIcon::Pointer => "pointer",
        CursorIcon::Progress => "progress",
        CursorIcon::Wait => "wait",
        CursorIcon::Cell => "cell",
        CursorIcon::Crosshair => "crosshair",
        CursorIcon::Text => "text",
        CursorIcon::VerticalText => "vertical-text",
        CursorIcon::Alias => "alias",
        CursorIcon::Copy => "copy",
        CursorIcon::Move => "move",
        CursorIcon::NoDrop => "no-drop",
        CursorIcon::NotAllowed => "not-allowed",
        CursorIcon::Grab => "grab",
        CursorIcon::Grabbing => "grabbing",
        CursorIcon::EResize => "e-resize",
        CursorIcon::NResize => "n-resize",
        CursorIcon::NeResize => "ne-resize",
        CursorIcon::NwResize => "nw-resize",
        CursorIcon::SResize => "s-resize",
        CursorIcon::SeResize => "se-resize",
        CursorIcon::SwResize => "sw-resize",
        CursorIcon::WResize => "w-resize",
        CursorIcon::EwResize => "ew-resize",
        CursorIcon::NsResize => "ns-resize",
        CursorIcon::NeswResize => "nesw-resize",
        CursorIcon::NwseResize => "nwse-resize",
        CursorIcon::ColResize => "col-resize",
        CursorIcon::RowResize => "row-resize",
        CursorIcon::AllScroll => "all-scroll",
        CursorIcon::ZoomIn => "zoom-in",
        CursorIcon::ZoomOut => "zoom-out",
        _ => "default",
    }
}

fn load_theme_cursors(map: &mut HashMap<String, CursorData>) {
    let theme_name = std::env::var("XCURSOR_THEME").unwrap_or_else(|_| "default".to_string());
    let size: u32 = std::env::var("XCURSOR_SIZE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(24);

    let theme = xcursor::CursorTheme::load(&theme_name);

    let cursor_names = [
        "default", "left_ptr", "text", "xterm", "pointer", "hand2",
        "grab", "grabbing", "crosshair", "move", "fleur",
        "not-allowed", "crossed_circle",
        "n-resize", "top_side", "s-resize", "bottom_side",
        "e-resize", "right_side", "w-resize", "left_side",
        "ne-resize", "top_right_corner", "nw-resize", "top_left_corner",
        "se-resize", "bottom_right_corner", "sw-resize", "bottom_left_corner",
        "ns-resize", "sb_v_double_arrow", "ew-resize", "sb_h_double_arrow",
        "nesw-resize", "nwse-resize", "col-resize", "row-resize",
        "wait", "watch", "progress", "left_ptr_watch",
        "help", "question_arrow", "context-menu",
        "all-scroll", "zoom-in", "zoom-out",
    ];

    for name in &cursor_names {
        if map.contains_key(*name) {
            continue;
        }
        if let Some(path) = theme.load_icon(name) {
            if let Ok(data) = std::fs::read(&path) {
                if let Some(images) = xcursor::parser::parse_xcursor(&data) {
                    let best = images
                        .iter()
                        .min_by_key(|img| (img.size as i32 - size as i32).unsigned_abs());

                    if let Some(img) = best {
                        let w = img.width as u16;
                        let h = img.height as u16;
                        let hx = img.xhot as u16;
                        let hy = img.yhot as u16;

                        // pixels_rgba is RGBA but rows are bottom-up — flip vertically
                        let row_bytes = w as usize * 4;
                        let mut rgba = img.pixels_rgba.clone();
                        for y in 0..(h as usize / 2) {
                            let top = y * row_bytes;
                            let bot = (h as usize - 1 - y) * row_bytes;
                            for x in 0..row_bytes {
                                rgba.swap(top + x, bot + x);
                            }
                        }

                        map.insert(name.to_string(), (w, h, hx, hy, rgba));
                    }
                }
            }
        }
    }

    // Add aliases: map CSS cursor names to X11 cursor names
    let aliases = [
        ("default", "left_ptr"),
        ("text", "xterm"),
        ("pointer", "hand2"),
        ("move", "fleur"),
        ("not-allowed", "crossed_circle"),
        ("wait", "watch"),
        ("progress", "left_ptr_watch"),
        ("help", "question_arrow"),
        ("n-resize", "top_side"),
        ("s-resize", "bottom_side"),
        ("e-resize", "right_side"),
        ("w-resize", "left_side"),
        ("ne-resize", "top_right_corner"),
        ("nw-resize", "top_left_corner"),
        ("se-resize", "bottom_right_corner"),
        ("sw-resize", "bottom_left_corner"),
        ("ns-resize", "sb_v_double_arrow"),
        ("ew-resize", "sb_h_double_arrow"),
    ];

    for (alias, target) in &aliases {
        if !map.contains_key(*alias) {
            if let Some(data) = map.get(*target) {
                map.insert(alias.to_string(), data.clone());
            }
        }
    }

    tracing::info!(cursors = map.len(), theme = %theme_name, size, "loaded xcursor theme");
}
