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
    /// Layout name for IPC/display purposes.
    fn name(&self) -> &str;

    fn arrange(&self, windows: &[WindowId], area: Rect) -> Vec<Placement>;

    /// Increase the master area ratio by a step. Returns true if changed.
    fn grow_master(&mut self) -> bool { false }

    /// Decrease the master area ratio by a step. Returns true if changed.
    fn shrink_master(&mut self) -> bool { false }

    /// Current master ratio, if applicable.
    fn master_ratio(&self) -> Option<f64> { None }

    /// Set the gap between windows. Returns true if changed.
    fn set_gap(&mut self, _gap: i32) -> bool { false }

    /// Current gap in pixels.
    fn gap(&self) -> i32 { 0 }
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

const RATIO_STEP: f64 = 0.05;
const RATIO_MIN: f64 = 0.20;
const RATIO_MAX: f64 = 0.80;

impl Layout for ColumnLayout {
    fn name(&self) -> &str { "column" }

    fn grow_master(&mut self) -> bool {
        if self.main_ratio < RATIO_MAX {
            self.main_ratio = (self.main_ratio + RATIO_STEP).min(RATIO_MAX);
            true
        } else {
            false
        }
    }

    fn shrink_master(&mut self) -> bool {
        if self.main_ratio > RATIO_MIN {
            self.main_ratio = (self.main_ratio - RATIO_STEP).max(RATIO_MIN);
            true
        } else {
            false
        }
    }

    fn master_ratio(&self) -> Option<f64> {
        Some(self.main_ratio)
    }

    fn set_gap(&mut self, gap: i32) -> bool {
        if self.gap != gap { self.gap = gap; true } else { false }
    }

    fn gap(&self) -> i32 { self.gap }

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

/// Monocle layout: every window fills the entire output area.
/// Only the focused window is typically visible (compositor handles z-order).
pub struct MonocleLayout {
    /// Gap in pixels around the edges.
    pub gap: i32,
}

impl Default for MonocleLayout {
    fn default() -> Self {
        Self { gap: 0 }
    }
}

impl Layout for MonocleLayout {
    fn name(&self) -> &str { "monocle" }

    fn set_gap(&mut self, gap: i32) -> bool {
        if self.gap != gap { self.gap = gap; true } else { false }
    }

    fn gap(&self) -> i32 { self.gap }

    fn arrange(&self, windows: &[WindowId], area: Rect) -> Vec<Placement> {
        let g = self.gap;
        windows
            .iter()
            .map(|&id| Placement {
                id,
                rect: Rect {
                    x: area.x + g,
                    y: area.y + g,
                    width: area.width - 2 * g,
                    height: area.height - 2 * g,
                },
            })
            .collect()
    }
}

/// Dwindle layout: each window splits the remaining space in alternating
/// directions (horizontal, vertical, horizontal, ...).
/// First window gets the larger portion, each subsequent window subdivides
/// the remainder.
pub struct DwindleLayout {
    pub main_ratio: f64,
    pub gap: i32,
}

impl Default for DwindleLayout {
    fn default() -> Self {
        Self {
            main_ratio: 0.55,
            gap: 4,
        }
    }
}

impl Layout for DwindleLayout {
    fn name(&self) -> &str { "dwindle" }

    fn grow_master(&mut self) -> bool {
        if self.main_ratio < RATIO_MAX {
            self.main_ratio = (self.main_ratio + RATIO_STEP).min(RATIO_MAX);
            true
        } else {
            false
        }
    }

    fn shrink_master(&mut self) -> bool {
        if self.main_ratio > RATIO_MIN {
            self.main_ratio = (self.main_ratio - RATIO_STEP).max(RATIO_MIN);
            true
        } else {
            false
        }
    }

    fn master_ratio(&self) -> Option<f64> {
        Some(self.main_ratio)
    }

    fn set_gap(&mut self, gap: i32) -> bool {
        if self.gap != gap { self.gap = gap; true } else { false }
    }

    fn gap(&self) -> i32 { self.gap }

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

        let mut placements = Vec::with_capacity(windows.len());
        let mut remaining = Rect {
            x: area.x + g,
            y: area.y + g,
            width: area.width - 2 * g,
            height: area.height - 2 * g,
        };

        for (i, &id) in windows.iter().enumerate() {
            if i == windows.len() - 1 {
                // Last window gets all remaining space
                placements.push(Placement { id, rect: remaining });
                break;
            }

            let ratio = if i == 0 { self.main_ratio } else { 0.5 };
            let split_horizontal = i % 2 == 0;

            if split_horizontal {
                let w = (remaining.width as f64 * ratio) as i32;
                placements.push(Placement {
                    id,
                    rect: Rect {
                        x: remaining.x,
                        y: remaining.y,
                        width: w,
                        height: remaining.height,
                    },
                });
                remaining.x += w + g;
                remaining.width -= w + g;
            } else {
                let h = (remaining.height as f64 * ratio) as i32;
                placements.push(Placement {
                    id,
                    rect: Rect {
                        x: remaining.x,
                        y: remaining.y,
                        width: remaining.width,
                        height: h,
                    },
                });
                remaining.y += h + g;
                remaining.height -= h + g;
            }
        }

        placements
    }
}

/// Centered master layout: master window in the center,
/// stack windows split evenly on left and right sides.
pub struct CenteredMasterLayout {
    pub main_ratio: f64,
    pub gap: i32,
}

impl Default for CenteredMasterLayout {
    fn default() -> Self {
        Self {
            main_ratio: 0.50,
            gap: 4,
        }
    }
}

impl Layout for CenteredMasterLayout {
    fn name(&self) -> &str { "centered" }

    fn grow_master(&mut self) -> bool {
        if self.main_ratio < RATIO_MAX {
            self.main_ratio = (self.main_ratio + RATIO_STEP).min(RATIO_MAX);
            true
        } else {
            false
        }
    }

    fn shrink_master(&mut self) -> bool {
        if self.main_ratio > RATIO_MIN {
            self.main_ratio = (self.main_ratio - RATIO_STEP).max(RATIO_MIN);
            true
        } else {
            false
        }
    }

    fn master_ratio(&self) -> Option<f64> {
        Some(self.main_ratio)
    }

    fn set_gap(&mut self, gap: i32) -> bool {
        if self.gap != gap { self.gap = gap; true } else { false }
    }

    fn gap(&self) -> i32 { self.gap }

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

        if windows.len() == 2 {
            // Two windows: master left, stack right (like column)
            let usable = area.width - 3 * g;
            let main_w = (usable as f64 * self.main_ratio) as i32;
            let stack_w = usable - main_w;
            let h = area.height - 2 * g;
            return vec![
                Placement {
                    id: windows[0],
                    rect: Rect { x: area.x + g, y: area.y + g, width: main_w, height: h },
                },
                Placement {
                    id: windows[1],
                    rect: Rect { x: area.x + g + main_w + g, y: area.y + g, width: stack_w, height: h },
                },
            ];
        }

        // 3+ windows: center master, left stack, right stack
        let usable_w = area.width - 4 * g; // left-edge, left-center, center-right, right-edge
        let main_w = (usable_w as f64 * self.main_ratio) as i32;
        let side_w = (usable_w - main_w) / 2;
        let usable_h = area.height - 2 * g;

        let mut placements = Vec::with_capacity(windows.len());

        // Master: centered
        let master_x = area.x + g + side_w + g;
        placements.push(Placement {
            id: windows[0],
            rect: Rect {
                x: master_x,
                y: area.y + g,
                width: main_w,
                height: usable_h,
            },
        });

        // Distribute stack windows: odd indices go left, even go right
        let stack = &windows[1..];
        let mut left: Vec<WindowId> = Vec::new();
        let mut right: Vec<WindowId> = Vec::new();
        for (i, &id) in stack.iter().enumerate() {
            if i % 2 == 0 {
                left.push(id);
            } else {
                right.push(id);
            }
        }

        // Left stack
        if !left.is_empty() {
            let left_x = area.x + g;
            let count = left.len() as i32;
            let cell_h = (usable_h - (count - 1) * g) / count;
            for (i, &id) in left.iter().enumerate() {
                let y = area.y + g + i as i32 * (cell_h + g);
                let h = if i as i32 == count - 1 {
                    area.y + area.height - g - y
                } else {
                    cell_h
                };
                placements.push(Placement {
                    id,
                    rect: Rect { x: left_x, y, width: side_w, height: h },
                });
            }
        }

        // Right stack
        if !right.is_empty() {
            let right_x = master_x + main_w + g;
            let count = right.len() as i32;
            let cell_h = (usable_h - (count - 1) * g) / count;
            for (i, &id) in right.iter().enumerate() {
                let y = area.y + g + i as i32 * (cell_h + g);
                let h = if i as i32 == count - 1 {
                    area.y + area.height - g - y
                } else {
                    cell_h
                };
                placements.push(Placement {
                    id,
                    rect: Rect { x: right_x, y, width: side_w, height: h },
                });
            }
        }

        placements
    }
}

/// Stacked layout: windows are stacked vertically with title bar slots visible
/// at the top, and the focused (first) window takes the remaining space below.
/// Similar to i3's stacked mode.
pub struct StackedLayout {
    /// Height of each title bar slot in pixels.
    pub tab_height: i32,
    /// Gap in pixels around the edges.
    pub gap: i32,
}

impl Default for StackedLayout {
    fn default() -> Self {
        Self {
            tab_height: 24,
            gap: 4,
        }
    }
}

impl Layout for StackedLayout {
    fn name(&self) -> &str { "stacked" }

    fn set_gap(&mut self, gap: i32) -> bool {
        if self.gap != gap { self.gap = gap; true } else { false }
    }

    fn gap(&self) -> i32 { self.gap }

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

        let mut placements = Vec::with_capacity(windows.len());
        let usable_w = area.width - 2 * g;

        // Tab bar slots at the top for non-focused windows
        let tab_count = windows.len() - 1;
        let tabs_total_height = tab_count as i32 * self.tab_height;

        // Focused window (first) gets the main area below the tabs
        let main_y = area.y + g + tabs_total_height;
        let main_h = area.height - 2 * g - tabs_total_height;
        placements.push(Placement {
            id: windows[0],
            rect: Rect {
                x: area.x + g,
                y: main_y,
                width: usable_w,
                height: main_h.max(1),
            },
        });

        // Non-focused windows get thin tab slots at the top
        for (i, &id) in windows[1..].iter().enumerate() {
            let y = area.y + g + i as i32 * self.tab_height;
            placements.push(Placement {
                id,
                rect: Rect {
                    x: area.x + g,
                    y,
                    width: usable_w,
                    height: self.tab_height,
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
    fn dwindle_four_windows() {
        let layout = DwindleLayout { main_ratio: 0.5, gap: 0 };
        let area = Rect { x: 0, y: 0, width: 1000, height: 1000 };
        let windows = vec![WindowId(1), WindowId(2), WindowId(3), WindowId(4)];
        let result = layout.arrange(&windows, area);

        assert_eq!(result.len(), 4);
        // First: left half
        assert_eq!(result[0].rect, Rect { x: 0, y: 0, width: 500, height: 1000 });
        // Second: top-right
        assert_eq!(result[1].rect, Rect { x: 500, y: 0, width: 500, height: 500 });
        // Third: bottom-right-left
        assert_eq!(result[2].rect, Rect { x: 500, y: 500, width: 250, height: 500 });
        // Fourth: bottom-right-right (remaining)
        assert_eq!(result[3].rect, Rect { x: 750, y: 500, width: 250, height: 500 });
    }

    #[test]
    fn centered_three_windows() {
        let layout = CenteredMasterLayout { main_ratio: 0.5, gap: 0 };
        let area = Rect { x: 0, y: 0, width: 1000, height: 500 };
        let windows = vec![WindowId(1), WindowId(2), WindowId(3)];
        let result = layout.arrange(&windows, area);

        assert_eq!(result.len(), 3);
        // Master: centered
        assert_eq!(result[0].rect.x, 250);
        assert_eq!(result[0].rect.width, 500);
        // Left stack (window 2)
        assert_eq!(result[1].rect.x, 0);
        assert_eq!(result[1].rect.width, 250);
        // Right stack (window 3)
        assert_eq!(result[2].rect.x, 750);
        assert_eq!(result[2].rect.width, 250);
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
