"""Write emission.json: torchaudio's MMS_FA emissions for synthetic waveforms.

usage: gen_emissions.py <out_dir>   (needs torch, torchaudio and the MMS_FA
checkpoint under $TORCH_HOME, as `packages.alignerWeights` builds with)

The waveforms are made by `wave()`, which tests/it_forward/main.rs repeats
in f64 so both sides feed the model identical samples.
"""

import json
import math
import sys
from pathlib import Path

import torch
import torchaudio

RATE = 16_000
# (samples, seed): one frame, a short line, a long line, a probe window. Under
# 400 samples torch raises; the Rust pass gives no frames, and both leave the
# line without word times.
WINDOWS = [(400, 1), (16_000, 2), (59_123, 3), (160_000, 4)]


def wave(n: int, seed: int) -> list[float]:
    """Three tones with a slow vibrato plus LCG noise, in f64, then f32."""
    state = seed
    out = []
    for i in range(n):
        state = (state * 6364136223846793005 + 1442695040888963407) % (1 << 64)
        noise = ((state >> 40) / float(1 << 24)) - 0.5
        t = i / RATE
        v = (
            0.3 * math.sin(2 * math.pi * (220 + 30 * seed) * t + 2 * math.sin(2 * math.pi * 5 * t))
            + 0.2 * math.sin(2 * math.pi * 660 * t)
            + 0.1 * math.sin(2 * math.pi * (1500 + 100 * seed) * t)
            + 0.05 * noise
        )
        out.append(v)
    return out


def main() -> None:
    model = torchaudio.pipelines.MMS_FA.get_model().eval()
    cases = []
    with torch.inference_mode():
        for n, seed in WINDOWS:
            w = torch.tensor(wave(n, seed), dtype=torch.float32).unsqueeze(0)
            out = model(w)
            emission = (out[0] if isinstance(out, tuple) else out)[0]
            cases.append({
                "samples": n,
                "seed": seed,
                "frames": emission.size(0),
                "emission": [float("%.6g" % x) for x in emission.flatten().tolist()],
            })
    Path(sys.argv[1], "emission.json").write_text(json.dumps({"cases": cases}, separators=(",", ":")) + "\n")


if __name__ == "__main__":
    main()
