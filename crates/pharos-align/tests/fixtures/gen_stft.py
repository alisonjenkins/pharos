"""Write stft.json: htdemucs's own spectrogram and inverse on synthetic input.

usage: gen_stft.py <out_dir>   (needs torch and demucs 4.1.0)

`signal` is a tone, a chirp and LCG noise; `spec` is `HTDemucs._spec` of it,
`[bin][frame][re, im]`. `inverse` is `HTDemucs._ispec` of `lcg_spectrum()`,
a spectrum no signal has, so the overlap-add and the window envelope are
checked apart from the forward transform. src/separate/stft.rs reads it and
repeats both generators, so only torch's outputs are stored.
"""

import json
import math
import sys
from pathlib import Path

import torch
from demucs.htdemucs import HTDemucs

RATE = 44_100
# Over the 2,560 samples the reflect padding needs, under 3 hops of 1,024 so
# the last frame is part padding.
LENGTH = 3_000


def lcg(state: int) -> tuple[int, float]:
    """The next state and a value in [-0.5, 0.5)."""
    state = (state * 6364136223846793005 + 1442695040888963407) % (1 << 64)
    return state, ((state >> 40) / float(1 << 24)) - 0.5


def signal() -> list[float]:
    state = 7
    out = []
    for i in range(LENGTH):
        state, noise = lcg(state)
        t = i / RATE
        out.append(
            0.4 * math.sin(2 * math.pi * 1_000 * t)
            + 0.2 * math.sin(2 * math.pi * (200 + 4_000 * t) * t)
            + 0.05 * noise
        )
    return out


def lcg_spectrum(bins: int, frames: int) -> torch.Tensor:
    """`[bin][frame]` complex, real then imaginary part from one LCG."""
    state = 11
    vals = []
    for _ in range(bins * frames * 2):
        state, v = lcg(state)
        vals.append(v)
    return torch.view_as_complex(torch.tensor(vals, dtype=torch.float32).view(bins, frames, 2))


def short(vals):
    """f32 values printed with the 9 digits that round-trip them."""
    return [float(f"{v:.9g}") for v in vals]


def main() -> None:
    out = Path(sys.argv[1])
    m = HTDemucs(sources=["drums", "bass", "other", "vocals"])
    assert (m.nfft, m.hop_length) == (4096, 1024)
    x = torch.tensor(signal(), dtype=torch.float32)
    with torch.no_grad():
        z = m._spec(x[None, None])[0, 0]
        r = lcg_spectrum(*z.shape)
        y = m._ispec(r[None, None], LENGTH)[0, 0]
    data = {
        "length": LENGTH,
        "bins": z.shape[0],
        "frames": z.shape[1],
        "spec": short(torch.view_as_real(z).flatten().tolist()),
        "inverse": short(y.tolist()),
    }
    (out / "stft.json").write_text(json.dumps(data, separators=(",", ":")))


if __name__ == "__main__":
    main()
