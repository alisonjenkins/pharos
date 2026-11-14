"""Export htdemucs's network, without its STFT and ISTFT, as ONNX for Burn.

usage: export_separator.py <checkpoint.th> <out_dir>

Writes `<out_dir>/htdemucs.onnx`: inputs `mix` [1, 2, L] (the time branch)
and `mag` [1, 4, 2048, 336] (the spectrogram, real and imaginary parts as
channels), outputs `x` [1, 4, 4, 2048, 336] and `xt` [1, 4, 2, L], L being
the model's 7.8 s segment at 44.1 kHz. pharos-align does the STFT, the
ISTFT and the sum `xt + istft(x)` itself.

The graph is put through onnxruntime's basic optimisations, which fold the
shape arithmetic burn-onnx cannot type, and the build fails if the
optimised graph strays from torch. torch runs here, at build time, so the
service image has no Python.
"""

import sys
from pathlib import Path

import numpy as np
import onnxruntime as ort
import torch
from demucs.states import load_model

SOURCES = ["drums", "bass", "other", "vocals"]
# Largest difference allowed between torch and the optimised graph on noise
# whose outputs peak near 0.3; the spike measured 6.6e-5.
TOLERANCE = 1e-3


class Core(torch.nn.Module):
    """HTDemucs.forward from the normalisation to the de-normalisation."""

    def __init__(self, m):
        super().__init__()
        self.m = m

    def forward(self, mix, mag):
        self_ = self.m
        x = mag
        B, C, Fq, T = x.shape
        mean = x.mean(dim=(1, 2, 3), keepdim=True)
        std = x.std(dim=(1, 2, 3), keepdim=True)
        x = (x - mean) / (1e-5 + std)
        xt = mix
        meant = xt.mean(dim=(1, 2), keepdim=True)
        stdt = xt.std(dim=(1, 2), keepdim=True)
        xt = (xt - meant) / (1e-5 + stdt)
        saved, saved_t, lengths, lengths_t = [], [], [], []
        for idx, encode in enumerate(self_.encoder):
            lengths.append(x.shape[-1])
            inject = None
            if idx < len(self_.tencoder):
                lengths_t.append(xt.shape[-1])
                tenc = self_.tencoder[idx]
                xt = tenc(xt)
                if not tenc.empty:
                    saved_t.append(xt)
                else:
                    inject = xt
            x = encode(x, inject)
            if idx == 0 and self_.freq_emb is not None:
                frs = torch.arange(x.shape[-2], device=x.device)
                emb = self_.freq_emb(frs).t()[None, :, :, None].expand_as(x)
                x = x + self_.freq_emb_scale * emb
            saved.append(x)
        if self_.crosstransformer:
            if self_.bottom_channels:
                b, c, f, t = x.shape
                x = x.reshape(b, c, f * t)
                x = self_.channel_upsampler(x)
                x = x.reshape(b, -1, f, t)
                xt = self_.channel_upsampler_t(xt)
            x, xt = self_.crosstransformer(x, xt)
            if self_.bottom_channels:
                x = x.reshape(b, -1, f * t)
                x = self_.channel_downsampler(x)
                x = x.reshape(b, -1, f, t)
                xt = self_.channel_downsampler_t(xt)
        for idx, decode in enumerate(self_.decoder):
            skip = saved.pop(-1)
            x, pre = decode(x, skip, lengths.pop(-1))
            offset = self_.depth - len(self_.tdecoder)
            if idx >= offset:
                tdec = self_.tdecoder[idx - offset]
                length_t = lengths_t.pop(-1)
                if tdec.empty:
                    pre = pre[:, :, 0]
                    xt, _ = tdec(pre, None, length_t)
                else:
                    skip = saved_t.pop(-1)
                    xt, _ = tdec(xt, skip, length_t)
        S = len(self_.sources)
        x = x.view(B, S, -1, Fq, T) * std[:, None] + mean[:, None]
        xt = xt.view(B, S, -1, mix.shape[-1]) * stdt[:, None] + meant[:, None]
        return x, xt


def main() -> None:
    checkpoint, out = Path(sys.argv[1]), Path(sys.argv[2])
    out.mkdir(parents=True, exist_ok=True)
    m = load_model(checkpoint).eval()
    # What pharos-align's STFT, segmenting and source pick assume.
    assert type(m).__name__ == "HTDemucs", type(m)
    assert list(m.sources) == SOURCES, m.sources
    assert (m.samplerate, m.audio_channels, m.nfft, m.hop_length) == (44100, 2, 4096, 1024)
    assert m.cac and m.use_train_segment
    seg = int(m.segment * m.samplerate)
    assert seg == 343_980, seg

    torch.manual_seed(0)
    mix = torch.randn(1, 2, seg) * 0.1
    core = Core(m).eval()
    with torch.no_grad():
        full = m(mix)
        mag = m._magnitude(m._spec(mix))
        x, xt = core(mix, mag)
        ours = xt + m._ispec(m._mask(m._spec(mix), x), seg)
    drift = (full - ours).abs().max().item()
    assert drift < TOLERANCE, f"core strays from HTDemucs.forward by {drift}"

    raw = out / "htdemucs-raw.onnx"
    torch.onnx.export(
        core, (mix, mag), str(raw),
        input_names=["mix", "mag"], output_names=["x", "xt"],
        opset_version=17, dynamo=False,
    )
    so = ort.SessionOptions()
    so.graph_optimization_level = ort.GraphOptimizationLevel.ORT_ENABLE_BASIC
    so.optimized_model_filepath = str(out / "htdemucs.onnx")
    ort.InferenceSession(str(raw), so, providers=["CPUExecutionProvider"])
    raw.unlink()

    sess = ort.InferenceSession(str(out / "htdemucs.onnx"), providers=["CPUExecutionProvider"])
    gx, gxt = sess.run(None, {"mix": mix.numpy(), "mag": mag.numpy()})
    for name, got, want in (("x", gx, x.numpy()), ("xt", gxt, xt.numpy())):
        d = float(np.abs(got - want).max())
        assert d < TOLERANCE, f"optimised graph's {name} strays from torch by {d}"
        print(f"{name}: max abs difference from torch {d:.2e}")


if __name__ == "__main__":
    main()
