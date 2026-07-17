#!/bin/sh
# Install disksun's .desktop file and icon so it appears in the GUI app
# menu on freedesktop-compliant desktops (GNOME, KDE, XFCE, Cinnamon,
# Wayland compositors with an app-launcher, etc.).
#
# Run this after installing the `disksun` binary (e.g. via
# `cargo install --git https://github.com/zoraster-org/disksun`).
# Re-run any time to reinstall.
set -eu

: "${XDG_DATA_HOME:=$HOME/.local/share}"

apps_dir="$XDG_DATA_HOME/applications"
icons_dir="$XDG_DATA_HOME/icons/hicolor/scalable/apps"

mkdir -p "$apps_dir" "$icons_dir"

here=$(cd -- "$(dirname -- "$0")" && pwd)

if [ ! -f "$here/disksun.desktop" ] || [ ! -f "$here/disksun.svg" ]; then
  printf 'error: expected disksun.desktop and disksun.svg next to this script (in %s)\n' "$here" >&2
  exit 1
fi

install -m 0644 "$here/disksun.desktop" "$apps_dir/disksun.desktop"
install -m 0644 "$here/disksun.svg"     "$icons_dir/disksun.svg"

# Best-effort cache refreshes; skip silently if the tools aren't present.
command -v update-desktop-database >/dev/null 2>&1 \
  && update-desktop-database "$apps_dir" >/dev/null 2>&1 || true
command -v gtk-update-icon-cache >/dev/null 2>&1 \
  && gtk-update-icon-cache -q "$XDG_DATA_HOME/icons/hicolor" >/dev/null 2>&1 || true

printf 'Installed:\n  %s\n  %s\n' \
  "$apps_dir/disksun.desktop" "$icons_dir/disksun.svg"
printf '\nYou may need to log out and back in for the entry to appear.\n'
