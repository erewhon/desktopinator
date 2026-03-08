use extism_pdk::*;
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
struct ArrangeInput {
    layout: String,
    windows: Vec<u64>,
    area: Area,
}

#[derive(Deserialize)]
struct Area {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

#[derive(Serialize)]
struct Placement {
    id: u64,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

const GAP: i32 = 4;

/// Return the list of layouts this plugin provides.
#[plugin_fn]
pub fn get_layouts(_input: String) -> FnResult<String> {
    Ok(serde_json::to_string(&["wasm-grid"]).unwrap())
}

/// Arrange windows in a grid layout.
#[plugin_fn]
pub fn arrange(input: String) -> FnResult<String> {
    let input: ArrangeInput = serde_json::from_str(&input)?;
    let n = input.windows.len();

    if n == 0 {
        return Ok("[]".to_string());
    }

    let area = &input.area;

    // Single window fills the area
    if n == 1 {
        let placements = vec![Placement {
            id: input.windows[0],
            x: area.x + GAP,
            y: area.y + GAP,
            width: area.width - 2 * GAP,
            height: area.height - 2 * GAP,
        }];
        return Ok(serde_json::to_string(&placements).unwrap());
    }

    let cols = (n as f64).sqrt().ceil() as i32;
    let rows = ((n as f64) / cols as f64).ceil() as i32;

    let usable_w = area.width - (cols + 1) * GAP;
    let usable_h = area.height - (rows + 1) * GAP;
    let cell_w = usable_w / cols;
    let cell_h = usable_h / rows;

    let mut placements = Vec::with_capacity(n);

    for (i, &id) in input.windows.iter().enumerate() {
        let mut col = (i as i32) % cols;
        let row = (i as i32) / cols;
        let mut w = cell_w;

        // Last row: stretch if fewer windows
        if row == rows - 1 {
            let remaining = n as i32 - row * cols;
            if remaining < cols {
                let this_usable = area.width - (remaining + 1) * GAP;
                w = this_usable / remaining;
                col = i as i32 - row * cols;
            }
        }

        placements.push(Placement {
            id,
            x: area.x + GAP + col * (w + GAP),
            y: area.y + GAP + row * (cell_h + GAP),
            width: w,
            height: cell_h,
        });
    }

    Ok(serde_json::to_string(&placements).unwrap())
}
