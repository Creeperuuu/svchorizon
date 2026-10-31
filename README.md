# SVC Horizon

A native Rust Simple Voice Chat client port for Horizon.

<sub>Development status: advanced prototype; not yet validated in-game or against an official server.</sub>

The project targets unmodified Paper/Spigot servers running the official Simple Voice Chat plugin and players using official Fabric, Forge and NeoForge clients. No Horizon-specific server plugin is intended.

## Current implementation

- Official TCP request-secret / secret handshake hooks.
- AES-128-GCM encrypted UDP transport and authentication.
- Vendored pure-Rust Opus codec.
- CPAL microphone capture and speaker playback.
- Push-to-talk (V), hold-to-whisper (B), toggleable RMS voice activation (C), and microphone mute (M).
- Concurrent per-speaker mixing, bounded sequence reordering, and short-burst Opus packet-loss concealment.
- Per-player volume/mute and microphone-gain engine controls, plus basic player/location spatial audio.
- Official group create/join/leave payload codecs and group response decoders (not yet wired to in-game controls).
- Connection and voice-activation HUD indicators.

The implementation is not yet complete: group management, remaining TCP payloads, device selection/settings UI, reconnect edge cases and end-to-end interoperability testing are outstanding. Per-player volume/mute setters are not yet exposed through an in-game player menu.

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
- Simple Voice Chat compatibility version: 20 (upstream 2.6.28+26.3)

See [protocol notes](docs/PROTOCOL.md) and [implementation status](docs/STATUS.md).

## Upstream references

- [Simple Voice Chat](https://github.com/henkelmax/simple-voice-chat)
- [Horizon](https://github.com/horizon-rust/horizon)
- [Horizon native mod template](https://github.com/horizon-rust/horizon-native-mod-template)
- [Pure-Rust Opus implementation](https://github.com/restsend/opus-rs)

License: MIT for project-authored files; the vendored Opus codec retains its BSD-3-Clause license.
