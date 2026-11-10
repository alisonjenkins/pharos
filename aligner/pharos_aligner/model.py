"""Forced alignment model loading and inference with PyTorch and torchaudio."""

from __future__ import annotations

import ctypes
import gc
import logging
import threading
import time
from typing import Any, Callable

import torch
import torchaudio

from pharos_aligner.tracing import tracer

logger = logging.getLogger("pharos_aligner")

# MMS_FA's wildcard label, matching any audio that is not the transcript.
STAR = "*"

Clock = Callable[[], float]


def _release_freed_memory() -> bool:
    """Hand glibc's freed heap pages back to the kernel; True if the trim ran."""
    # The weights' smaller tensors sit in malloc arenas, which glibc keeps
    # after free; without a trim the unload barely moves RSS. libc is already
    # loaded, so look the symbol up in the process itself: find_library("c")
    # needs ldconfig or a compiler, and the service image has neither.
    try:
        ctypes.CDLL(None).malloc_trim(0)
    except (OSError, AttributeError) as e:
        logger.warning("malloc_trim unavailable; freed heap stays resident: %s", e)
        return False
    return True


def _anon_mib() -> int | None:
    """This process's anonymous memory in MiB, or None off Linux."""
    try:
        with open("/proc/self/smaps_rollup", encoding="ascii") as f:
            for line in f:
                if line.startswith("Anonymous:"):
                    return int(line.split()[1]) // 1024
    except (OSError, ValueError, IndexError):
        return None
    return None


def set_threads(n: int) -> None:
    """Cap the threads torch runs each op on."""
    torch.set_num_threads(n)


class Aligner:
    """Lazy-loaded multilingual forced aligner using torchaudio MMS_FA.

    With `idle_unload_s` set, `unload_if_idle` drops the model once no
    alignment has run for that long; the next alignment loads it again.
    """

    def __init__(self, idle_unload_s: float | None = None, clock: Clock = time.monotonic) -> None:
        self._model: Any = None
        self._tokenizer: Any = None
        self._aligner: Any = None
        self._idle_unload_s = idle_unload_s
        self._clock = clock
        self._last_used = clock()
        # align and unload_if_idle run on different threads.
        self._lock = threading.Lock()

    @property
    def loaded(self) -> bool:
        return self._model is not None

    def _load(self) -> tuple[Any, Any, Any]:
        bundle = torchaudio.pipelines.MMS_FA
        model = bundle.get_model()
        model.eval()
        return model, bundle.get_tokenizer(), bundle.get_aligner()

    def _ensure_loaded(self) -> None:
        if self._model is not None:
            return
        started = time.monotonic()
        self._model, self._tokenizer, self._aligner = self._load()
        logger.info("model loaded in %d ms", int((time.monotonic() - started) * 1000))

    def unload_if_idle(self) -> bool:
        """Drop the model if it has sat unused for the idle timeout; True if it did."""
        if self._idle_unload_s is None:
            return False
        with self._lock:
            if self._model is None:
                return False
            idle_s = self._clock() - self._last_used
            if idle_s < self._idle_unload_s:
                return False
            self._model = None
            self._tokenizer = None
            self._aligner = None
            anon_before = _anon_mib()
            gc.collect()
            trimmed = _release_freed_memory()
            anon_after = _anon_mib()
        logger.info(
            "model unloaded after %d s idle anon_before_mib=%s anon_after_mib=%s trimmed=%s",
            int(idle_s),
            anon_before,
            anon_after,
            trimmed,
        )
        return True

    def align(self, pcm_f32le: bytes, tokens: list[str], lead_star: bool = False) -> list[tuple[int, int]]:
        """Align tokens against 16kHz float32 PCM audio, returning (start_ms, end_ms) per word.

        `lead_star` lets music before the first word go unmatched, for a
        window that starts well ahead of where the line may be sung.
        """
        if not pcm_f32le or not tokens:
            return []
        with self._lock:
            try:
                self._ensure_loaded()
                return self._align_loaded(pcm_f32le, tokens, lead_star)
            finally:
                self._last_used = self._clock()

    def _align_loaded(self, pcm_f32le: bytes, tokens: list[str], lead_star: bool) -> list[tuple[int, int]]:
        waveform = torch.frombuffer(bytearray(pcm_f32le), dtype=torch.float32).unsqueeze(0)
        n_samples = waveform.size(1)
        if n_samples == 0:
            return []

        with torch.inference_mode():
            # The wav2vec2 forward pass and the CTC search, apart: which of
            # them a slow song spends its time in decides the fix (B1164).
            with tracer.start_as_current_span("aligner.forward", attributes={"samples": n_samples}):
                out = self._model(waveform)
            emission = out[0] if isinstance(out, tuple) else out
            num_frames = emission.size(1)
            if num_frames == 0:
                return []

            # A star token after the last word soaks up the music that follows
            # the line; without it CTC must spend every frame of the window on
            # the lyrics, smearing the words across an instrumental break up
            # to the next line. None goes before the first word: a line is sung
            # from its stamp, and a leading star let the words drift late.
            lead = [STAR] if lead_star else []
            with tracer.start_as_current_span("aligner.ctc", attributes={"frames": num_frames}):
                tokenized = self._tokenizer([*lead, *tokens, STAR])
                # CTC needs a frame a character: a window too short for its
                # text cannot be aligned, and the aligner would raise.
                if sum(len(t) for t in tokenized) > num_frames:
                    return []
                spans = self._aligner(emission[0], tokenized)[len(lead) : -1]

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


def start_idle_reaper(aligner: Aligner, interval_s: float) -> threading.Thread:
    """Check every `interval_s` seconds whether the model has idled out."""

    def run() -> None:
        while True:
            time.sleep(interval_s)
            try:
                aligner.unload_if_idle()
            except Exception:
                logger.exception("idle unload failed")

    thread = threading.Thread(target=run, name="aligner-idle-reaper", daemon=True)
    thread.start()
    return thread
