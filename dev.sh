#!/usr/bin/env bash
set -euo pipefail

# Build first
echo ":: building desktopinator"
cargo build 2>&1

LOGFILE=$(mktemp /tmp/desktopinator.XXXXXX.log)

echo ":: starting compositor (log: $LOGFILE)"

# Start compositor in background, capture output to find the socket name
# Pass --headless to desktopinator if HEADLESS=1 is set
DINATOR_ARGS=()
if [ "${HEADLESS:-}" = "1" ]; then
    DINATOR_ARGS+=(--headless)
fi
cargo run -- "${DINATOR_ARGS[@]}" 2>&1 | tee "$LOGFILE" &
COMPOSITOR_PID=$!
trap 'echo ":: shutting down"; kill $COMPOSITOR_PID 2>/dev/null; wait $COMPOSITOR_PID 2>/dev/null; rm -f "$LOGFILE"' EXIT

# Wait for the socket name to appear in the log
echo ":: waiting for compositor..."
SOCKET=""
for i in $(seq 1 50); do
    if [ -f "$LOGFILE" ]; then
        SOCKET=$(sed 's/\x1b\[[0-9;]*m//g' "$LOGFILE" 2>/dev/null | grep -oP 'wayland socket listening socket=\K\S+' || true)
        if [ -n "$SOCKET" ]; then
            break
        fi
    fi
    sleep 0.1
done

if [ -z "$SOCKET" ]; then
    echo ":: ERROR: could not detect compositor socket"
    echo ":: log output:"
    cat "$LOGFILE"
    exit 1
fi

echo ":: compositor ready on $SOCKET"
export WAYLAND_DISPLAY="$SOCKET"

sleep 0.5

# Launch clients -- pass them as arguments, or use defaults
if [ $# -gt 0 ]; then
    for cmd in "$@"; do
        echo ":: launching: $cmd"
        $cmd &
    done
else
    if command -v foot &>/dev/null; then
        echo ":: launching foot (WAYLAND_DISPLAY=$SOCKET)"
        foot &
    elif command -v alacritty &>/dev/null; then
        echo ":: launching alacritty (WAYLAND_DISPLAY=$SOCKET)"
        alacritty &
    else
        echo ":: no known wayland terminal found"
        echo ":: run clients manually with: WAYLAND_DISPLAY=$SOCKET <command>"
    fi
fi

# Wait for compositor to exit (close the winit window to quit)
wait $COMPOSITOR_PID
