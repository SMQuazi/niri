#!/bin/bash
# Restart XDG Desktop Portals for niri with InputCapture support

echo "Stopping all portal processes..."
killall -9 xdg-desktop-portal xdg-desktop-portal-gnome xdg-desktop-portal-gtk 2>/dev/null

echo "Waiting for processes to terminate..."
sleep 2

echo "Starting GNOME portal backend..."
/usr/local/libexec/xdg-desktop-portal-gnome > /tmp/portal-gnome.log 2>&1 &
sleep 2

echo "Starting portal frontend..."
/usr/lib/xdg-desktop-portal > /tmp/portal-frontend.log 2>&1 &
sleep 3

echo "Verifying portals are running..."
ps aux | grep -E "xdg-desktop-portal" | grep -v grep

echo ""
echo "Portal logs:"
echo "  GNOME backend: /tmp/portal-gnome.log"
echo "  Frontend: /tmp/portal-frontend.log"
echo ""
echo "To check if InputCapture is loaded:"
echo "  busctl --user introspect org.freedesktop.impl.portal.desktop.gnome /org/freedesktop/portal/desktop | grep InputCapture"
