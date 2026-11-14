"""Rewrites a burnpack (.bpk) without its tensors' param ids.

onnx2burn gives each tensor a random param id, so two conversions of the same
network differ and the flake's output is not reproducible. The ids only match
training state; when one is missing, burn-store keeps the loading module's own
id. The tensor data is copied as is.

usage: strip_param_ids.py IN.bpk OUT.bpk
"""

import struct
import sys

import cbor2

MAGIC = 0x4255524E
HEADER = struct.Struct("<IHI")
ALIGN = 256


def data_start(metadata_len: int) -> int:
    return -(-(HEADER.size + metadata_len) // ALIGN) * ALIGN


def main() -> None:
    src, dst = sys.argv[1:]
    with open(src, "rb") as f:
        raw = f.read()
    magic, version, size = HEADER.unpack_from(raw)
    if magic != MAGIC:
        sys.exit(f"{src}: not a burnpack (magic {magic:#x})")
    metadata = cbor2.loads(raw[HEADER.size : HEADER.size + size])
    if cbor2.dumps(metadata) != raw[HEADER.size : HEADER.size + size]:
        sys.exit(f"{src}: metadata does not re-encode to the same bytes")
    for desc in metadata["tensors"].values():
        desc.pop("param_id", None)
    out = cbor2.dumps(metadata)
    data = raw[data_start(size) :]
    with open(dst, "wb") as f:
        f.write(HEADER.pack(magic, version, len(out)))
        f.write(out)
        f.write(b"\0" * (data_start(len(out)) - HEADER.size - len(out)))
        f.write(data)


if __name__ == "__main__":
    main()
