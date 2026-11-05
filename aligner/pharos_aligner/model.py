"""Forced alignment model loading and inference with PyTorch and torchaudio."""

from __future__ import annotations

from typing import Any
import torch
import torchaudio


class Aligner:
    """Lazy-loaded multilingual forced aligner using torchaudio MMS_FA."""

    def __init__(self) -> None:
        self._model: Any = None
        self._tokenizer: Any = None
        self._aligner: Any = None

    def _ensure_loaded(self) -> None:
        if self._model is not None:
            return
        bundle = torchaudio.pipelines.MMS_FA
        model = bundle.get_model()
        model.eval()
        self._model = model
        self._tokenizer = bundle.get_tokenizer()
        self._aligner = bundle.get_aligner()

    def align(self, pcm_f32le: bytes, tokens: list[str]) -> list[tuple[int, int]]:
        """Align tokens against 16kHz float32 PCM audio, returning (start_ms, end_ms) per word."""
        if not pcm_f32le or not tokens:
            return []

        self._ensure_loaded()

        waveform = torch.frombuffer(bytearray(pcm_f32le), dtype=torch.float32).unsqueeze(0)
        n_samples = waveform.size(1)
        if n_samples == 0:
            return []

        with torch.inference_mode():
            out = self._model(waveform)
            emission = out[0] if isinstance(out, tuple) else out
            num_frames = emission.size(1)
            if num_frames == 0:
                return []

            tokenized = self._tokenizer(tokens)
            # CTC needs a frame a character: a window too short for its text
            # cannot be aligned, and the aligner would raise.
            if sum(len(t) for t in tokenized) > num_frames:
                return []
            spans = self._aligner(emission[0], tokenized)

        ratio = (n_samples / num_frames / 16000.0) * 1000.0
        result: list[tuple[int, int]] = []
        for word_spans in spans:
            if not word_spans:
                result.append((0, 0))
                continue
            start_ms = int(word_spans[0].start * ratio)
            end_ms = int(word_spans[-1].end * ratio)
            result.append((start_ms, end_ms))
        return result
