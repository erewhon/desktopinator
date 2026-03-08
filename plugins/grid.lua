--[[ [manifest]
id = "grid-layout"
name = "Grid Layout"
version = "0.1.0"
]]

-- Grid layout: arranges windows in an NxM grid, filling rows first.
-- Automatically determines the best number of columns based on window count.

local gap = 4

desktopinator.register_layout("grid", function(windows, area)
    local n = #windows
    if n == 0 then return {} end

    -- Single window fills the whole area
    if n == 1 then
        return {{
            id = windows[1],
            x = area.x + gap,
            y = area.y + gap,
            width = area.width - 2 * gap,
            height = area.height - 2 * gap,
        }}
    end

    local cols = math.ceil(math.sqrt(n))
    local rows = math.ceil(n / cols)

    local usable_w = area.width - (cols + 1) * gap
    local usable_h = area.height - (rows + 1) * gap
    local cell_w = math.floor(usable_w / cols)
    local cell_h = math.floor(usable_h / rows)

    local placements = {}
    for i, id in ipairs(windows) do
        local col = (i - 1) % cols
        local row = math.floor((i - 1) / cols)

        -- Last row may have fewer windows — stretch them wider
        local this_cols = cols
        if row == rows - 1 then
            local remaining = n - row * cols
            if remaining < cols then
                this_cols = remaining
                local this_usable = area.width - (this_cols + 1) * gap
                cell_w = math.floor(this_usable / this_cols)
                col = (i - 1) - row * cols
            end
        end

        placements[i] = {
            id = id,
            x = area.x + gap + col * (cell_w + gap),
            y = area.y + gap + row * (cell_h + gap),
            width = cell_w,
            height = cell_h,
        }
    end
    return placements
end)
