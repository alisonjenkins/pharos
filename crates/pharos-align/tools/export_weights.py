"""Write the MMS_FA acoustic model's weights as one f32 file and its index.

usage: export_weights.py <out_dir>

`weights.bin` is every tensor of the inner wav2vec2 model, f32 little-endian,
back to back; `weights.json` maps each name to `{"off": <f32 offset>,
"shape": [...]}`. The positional conv's weight norm is folded into its
effective weight; nothing else is changed or dropped. torch runs here, at
build time, so the service image has no Python.
"""

import json
import sys
from pathlib import Path

import torch
import torchaudio

# What the converted model must hold; a torchaudio that names or shapes the
# model differently fails the build instead of the service.
TENSORS = 422
FLOATS = 315_466_268
POS_CONV = "encoder.transformer.pos_conv_embed.conv.weight"


def main() -> None:
    out = Path(sys.argv[1])
    out.mkdir(parents=True, exist_ok=True)
    model = torchaudio.pipelines.MMS_FA.get_model().eval()
    inner = getattr(model, "model", model)
    with torch.no_grad():
        state = {
            k: v
            for k, v in inner.state_dict().items()
            if not k.startswith(POS_CONV) and "parametrizations" not in k
        }
        state[POS_CONV] = inner.encoder.transformer.pos_conv_embed.conv.weight.detach()
        index = {}
        off = 0
        with open(out / "weights.bin", "wb") as f:
            for name, tensor in state.items():
                a = tensor.detach().to(torch.float32).contiguous().numpy()
                if a.dtype.byteorder == ">":
                    a = a.byteswap()
                f.write(a.tobytes())
                index[name] = {"off": off, "shape": list(a.shape)}
                off += a.size
    (out / "weights.json").write_text(json.dumps(index, indent=0) + "\n")
    if len(index) != TENSORS or off != FLOATS:
        sys.exit(f"expected {TENSORS} tensors and {FLOATS} floats, wrote {len(index)} and {off}")
    print(f"tensors={len(index)} floats={off}")


if __name__ == "__main__":
    main()
