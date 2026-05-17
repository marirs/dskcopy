# dskcopy

`dskcopy` is a Rust-based disk image writer and verifier for removable drives.

The GUI is intentionally focused: choose or drag in an existing disk image, select a target drive, then flash it. It does not download operating system installers.

## Workspace crates

- `dskcopy` - main CLI + GUI binary entry point
- `crates/dskcopy-core` - image parsing, partition planning, hashing, verification
- `crates/dskcopy-platform` - platform backend traits and OS-specific modules
- `crates/dskcopy-ui` - Slint GUI crate
- `crates/dskcopy-web` - Axum + Tera website server binary

## Features

- Drag-and-drop or file-picker image selection in the GUI
- Raw image flashing with progress reporting
- Verification support
- Removable-drive detection through platform backends
- CLI mode via `clap`
- Supported source formats include ISO, IMG, DMG, RAW, GZ, XZ, ZIP, BZ2, VHD, VHDX and FFU

## Build

```bash
cargo build --release
```

## Usage

The `dskcopy` package ships two binaries:

- `dskcopy-cli` – command-line interface (no GUI deps)
- `dskcopy-gui` – Slint desktop app (built only with `--features gui`)

GUI:

```bash
cargo run -p dskcopy --features gui --bin dskcopy-gui
```

CLI:

```bash
cargo run -p dskcopy --bin dskcopy-cli -- flash ./image.iso /dev/diskX
cargo run -p dskcopy --bin dskcopy-cli -- list-drives
```

## Distribution

App icon sources live in `assets/`:

- `assets/dskcopy-icon.svg` – master vector icon
- `assets/dskcopy.icns` – generated macOS icon
- `assets/icon_256.png` – PNG used by the Linux `.desktop` entry
- `assets/dskcopy-gui.desktop` – Linux desktop entry

Regenerate the macOS icns + 256 px PNG with:

```bash
bash scripts/make-icns.sh
```

### macOS `.app`

```bash
cargo install cargo-bundle
cargo bundle --release -p dskcopy --features gui --bin dskcopy-gui
open target/release/bundle/osx/dskcopy.app
```

The Dock icon only appears when launching the `.app` bundle – running `target/release/dskcopy-gui` directly will show the generic macOS executable icon.

### Releases

Tagging `v*` triggers `.github/workflows/release.yml`, which builds CLI + GUI artifacts for macOS (Apple Silicon + Intel), Windows (x86_64 + ARM64) and Linux (x86_64; CLI also aarch64), produces `.app.tar.gz` bundles on macOS, and publishes them to a GitHub Release with SHA-256 sidecars.

## Website

The marketing site lives in `crates/dskcopy-web` and is served by an Axum + Tera Rust binary:

```bash
cargo run -p dskcopy-web
# http://0.0.0.0:3000
```

Environment overrides: `DSKCOPY_WEB_BIND`, `DSKCOPY_REPO`, `DSKCOPY_VERSION`.