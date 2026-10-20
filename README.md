# MaomiTerminal

A cross-platform terminal app written in Rust on [GPUI](https://www.gpui.rs/) (the UI framework behind Zed). The terminal engine is [`alacritty_terminal`](https://crates.io/crates/alacritty_terminal) with [`portable-pty`](https://crates.io/crates/portable-pty) for shells.

> Status: early scaffold. The app opens an empty window.

## Layout

| Crate | Package | Purpose |
|---|---|---|
| `crates/app` | `maomi-app` (bin `maomiterminal`) | Window and app boot |
| `crates/ui` | `maomi-ui` | Theme (skins Aurora / Onyx, dark / light) and shared components |
| `crates/term` | `maomi-term` | PTY sessions and terminal emulation, no GPUI |
| `crates/config` | `maomi-config` | JSON settings in the platform config directory, no GPUI |

## Build and run

```sh
cargo run
```

The first build compiles GPUI and takes several minutes. Set `MAOMITERMINAL_LOG` (for example `debug`) to change the log filter.

## Checks

```sh
cargo check --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo deny check
```

## Licence

Apache-2.0, see [LICENSE](LICENSE) and [NOTICE](NOTICE). Third-party licences: `cargo about generate about.hbs > THIRD-PARTY.md`.

Zed's terminal crates are GPL-licensed and are reference-only here: no code is copied from them.
