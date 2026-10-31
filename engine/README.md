# Bundled native voice engine

This folder contains the source for the native `svchorizon-engine` crate. The build staging script copies `src/`, `tests/`, and `COPYING` into `mixins/svchorizon/engine/` before native-mixin validation and packaging.

## Opus codec provenance

The Opus encoder/decoder implementation is vendored from:

- Repository: https://github.com/restsend/opus-rs
- License: BSD-3-Clause (see [COPYING](COPYING))

The SVC UDP transport, AES-GCM framing, CPAL device I/O, resampling, and spatial playback glue are project-authored Rust code.

Do not remove the upstream license notice when updating the vendored codec.
