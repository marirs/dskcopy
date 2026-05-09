# dskcopy

`dskcopy` is a Rust-based disk image writing and verification project with a modular workspace layout.

## Workspace crates

- `dskcopy` - main CLI + GUI binary entry point
- `crates/dskcopy-core` - image parsing, partition planning, hashing, verification
- `crates/dskcopy-platform` - platform backend traits and OS-specific modules
- `crates/dskcopy-ui` - Slint UI crate (3-tab stub layout)
- `crates/dskcopy-web` - Axum + Tera website server binary

## Features (scaffold)

- Shared core logic crate for disk/image operations
- Platform abstraction crate for backend implementations
- Slint 1.x GUI crate
- CLI mode via `clap`
- Separate web server crate for the project website

## Build

```bash
cargo build --release
```

## Usage

GUI mode (default, no args):

```bash
cargo run -p dskcopy
```

CLI mode (example):

```bash
cargo run -p dskcopy -- --device /dev/diskX --image ./image.img --verify
```

Run website server:

```bash
cargo run -p dskcopy-web
```