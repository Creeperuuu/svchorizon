#!/usr/bin/env python3
"""Extract the RNNoise float model arrays from xiph's rnnoise_data.c into a
compact binary the engine's Rust denoiser parses at start-up.

Source: xiph/rnnoise at 372f7b4b76cde4ca1ec4605353dd17898a99de38 (the commit
Simple Voice Chat's RNNoise JNI library builds), model data 0b50c45. The C
file declares `static const float name[N] = {...}` and
`static const int name[N] = {...}` arrays plus a WeightArray index; only the
arrays the official float compute path reads are kept.

Container: "RNDW" | u32 version=1 | u32 count | per array:
  u8 name_len | name | u8 dtype (0=f32 LE, 1=i32 LE) | u32 count | payload.
"""
from pathlib import Path
import re
import struct
import sys

SRC = Path(sys.argv[1]) if len(sys.argv) > 1 else Path(
    "/home/z/my-project/workspace/rnnoise-classic/src/rnnoise_data.c")
OUT = Path(sys.argv[2]) if len(sys.argv) > 2 else Path(
    "/home/z/my-project/workspace/svchorizon/engine/src/rnnoise/weights.bin")

KEEP = {
    "conv1_bias", "conv1_weights_float",
    "conv2_bias", "conv2_weights_float",
    "gru1_input_bias", "gru1_input_weights_float", "gru1_input_weights_idx",
    "gru1_recurrent_bias", "gru1_recurrent_weights_float",
    "gru1_recurrent_weights_idx", "gru1_recurrent_weights_diag",
    "gru2_input_bias", "gru2_input_weights_float", "gru2_input_weights_idx",
    "gru2_recurrent_bias", "gru2_recurrent_weights_float",
    "gru2_recurrent_weights_idx", "gru2_recurrent_weights_diag",
    "gru3_input_bias", "gru3_input_weights_float", "gru3_input_weights_idx",
    "gru3_recurrent_bias", "gru3_recurrent_weights_float",
    "gru3_recurrent_weights_idx", "gru3_recurrent_weights_diag",
    "dense_out_bias", "dense_out_weights_float",
    "vad_dense_bias", "vad_dense_weights_float",
}

ARRAY_RE = re.compile(
    r"static const (float|int)\s+([a-z0-9_]+)\s*\[[0-9]*\]\s*=\s*\{(.*?)\};",
    re.DOTALL,
)


def parse_numbers(text, dtype):
    text = re.sub(r"/\*.*?\*/", "", text, flags=re.DOTALL)
    text = re.sub(r"//[^\n]*", "", text)
    parts = [p.strip() for p in text.split(",")]
    parts = [p for p in parts if p]
    if dtype == "float":
        return [float(p.rstrip("f")) for p in parts]
    return [int(p, 0) for p in parts]


def main():
    text = SRC.read_text()
    arrays = {}
    for match in ARRAY_RE.finditer(text):
        dtype, name, body = match.group(1), match.group(2), match.group(3)
        if name not in KEEP:
            continue
        arrays[name] = (dtype, parse_numbers(body, dtype))
    missing = KEEP - set(arrays)
    if missing:
        raise SystemExit(f"missing arrays: {sorted(missing)}")
    extra = set(arrays) - KEEP
    if extra:
        raise SystemExit(f"unexpected arrays: {sorted(extra)}")

    out = bytearray()
    out += b"RNDW"
    out += struct.pack("<II", 1, len(arrays))
    for name in sorted(arrays):
        dtype, values = arrays[name]
        blob = name.encode()
        out.append(len(blob))
        out += blob
        out.append(0 if dtype == "float" else 1)
        out += struct.pack("<I", len(values))
        if dtype == "float":
            out += struct.pack(f"<{len(values)}f", *values)
        else:
            out += struct.pack(f"<{len(values)}i", *values)
    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_bytes(out)
    total_floats = sum(len(v) for _, v in arrays.values())
    print(f"wrote {OUT} ({len(out)} bytes, {len(arrays)} arrays, "
          f"{total_floats} values)")
    for name in sorted(arrays):
        dtype, values = arrays[name]
        kind = "i32" if dtype == "int" else "f32"
        print(f"  {name}: {kind}[{len(values)}]")


if __name__ == "__main__":
    main()
