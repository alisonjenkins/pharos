"""Regenerate the parity fixtures from the Python aligner and torchaudio.

Run with the aligner service's Python environment (torch + torchaudio 2.11.0,
the versions the pharos-aligner image pins), the repo's aligner/ on sys.path:

    PYTHONPATH=aligner python crates/pharos-align/tests/fixtures/gen_fixtures.py crates/pharos-align/tests/fixtures

All emissions are synthetic: seeded random grids, peaky blank-dominant frames
and random log-softmax rows. Nothing here comes from real audio.
"""

from __future__ import annotations

import json
import random
import re
import sys
from pathlib import Path

import torch
import torchaudio
import torchaudio.functional as F

from pharos_aligner.model import Aligner
from pharos_aligner.text import words

OUT = Path(sys.argv[1])
GRID = 64  # grid emissions are multiples of -1/64: exact in f32, so ties are real ties


def text_fixture() -> dict:
    import unicodedata

    spaces = []
    single = {}
    # Code points this Python's Unicode does not assign: newer Unicode in the
    # Rust crate may fold them, and no lyric written today can contain them.
    unassigned: list[list[int]] = []
    for cp in range(0x110000):
        if unicodedata.category(chr(cp)) == "Cn":
            if unassigned and unassigned[-1][1] == cp - 1:
                unassigned[-1][1] = cp
            else:
                unassigned.append([cp, cp])
    for cp in range(0x110000):
        if 0xD800 <= cp <= 0xDFFF:
            continue
        c = chr(cp)
        if re.fullmatch(r"\s", c):
            spaces.append(cp)
        ws = words(c)
        if ws:
            single[str(cp)] = ws[0].token
    strings = [
        "plain words here",
        "  leading and trailing  ",
        "tab\tnew\nline\r\nmixed\u000bvt\u000cff",
        "no break em　ideo ls\u0085nel",
        "sep\u001cfs\u001dgs\u001ers\u001fus",
        "don’t can't rock‘n’roll",
        "café naïve Ångström ıstanbul İzmir",
        "ﬁne ﬂow ① Ⅸ ½ ² ㎡",
        "ＡＢＣ fullwidth",
        "123 456 -- !! ?? ... --",
        "emoji \U0001f3b5 la\U0001f3b5la",
        "一二三 あい 한글 абв αβγ",
        "straße ẞ Æsir øre œuvre þ",
        "zero​width joiner‍here soft­hyphen",
        "combining é ä ñ alone ́̈",
        "MiXeD CaSe ALL CAPS",
        "o'clock 'quoted' ''",
        "",
        "   ",
    ]
    return {
        "unicode_version": unicodedata.unidata_version,
        "unassigned": unassigned,
        "spaces": spaces,
        "single": single,
        "strings": [{"text": s, "words": [[w.at, w.token] for w in words(s)]} for s in strings],
    }


def align_case(name: str, emission: torch.Tensor, words_tok: list[list[int]], grid: bool) -> dict:
    flat = [t for w in words_tok for t in w]
    case: dict = {"name": name, "frames": emission.size(0), "classes": emission.size(1), "words": words_tok}
    if grid:
        case["grid"] = [int(round(-v * GRID)) for v in emission.flatten().tolist()]
    else:
        # 9 significant digits round-trip any f32 exactly.
        case["emission"] = [float(f"{v:.9g}") for v in emission.flatten().tolist()]
    try:
        path, scores = F.forced_align(emission.unsqueeze(0), torch.tensor([flat], dtype=torch.int32), blank=0)
    except RuntimeError as e:
        case["error"] = str(e).splitlines()[0]
        return case
    case["path"] = path[0].tolist()
    case["log_scores"] = scores[0].tolist()
    spans = F.merge_tokens(path[0], scores[0].exp())
    assert len(spans) == len(flat)
    i = 0
    out = []
    for w in words_tok:
        out.append([[s.token, s.start, s.end, s.score] for s in spans[i : i + len(w)]])
        i += len(w)
    case["spans"] = out
    return case


def grid_emission(rng: random.Random, frames: int, classes: int, hi: int) -> torch.Tensor:
    vals = [-rng.randint(0, hi) / GRID for _ in range(frames * classes)]
    return torch.tensor(vals, dtype=torch.float32).reshape(frames, classes)


def peaky_emission(rng: random.Random, frames: int, classes: int, flat: list[int]) -> torch.Tensor:
    # Mostly blank, each target lit for a short run in order, like a trained CTC head.
    e = torch.full((frames, classes), -10.0)
    for t in range(frames):
        e[t, 0] = -rng.randint(0, 8) / GRID
        for c in range(1, classes):
            e[t, c] = -rng.randint(256, 900) / GRID
    pos = sorted(rng.sample(range(frames), min(len(flat), frames)))
    for t, tok in zip(pos, flat):
        for dt in range(rng.randint(1, 3)):
            if t + dt < frames:
                e[t + dt, tok] = -rng.randint(0, 6) / GRID
                e[t + dt, 0] = -rng.randint(64, 400) / GRID
    return e


def random_words(rng: random.Random, n_words: int, classes: int, repeat_p: float) -> list[list[int]]:
    out = []
    prev = None
    for _ in range(n_words):
        w = []
        for _ in range(rng.randint(1, 5)):
            tok = prev if prev is not None and rng.random() < repeat_p else rng.randint(1, classes - 1)
            w.append(tok)
            prev = tok
        out.append(w)
    return out


def ctc_fixture() -> list[dict]:
    rng = random.Random(1166)
    cases = []
    # Hand-shaped boundaries: T against L + R (R = repeats in the flattened targets).
    for name, words_tok, frames in [
        ("single_token_one_frame", [[1]], 1),
        ("single_token_two_frames", [[1]], 2),
        ("exact_l_no_repeats", [[1, 2], [3]], 3),
        ("t_equals_l_plus_r", [[1, 1], [2]], 4),
        ("t_below_l_plus_r", [[1, 1], [2]], 3),
        ("repeat_across_word_boundary", [[1, 2], [2, 3]], 6),
        ("repeat_across_boundary_tight", [[1, 2], [2, 3]], 5),
        ("all_same_token", [[4, 4, 4], [4]], 8),
        ("all_same_token_tight", [[4, 4, 4], [4]], 7),
    ]:
        cases.append(align_case(name, grid_emission(rng, frames, 8, 320), words_tok, grid=True))
    # Coarse grids: many exact ties, exercising the three-way branch and the final pick.
    for i in range(14):
        w = random_words(rng, rng.randint(1, 4), 6, 0.3)
        flat = sum(len(x) for x in w)
        frames = flat + rng.randint(0, 2 * flat + 4)
        cases.append(align_case(f"tie_grid_{i}", grid_emission(rng, frames, 6, rng.choice([1, 2, 3])), w, grid=True))
    # Wide grids.
    for i in range(10):
        w = random_words(rng, rng.randint(1, 6), 8, 0.2)
        flat = sum(len(x) for x in w)
        frames = flat + rng.randint(0, 3 * flat + 10)
        cases.append(align_case(f"grid_{i}", grid_emission(rng, frames, 8, 640), w, grid=True))
    # Peaky, blank-dominant.
    for i in range(8):
        w = random_words(rng, rng.randint(2, 7), 10, 0.15)
        flat_tok = [t for x in w for t in x]
        frames = 3 * len(flat_tok) + rng.randint(5, 60)
        cases.append(align_case(f"peaky_{i}", peaky_emission(rng, frames, 10, flat_tok), w, grid=True))
    # Off-grid f32 log-softmax rows: arithmetic must round the same way as torchaudio's f32 loop.
    gen = torch.Generator().manual_seed(1166)
    for i in range(5):
        w = random_words(rng, rng.randint(1, 5), 29, 0.1)
        flat = sum(len(x) for x in w)
        frames = flat + rng.randint(0, 2 * flat + 8)
        e = torch.log_softmax(torch.randn(frames, 29, generator=gen) * 3, dim=-1)
        cases.append(align_case(f"logsoftmax_{i}", e, w, grid=False))
    return cases


class FixedModel:
    def __init__(self, emission: torch.Tensor) -> None:
        self.emission = emission

    def __call__(self, waveform: torch.Tensor):
        return (self.emission.unsqueeze(0), None)


def line_fixture() -> list[dict]:
    """End to end through model.py's Aligner, the model swapped for a fixed emission."""
    rng = random.Random(1165)
    bundle = torchaudio.pipelines.MMS_FA
    letters = "abcdefghijklmnopqrstuvwxyz'"
    cases = []
    for i in range(16):
        tokens = ["".join(rng.choice(letters) for _ in range(rng.randint(1, 6))) for _ in range(rng.randint(1, 4))]
        chars = sum(len(t) for t in tokens) + 1
        frames = max(1, chars + rng.randint(-2, 2 * chars))
        if i == 13:
            tokens, frames = ["ll", "l"], 5  # passes model.py's frame check, fails CTC's repeat bound
        if i == 14:
            tokens, frames = ["abc", "de"], 4  # fewer frames than characters: no alignment
        if i == 15:
            tokens, frames = ["ll", "l"], 7  # with the lead star, exactly enough frames for the repeats
        n_samples = frames * 320 + rng.randint(-319, 319)
        lead_star = i % 3 == 0
        e = grid_emission(rng, frames, 28, 900)
        e = torch.cat([e, torch.zeros(frames, 1)], dim=-1)
        a = Aligner()
        a._model, a._tokenizer, a._aligner = FixedModel(e), bundle.get_tokenizer(), bundle.get_aligner()
        case = {"tokens": tokens, "lead_star": lead_star, "n_samples": n_samples, "frames": frames}
        case["grid"] = [int(round(-v * GRID)) for v in e.flatten().tolist()]
        try:
            case["ms"] = a._align_loaded(b"\0" * (4 * n_samples), tokens, lead_star)
        except (RuntimeError, ValueError) as err:
            case["error"] = str(err).splitlines()[0]
        cases.append(case)
    return cases


def ms_fixture() -> list[list[int]]:
    rng = random.Random(1164)
    out = []
    for _ in range(400):
        frames = rng.randint(1, 4000)
        n = frames * 320 + rng.randint(-319, 400)
        s = rng.randint(0, frames)
        e = rng.randint(s, frames)
        ratio = (n / frames / 16000.0) * 1000.0
        out.append([n, frames, s, e, int(s * ratio), int(e * ratio)])
    return out


def main() -> None:
    OUT.mkdir(parents=True, exist_ok=True)
    meta = {"torch": torch.__version__, "torchaudio": torchaudio.__version__, "dictionary": torchaudio.pipelines.MMS_FA.get_dict()}
    (OUT / "text.json").write_text(json.dumps(text_fixture(), ensure_ascii=True, separators=(",", ":")) + "\n")
    (OUT / "ctc.json").write_text(json.dumps({**meta, "grid": GRID, "cases": ctc_fixture()}, separators=(",", ":")) + "\n")
    (OUT / "line.json").write_text(json.dumps({**meta, "grid": GRID, "cases": line_fixture(), "ms": ms_fixture()}, separators=(",", ":")) + "\n")


if __name__ == "__main__":
    main()
