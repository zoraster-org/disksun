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
