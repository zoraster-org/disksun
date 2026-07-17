#!/bin/sh
# User-local installer bundled inside a release tarball. Extract the
# tarball, cd into the extracted directory, then run: ./install.sh
#
# Installs to XDG paths under $HOME — no root required, nothing outside
# your user's directories is modified.
set -eu

: "${XDG_DATA_HOME:=$HOME/.local/share}"

bin_dir="$HOME/.local/bin"
mkdir -p "$bin_dir"

here=$(cd -- "$(dirname -- "$0")" && pwd)

if [ ! -x "$here/disksun" ]; then
  printf 'error: expected the disksun binary next to this script (in %s)\n' "$here" >&2
  exit 1
fi

# ---- runtime dependency check ---------------------------------------
# disksun dlopens these at startup (they are not in the ELF NEEDED list,
# so a missing one only shows up as a crash at launch): check the
# linker cache and suggest — or, interactively, offer to run — the
# right package-manager command.
have_lib() {
  { ldconfig -p 2>/dev/null || /sbin/ldconfig -p 2>/dev/null; } | grep -q "$1"
}
libs="libwayland-client.so.0 libxkbcommon.so.0 libEGL.so.1 libGL.so.1"
if [ "${XDG_SESSION_TYPE:-}" = "x11" ]; then
  libs="$libs libX11.so.6 libXcursor.so.1"
fi
missing=""
for lib in $libs; do
  have_lib "$lib" || missing="$missing $lib"
done
if [ -n "$missing" ]; then
  printf '\nMissing runtime libraries:%s\n' "$missing"
  if command -v dnf >/dev/null 2>&1; then
    dep_cmd="sudo dnf install -y libwayland-client libxkbcommon libglvnd-egl libglvnd-glx libX11 libXcursor"
  elif command -v apt-get >/dev/null 2>&1; then
    dep_cmd="sudo apt-get install -y libwayland-client0 libxkbcommon0 libegl1 libgl1 libx11-6 libxcursor1"
  elif command -v pacman >/dev/null 2>&1; then
    dep_cmd="sudo pacman -S --needed wayland libxkbcommon libglvnd libx11 libxcursor"
  elif command -v zypper >/dev/null 2>&1; then
    dep_cmd="sudo zypper install libwayland-client0 libxkbcommon0 Mesa-libEGL1 Mesa-libGL1 libX11-6 libXcursor1"
  else
    dep_cmd=""
  fi
  if [ -z "$dep_cmd" ]; then
    printf "Install them with your distro's package manager, then re-run disksun.\n"
  elif [ -t 0 ]; then
    printf 'Install them now with:\n  %s\nRun it? [y/N] ' "$dep_cmd"
    read -r ans || ans=""
    case "$ans" in
      y|Y|yes|YES) $dep_cmd ;;
      *) printf 'Skipped — run it yourself before starting disksun.\n' ;;
    esac
  else
    # piped install (curl | sh): never run sudo without a real prompt
    printf 'Install them with:\n  %s\n' "$dep_cmd"
  fi
fi

install -m 0755 "$here/disksun" "$bin_dir/disksun"

if [ -x "$here/contrib/install-desktop.sh" ]; then
  "$here/contrib/install-desktop.sh"
fi

printf '\nInstalled:\n  %s\n' "$bin_dir/disksun"

case ":$PATH:" in
  *":$bin_dir:"*) ;;
  *)
    printf '\nNote: %s is not on your PATH.\n' "$bin_dir"
    printf '  Add this to your shell rc:  export PATH="%s:$PATH"\n' "$bin_dir"
    ;;
esac
