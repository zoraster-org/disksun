# disksun

interactive pie disk-usage viewer for Linux. Rust + `eframe`, Wayland & X11.

![Screenshot](docs/screenshot.png)

Click a wedge to descend, `h`/`Backspace` to go back, drag a wedge onto the trash to delete.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/zoraster-org/disksun/main/packaging/get-disksun.sh | sh
```

Or grab a `.deb` / `.rpm` / tarball from the [latest release](https://github.com/zoraster-org/disksun/releases/latest):

```sh
sudo apt install ./disksun_*_amd64.deb    # Debian / Ubuntu
sudo dnf install ./disksun-*.x86_64.rpm   # Fedora / RHEL
```

From source: `cargo install --git https://github.com/zoraster-org/disksun` (needs Rust + `libwayland-dev libxkbcommon-dev libgl-dev libx11-dev` or your distro's equivalent).

## Usage

```sh
disksun                    # scan $HOME
disksun /some/path         # scan a path
disksun --scan [--cross] / # headless, prints tree to stdout
```

## NixOS

```nix
disksun = pkgs.rustPlatform.buildRustPackage {
  pname = "disksun";
  version = "0.1.0";
  src = pkgs.fetchFromGitHub {
    owner = "zoraster-org"; repo = "disksun";
    rev = "vX.Y.Z"; hash = lib.fakeHash;
  };
  cargoHash = lib.fakeHash;
  nativeBuildInputs = [ pkgs.makeWrapper ];
  postFixup = ''
    wrapProgram $out/bin/disksun --prefix LD_LIBRARY_PATH : ${lib.makeLibraryPath [
      pkgs.wayland pkgs.libxkbcommon pkgs.libglvnd
    ]}
  '';
};
```

## License

GPL-3.0-or-later — see [LICENSE](LICENSE).
