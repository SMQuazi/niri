#!/bin/bash
# Wrapper script to run niri with crash logging

kill -9 $(lsof -t -i :24800) 2>/dev/null

LOG_FILE="/tmp/niri-crash.log"

# Unset GDK_BACKEND to allow portals to use Wayland
unset GDK_BACKEND

# Restart the portal with clean environment
killall xdg-desktop-portal-gnome 2>/dev/null
sleep 1
nohup /usr/local/libexec/xdg-desktop-portal-gnome > /tmp/portal.log 2>&1 &

echo "=== Niri started at $(date) ===" > "$LOG_FILE"

# Run niri with logging, capture output (use development build)
RUST_LOG=warn,niri=debug RUST_BACKTRACE=1 /home/likwidsage/source/niri/target/release/niri --session 2>&1 | tee -a "$LOG_FILE"

EXIT_CODE=$?

echo "=== Niri exited at $(date) with code $EXIT_CODE ===" >> "$LOG_FILE"

# If niri crashed, keep the log
if [ $EXIT_CODE -ne 0 ]; then
    echo "Niri crashed! Log saved to $LOG_FILE"
    sleep 10
fi
