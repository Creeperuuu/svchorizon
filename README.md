# SVC Horizon

Native Rust port of the Simple Voice Chat client for Horizon.

**Status: protocol-core prototype, not a working voice-chat mod.** This repository currently establishes a Rust workspace and an initial subset of the official UDP protocol. It does not yet intercept Minecraft TCP custom payloads, capture/play audio, implement Opus, integrate Horizon hooks, or produce an installable hnmod. Do not use it as a functional client.

## Goal

Interoperate with unmodified Paper/Spigot servers running the official Simple Voice Chat plugin and players using official Fabric, Forge, and NeoForge clients. No Horizon-specific server plugin.

## Architecture

1. Minecraft TCP custom payloads negotiate the voice server and provide the per-player secret.
2. A separate UDP socket authenticates and carries encrypted voice packets.
3. Opus handles microphone encoding and speaker decoding.
4. Horizon native source mixins connect the runtime, keybinds, settings, and HUD.

See [protocol notes](docs/PROTOCOL.md) and [implementation status](docs/STATUS.md).

## Build

Requires stable Rust and Cargo.

```sh
cargo test --workspace
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
```

The final deliverable is intended to be a source-based `.hnmod`, not a Java wrapper or modified game binary.

## References

- [Simple Voice Chat](https://github.com/henkelmax/simple-voice-chat)
- [Horizon](https://github.com/horizon-rust/horizon)
- [Horizon native mod documentation](https://github.com/horizon-rust/horizon/blob/main/docs/native-mods.md)
- [Horizon native mod template](https://github.com/horizon-rust/horizon-native-mod-template)

License: MIT.
