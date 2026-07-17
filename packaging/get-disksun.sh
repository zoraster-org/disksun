#!/bin/sh
# One-line installer. Detects your CPU, downloads the latest disksun
# release tarball from GitHub, extracts it, and runs its install.sh.
# No Rust toolchain required.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/zoraster-org/disksun/main/packaging/get-disksun.sh | sh
set -eu

repo="zoraster-org/disksun"

os=$(uname -s)
[ "$os" = "Linux" ] || { printf 'error: unsupported OS: %s (Linux only)\n' "$os" >&2; exit 1; }

arch=$(uname -m)
case "$arch" in
  x86_64|amd64)   target="x86_64-unknown-linux-gnu" ;;
  aarch64|arm64)  target="aarch64-unknown-linux-gnu" ;;
  *) printf 'error: unsupported architecture: %s (need x86_64 or aarch64)\n' "$arch" >&2; exit 1 ;;
esac

# github.com/OWNER/REPO/releases/latest 302-redirects to the tag URL;
# extract the tag from the effective URL — no jq, no JSON parsing.
tag=$(curl -fsSLI -o /dev/null -w '%{url_effective}\n' \
        "https://github.com/$repo/releases/latest" | sed 's:.*/::')

if [ -z "${tag:-}" ] || [ "$tag" = "latest" ]; then
  printf 'error: could not determine latest release tag for %s (no releases yet?)\n' "$repo" >&2
  exit 1
fi

asset="disksun-${tag}-${target}.tar.gz"
url="https://github.com/$repo/releases/download/${tag}/${asset}"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

printf 'Downloading %s\n' "$url"
curl -fsSL --progress-bar -o "$tmp/$asset" "$url"

printf 'Extracting...\n'
tar -C "$tmp" -xzf "$tmp/$asset"

extracted="$tmp/disksun-${tag}-${target}"
if [ ! -d "$extracted" ]; then
  printf 'error: expected %s after extract\n' "$extracted" >&2
  exit 1
fi

printf 'Running install.sh...\n\n'
sh "$extracted/install.sh"
