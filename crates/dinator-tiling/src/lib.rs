/// A rectangle in logical coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// Identifies a window for layout purposes.
/// The tiling engine doesn't know about Smithay types -- it works with opaque IDs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WindowId(pub u64);

/// A positioned window: the layout engine's output.
#[derive(Debug, Clone, Copy)]
pub struct Placement {
    pub id: WindowId,
    pub rect: Rect,
}

/// Layout algorithms take a list of window IDs and an output area,
/// and return where each window should go.
pub trait Layout {
    fn arrange(&self, windows: &[WindowId], area: Rect) -> Vec<Placement>;
}

/// Master-stack layout: first window takes the left portion,
/// remaining windows stack vertically on the right.
pub struct ColumnLayout {
    /// Fraction of the output width given to the master window (0.0 - 1.0).
    pub main_ratio: f64,
    /// Gap in pixels between windows and around the edges.
    pub gap: i32,
}

impl Default for ColumnLayout {
    fn default() -> Self {
        Self {
            main_ratio: 0.55,
            gap: 4,
        }
    }
}

impl Layout for ColumnLayout {
    fn arrange(&self, windows: &[WindowId], area: Rect) -> Vec<Placement> {
        if windows.is_empty() {
            return Vec::new();
        }

        let g = self.gap;

        if windows.len() == 1 {
            return vec![Placement {
                id: windows[0],
                rect: Rect {
                    x: area.x + g,
                    y: area.y + g,
                    width: area.width - 2 * g,
                    height: area.height - 2 * g,
                },
            }];
        }

        let usable_width = area.width - 3 * g; // outer left + middle + outer right
        let main_width = (usable_width as f64 * self.main_ratio) as i32;
        let stack_width = usable_width - main_width;
        let stack_count = windows.len() - 1;
        let usable_height = area.height - 2 * g; // outer top + outer bottom
        let stack_usable = usable_height - (stack_count as i32 - 1) * g;
        let stack_height = stack_usable / stack_count as i32;

        let mut placements = Vec::with_capacity(windows.len());

        // Master window: left side
        placements.push(Placement {
            id: windows[0],
            rect: Rect {
                x: area.x + g,
                y: area.y + g,
                width: main_width,
                height: usable_height,
            },
        });

        // Stack windows: right side
        let stack_x = area.x + g + main_width + g;
        for (i, &id) in windows[1..].iter().enumerate() {
            let y = area.y + g + i as i32 * (stack_height + g);
            let height = if i == stack_count - 1 {
                // Last window gets remaining space (handles rounding)
                area.y + area.height - g - y
            } else {
                stack_height
            };

            placements.push(Placement {
                id,
                rect: Rect {
                    x: stack_x,
                    y,
                    width: stack_width,
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

    fn no_gap_layout(main_ratio: f64) -> ColumnLayout {
        ColumnLayout { main_ratio, gap: 0 }
    }

    #[test]
    fn empty_layout() {
        let layout = no_gap_layout(0.55);
        let result = layout.arrange(&[], Rect { x: 0, y: 0, width: 1920, height: 1080 });
        assert!(result.is_empty());
    }

    #[test]
    fn single_window_fills_area() {
        let layout = no_gap_layout(0.55);
        let area = Rect { x: 0, y: 0, width: 1920, height: 1080 };
        let result = layout.arrange(&[WindowId(1)], area);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].rect, area);
    }

    #[test]
    fn two_windows_split() {
        let layout = no_gap_layout(0.5);
        let area = Rect { x: 0, y: 0, width: 1000, height: 500 };
        let result = layout.arrange(&[WindowId(1), WindowId(2)], area);

        assert_eq!(result.len(), 2);
        assert_eq!(result[0].rect, Rect { x: 0, y: 0, width: 500, height: 500 });
        assert_eq!(result[1].rect, Rect { x: 500, y: 0, width: 500, height: 500 });
    }

    #[test]
    fn three_windows_master_plus_stack() {
        let layout = no_gap_layout(0.5);
        let area = Rect { x: 0, y: 0, width: 1000, height: 600 };
        let result = layout.arrange(&[WindowId(1), WindowId(2), WindowId(3)], area);

        assert_eq!(result.len(), 3);
        assert_eq!(result[0].rect, Rect { x: 0, y: 0, width: 500, height: 600 });
        assert_eq!(result[1].rect, Rect { x: 500, y: 0, width: 500, height: 300 });
        assert_eq!(result[2].rect, Rect { x: 500, y: 300, width: 500, height: 300 });
    }

    #[test]
    fn gaps_applied() {
        let layout = ColumnLayout { main_ratio: 0.5, gap: 10 };
        let area = Rect { x: 0, y: 0, width: 1000, height: 500 };
        let result = layout.arrange(&[WindowId(1), WindowId(2)], area);

        assert_eq!(result.len(), 2);
        // Master: 10px from left, 10px gap to stack
        assert_eq!(result[0].rect.x, 10);
        assert_eq!(result[0].rect.y, 10);
        assert_eq!(result[0].rect.width, 485); // (1000 - 30) * 0.5
        assert_eq!(result[0].rect.height, 480); // 500 - 20
        // Stack: after master + gap, 10px from right
        assert_eq!(result[1].rect.x, 10 + 485 + 10);
        assert_eq!(result[1].rect.width, 485);
    }
}
