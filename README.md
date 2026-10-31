# SVC Horizon

A native Rust Simple Voice Chat client port for Horizon.

<sub>Development status: advanced prototype; not yet validated in-game or against an official server.</sub>

The project targets unmodified Paper/Spigot servers running the official Simple Voice Chat plugin and players using official Fabric, Forge and NeoForge clients. No Horizon-specific server plugin is intended.

## Current implementation

- Official TCP request-secret / secret handshake hooks.
- AES-128-GCM encrypted UDP transport and authentication.
- Vendored pure-Rust Opus codec.
- CPAL microphone capture and speaker playback.
- Push-to-talk (V), hold-to-whisper (B), toggleable RMS voice activation (C), and microphone mute (M, synchronized to the server's client-state packet).
- Concurrent per-speaker mixing, bounded sequence reordering, and short-burst Opus packet-loss concealment.
- Per-player volume/mute and microphone-gain engine controls, plus basic player/location spatial audio.
- Native group controls: create a public `Horizon` group (G), cycle discovered groups (N), join an unlocked selected group (J), and leave (H).
- Official player-state tracking with player selection (P), local ignore/mute (O), and per-player volume adjustment (Page Up/Page Down).
- Connection and voice-activation HUD indicators, plus exponential-backoff UDP reconnect attempts while the Minecraft server session remains active.

The implementation is not yet complete: password entry and group customization, persistent player controls, remaining TCP payloads, device selection/settings UI, server-transfer edge cases and end-to-end interoperability testing are outstanding. Player controls are currently keybind-driven rather than a full player-list screen.

## Build and validate

The GitHub Actions workflows are manual-trigger only. Open **Actions → Build SVC Horizon hnmod → Run workflow** to run Windows/Linux protocol checks, compile the native engine for Windows, validate the pinned Horizon native mixins on Linux, and package the source bundle.

The workflow artifact is `svchorizon-0.1.0.hnmod`. It is a source-based native mod bundle, not a Java mod or a modified Minecraft executable.

Local protocol checks:

```sh
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
```

## Pinned compatibility

- Minecraft: 26.3
- Horizon: `ba6344a59cb9e755b730dcf02285fc0d33b97d3c`
- horizon-mixins: `d9c83d7096b7ea5b83678da2efdfd6161f74997e`
- Simple Voice Chat compatibility version: 20 (upstream 2.6.27+26.3)

See [protocol notes](docs/PROTOCOL.md) and [implementation status](docs/STATUS.md).

## Upstream references

- [Simple Voice Chat](https://github.com/henkelmax/simple-voice-chat)
- [Horizon](https://github.com/horizon-rust/horizon)
- [Horizon native mod template](https://github.com/horizon-rust/horizon-native-mod-template)
- [Pure-Rust Opus implementation](https://github.com/restsend/opus-rs)

License: MIT for project-authored files; the vendored Opus codec retains its BSD-3-Clause license.
