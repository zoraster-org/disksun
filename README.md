# disksun

interactive pie/sunburst disk-usage viewer for Linux.

![Screenshot of disksun scanning /](docs/screenshot.png)

A directory is drawn as a pie: every child is a wedge whose angle is
proportional to its share of the parent. Click a directory wedge to descend,
`h` / `Backspace` / the "Up" button to go back. Drag a wedge onto the trash
can (bottom-right) to move that file or folder to the Trash. The sidebar
lists the largest children; the top bar lets you rescan an arbitrary path,
jump to any mounted partition, or scan the whole disk as admin (root, via
sudo in a terminal).

Written in Rust with [`eframe` / `egui`](https://github.com/emilk/egui) —
runs on both Wayland and X11.

## Install

### One-liner (recommended)

```sh
curl -fsSL https://raw.githubusercontent.com/zoraster-org/disksun/main/packaging/get-disksun.sh | sh
```

Detects your CPU (x86_64 / aarch64), downloads the latest release
tarball, installs the binary to `~/.local/bin/disksun`, and wires up the
GUI menu entry. No Rust toolchain required.

### Debian / Ubuntu (.deb)

Grab the `.deb` for your architecture from the
[latest release](https://github.com/zoraster-org/disksun/releases/latest):

```sh
sudo apt install ./disksun_*_amd64.deb    # or ..._arm64.deb
```

Installs binary, `.desktop`, and icon system-wide. Uninstall with
`sudo apt remove disksun`.

### Fedora / RHEL (.rpm)

Grab the `.rpm` for your architecture from the
[latest release](https://github.com/zoraster-org/disksun/releases/latest):

```sh
sudo dnf install ./disksun-*.x86_64.rpm    # or ...aarch64.rpm
```

Uninstall with `sudo dnf remove disksun`.

### Prebuilt tarball (manual)

Download `disksun-<tag>-<target>.tar.gz` from the
[releases page](https://github.com/zoraster-org/disksun/releases), extract
it, then run the bundled installer:

```sh
tar xzf disksun-*.tar.gz
cd disksun-*/
./install.sh
```

Installs to `~/.local/bin/` and the XDG data dirs — no root.

### From source with cargo

```sh
cargo install --git https://github.com/zoraster-org/disksun
```

Drops `disksun` into `~/.cargo/bin`. Needs the Rust toolchain plus the
runtime libraries listed below. If you want a menu entry, follow
[GUI menu entry](#gui-menu-entry) below.

### Runtime dependencies

`eframe` dlopens the display-server and GL libraries at runtime. The
`.deb` / `.rpm` packages already pull these in — this table is only for
`cargo install`, the tarball, or building from source:

| Distro           | Package(s)                                                   |
| ---------------- | ------------------------------------------------------------ |
| Debian / Ubuntu  | `libwayland-client0 libxkbcommon0 libgl1 libx11-6`           |
| Fedora / RHEL    | `wayland-libs-client libxkbcommon mesa-libGL libX11`         |
| Arch             | `wayland libxkbcommon mesa libx11`                           |
| Alpine           | `wayland-libs-client libxkbcommon mesa-gl libx11`            |

For building from source, also install the matching `-dev` / `-devel`
packages plus `pkg-config`.

### NixOS

`eframe` won't find the system libs through Nix's glibc, so wrap the
binary with an explicit `LD_LIBRARY_PATH`:

```nix
disksun = pkgs.rustPlatform.buildRustPackage {
  pname = "disksun";
  version = "0.1.0";
  src = pkgs.fetchFromGitHub {
    owner = "zoraster-org";
    repo = "disksun";
    rev = "vX.Y.Z";
    hash = lib.fakeHash; # replace with real hash
  };
  cargoLock.lockFile = ./Cargo.lock; # or use cargoHash
  nativeBuildInputs = [ pkgs.makeWrapper ];
  postFixup = ''
    wrapProgram $out/bin/disksun \
      --prefix LD_LIBRARY_PATH : ${lib.makeLibraryPath [
        pkgs.wayland pkgs.libxkbcommon pkgs.libglvnd
      ]}
  '';
};
```

## Usage

```sh
disksun                    # GUI, scans $HOME
disksun /some/path         # GUI, scans /some/path
disksun --scan /some/path  # headless walker; prints the tree to stdout
disksun --scan --cross /   # ... crossing filesystem boundaries
```

The GUI's "Scan whole disk (root)" button reruns `disksun --scan --cross /`
under `sudo` in a terminal so it can read paths your user can't.

### Launcher

`contrib/disksun-launch.sh` is a tiny wrapper that runs disksun detached
from its parent (useful when binding it to a waybar/i3blocks/eww button so
a bar reload doesn't kill the GUI). Copy it into your `$PATH` or crib the
one line.

## GUI menu entry

If you use a desktop that shows a graphical app menu (GNOME, KDE, XFCE,
Cinnamon, wofi, rofi, `bemenu -x run` etc.), install the `.desktop` file
and icon so Disksun shows up alongside your other apps:

```sh
# From a git checkout or an extracted release tarball:
./contrib/install-desktop.sh
```

The script copies `contrib/disksun.desktop` to
`$XDG_DATA_HOME/applications/` and `contrib/disksun.svg` to
`$XDG_DATA_HOME/icons/hicolor/scalable/apps/` (both default to `~/.local/share/…`)
and refreshes the desktop/icon caches if the helpers are installed. Log
out and back in if the entry doesn't appear immediately.

If you installed via `cargo install --git …` and don't have the repo
locally, grab the three files first:

```sh
mkdir -p /tmp/disksun-contrib && cd /tmp/disksun-contrib
for f in disksun.desktop disksun.svg install-desktop.sh; do
  curl -fsSLO "https://raw.githubusercontent.com/zoraster-org/disksun/main/contrib/$f"
done
chmod +x install-desktop.sh && ./install-desktop.sh
```

## License

GPL-3.0-or-later — see [LICENSE](LICENSE).
