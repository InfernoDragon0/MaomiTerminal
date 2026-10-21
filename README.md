# MaomiTerminal

A cross-platform terminal app written in Rust on [GPUI](https://www.gpui.rs/) (the UI framework behind Zed). The terminal engine is [`alacritty_terminal`](https://crates.io/crates/alacritty_terminal) with [`portable-pty`](https://crates.io/crates/portable-pty) for shells.

> Status: pre-release. Local shells in tabs and split panes, multiple windows, settings.

## Install

Download the installer for your OS from [Releases](https://github.com/InfernoDragon0/MaomiTerminal/releases). The app updates itself: it downloads new releases in the background and installs them when you quit (Settings > Updates turns this off).

| OS | File | Notes |
|---|---|---|
| Windows | `MaomiTerminal-win-Setup.exe` | Installs for the current user, no admin needed. |
| macOS | `MaomiTerminal-osx-Setup.pkg` | Universal (Apple silicon and Intel). |
| Linux | `MaomiTerminal.AppImage` | `chmod +x` it and run. |

Builds are not code-signed, so the OS warns on first launch:

- **Windows SmartScreen:** click **More info**, then **Run anyway**. Updates install without the prompt.
- **macOS Gatekeeper:** open the app once, then go to System Settings > Privacy & Security and click **Open Anyway**.

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

## Releasing

The version lives only in the root `Cargo.toml` (`[workspace.package] version`).

1. Bump the version and commit.
2. Push a tag `v<version>` (for example `v0.2.0`). The tag must match `Cargo.toml` or the workflow stops.
3. `.github/workflows/release.yml` builds Windows, macOS (universal) and Linux, packs each with [Velopack](https://velopack.io), and publishes one GitHub release. Installed apps pick it up as an update.
4. Optional: update `packaging/winget/` (version, URL, `winget hash` of the Setup.exe) and open a PR on `microsoft/winget-pkgs`.

The `velopack` crate in `Cargo.toml` and `VPK_VERSION` in `release.yml` must stay on the same version.

## Licence

Apache-2.0, see [LICENSE](LICENSE) and [NOTICE](NOTICE). Third-party licences: `cargo about generate about.hbs > THIRD-PARTY.md`.

Zed's terminal crates are GPL-licensed and are reference-only here: no code is copied from them.
