# GPUI G1 spike

This is an isolated binary. It does not belong to the Arca workspace and does
not change `arca-gui`: egui remains the production UI.

Pinned source: `zed-industries/zed@3384317a9931a21bb5ad8706f0f9d82cb02a71ec`.
The pin is used for both `gpui` and `gpui_platform`. `rust-toolchain.toml`
requires Rust `1.97.1` for a clean reproduction.

## Checks

```text
rustup run 1.97.1 cargo check --manifest-path spikes/gpui/Cargo.toml
rustup run 1.97.1 cargo test --manifest-path spikes/gpui/Cargo.toml
rustup run 1.97.1 cargo run --manifest-path spikes/gpui/Cargo.toml
```

The Linux manifest enables `wayland` and `x11` only in the Linux target
section. Windows has no Linux backend features; macOS enables `font-kit` only.

The window intentionally exercises only seams needed before a migration:
`uniform_list` with all 6,000 fixture rows, Ctrl/Shift selection and cursor
reveal, real per-cell resizable columns, an `EntityInputHandler` filter field
with UTF-16 IME ranges and caret geometry, an AccessKit-labelled list, a
modal that removes the background from the focus/action tree, a synthetic RGBA
PNG, and a GPUI file-drop listener. The optional Windows feature
`platform-probes` type-checks the existing `rfd`, `clipboard-win`, and
`arca-drag` seams without calling them:

```text
cargo check --locked --features platform-probes --manifest-path spikes/gpui/Cargo.toml
```

The probes do not extract data or open dialogs. `arca-drag` remains outside
this window because starting OLE drag-and-drop is a blocking user interaction.
