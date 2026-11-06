# Animated, typeset anime subtitles from a local model — design

Status: research and design only, nothing built. Written 2026-11-05 for Alison.

## 1. What we are trying to make

Good fansubs did more than translate. The parts worth copying in spirit:

| What fansubs did | What it needs from us |
|---|---|
| **Dialogue that reacts.** Shouts punch in, whispers shrink and fade, inner thoughts glow, the phone voice sits in a box. | Who is speaking, how loud, what emotion, what the line means |
| **Typeset signs.** On-screen Japanese text (shop signs, letters, title cards) gets an English version that matches its colour and font weight, sits on the sign and moves with it. | Find text in the frame, read and translate it, follow it as the camera moves |
| **Karaoke.** Opening and ending songs light up syllable by syllable, with effects on the beat. | Song boundaries, lyrics, syllable timing, tempo |
| **Placement.** Lines move out of the way of on-screen text and faces. | Where the busy parts of the frame are |

The goal is **original Pharos effects in that spirit**, not a copy of any group's look or assets. The design below makes that structural: the model chooses *what kind of moment this is*, and Pharos's own effect library decides *how it looks*.

## 2. Where Pharos is today (checked in the repo)

- The web UI already draws ASS/SSA with **libass in WebAssembly (JASSUB)**, over the `<video>` (`crates/pharos-ui/src/native/web.rs`, `StyledSubtitles`, vendored at `/ui/assets/vendor/jassub`). The server serves the original script at `…/subtitles/{n}/stream.ass` (`playback.rs::ass_url`).
- Known gaps (backlog): **external `.ass` files show as plain text**, and **embedded fonts (MKV attachments) are not used**. Both have to be fixed before any of this, because the generated track will be an external `.ass` that needs its own fonts.
- Audio analysis already runs per file in the libav worker pool (`pharos_server::audio_analysis`, tempo and energy), with the "valid only while the file signature matches" rule. The karaoke work has a planned forced-alignment worker (part 3, #762, waiting on the 3–4 GB image OK). Both are patterns to reuse.
- Some anime files have no subtitles at all (Kaiju No. 8, One Piece, Ancient Magus' Bride in the library). Styling needs text, so those need a text source first (see §4).

## 3. Output target

### Recommendation: a Pharos effect plan, compiled to ASS

Two layers:

1. **Effect plan (Pharos-native, semantic).** A small, typed description of *intent*: "line 212 is a shout, intensity 3, speaker B", "sign at 4:31–4:35 on this tracked quad, says 'Baratie'", "song 0:00–1:30, these syllables, 128 BPM". This is what detectors and the model produce.
2. **Compiler (deterministic Rust).** Turns plan + theme + device budget into **ASS v4+ with libass override tags** (`\pos \move \t \fad \kf \blur \bord \frz \fscx \clip \p` drawings and so on).

Why not have the model write ASS directly:

- ASS is long, fiddly and easy to get subtly wrong. A tracked sign is one line per frame (hundreds of lines for a few seconds); no language model gets that pixel-accurate. Geometry belongs to computer vision, not to the model.
- A small output vocabulary (a dozen intents, an intensity, a placement) is learnable from hundreds of examples, not millions.
- The look lives in Pharos's own effect templates, so we never learn or reproduce a fansub group's style.
- One plan, many renderings: a **device budget** (TV gets cheaper effects than a desktop), a user **intensity** setting (Off / Subtle / Full), and `prefers-reduced-motion` all apply at compile time.
- It is testable and observable: every plan entry carries the reason it was chosen; the compiler can be property-tested (output parses, every event inside its time range, budget respected).

Why ASS as the final format and not something new: JASSUB already renders it on the web, jellyfin-web renders it, and other Jellyfin clients either render it or fall back to Pharos's existing burn-in path. Zero new renderer work for v1.

Effect plan sketch (would live in `pharos-core`, cap'n proto for the native API):

```text
EffectPlan { version, source_track, analysis_version, file_signature,
  lines:  [LineFx { start_ms, end_ms, text, speaker: Option<SpeakerId>,
                    intent, intensity: 0..=3, placement, reason }],
  signs:  [SignFx { start_ms, end_ms, text_src, text_en,
                    track: [Keyframe { t_ms, quad: [Point; 4] }],
                    fill, outline, weight, reason }],
  songs:  [SongFx { start_ms, end_ms, bpm, lines: [[Syllable { t_ms, dur_ms, text }]],
                    style, reason }] }

Intent = Plain | Shout | Whisper | Thought | Narration | OffScreen
       | Phone | AttackCall | Flashback | Sarcasm | Sfx
```

### Original effect library (first set)

Named generically, designed by us, fonts from OFL-licensed families (Google Fonts) shipped with Pharos:

- **Impact** (shout): 115% scale pop to 100% over 120 ms, slight tilt, brief glow.
- **Tremor** (scream): few-pixel jitter, damped over the line.
- **Hush** (whisper): smaller, 70% opacity, soft blur, slow fade.
- **Inner voice** (thought): italic, soft halo, no bracket clutter.
- **Echo** (attack call): two ghost copies trailing and fading; big title-card variant at Full intensity.
- **Wire** (phone, radio): boxed, monospaced, slight flicker.
- **Memory** (flashback): desaturated, warm tint, letter-spaced.
- **Speaker colour**: each recurring speaker gets a stable, readable outline colour per show (Off by default; some people hate it).
- **Karaoke**: Sweep (fill left to right), Bounce (syllable lifts on its beat), Ember (glow trail), each pulsing on the measured BPM.
- **Sign**: English text matched to the sign's colours, pinned to its tracked quad with perspective, clipped to it.

## 4. Inputs and how we get each one

All of this runs offline, per episode, after scan. Nothing runs during playback.

| Signal | Source | Tool (all local, all open) | Notes |
|---|---|---|---|
| Text and timing | Existing subtitle track (embedded or external) | Parse existing SRT/ASS | Existing ASS lines that already carry `\pos`/`\k` are kept as-is and only restyled |
| Text when there is none | Audio | Whisper large-v3-turbo (whisper.cpp) with translate, or JA transcript + local LLM translation | Quality is the weak point; mark the track "machine translated" |
| Word and syllable timing | Audio + text | CTC forced alignment (wav2vec2/MMS), the karaoke aligner (#762) | Japanese lyrics need romaji mora split (a romaniser like cutlet/pykakasi) |
| Loudness and pitch per line | Audio | libav in the existing worker (EBU R128 short-term, F0) | Cheap; the MVP's main signal |
| Vocals vs music | Audio | Demucs/HTDemucs | Makes loudness and alignment robust during battle music |
| Speaker | Audio | pyannote diarization + voice embeddings clustered per show | Anime has many voices; expect noise, so speaker colour stays opt-in |
| Emotion | Audio (+ text) | emotion2vec-class speech emotion model; text sentiment from the LLM | Angry/happy/sad/neutral/surprised is enough |
| Shots | Video | ffmpeg `scdet` or TransNetV2 | Effects never straddle a hard cut |
| Opening / ending | Chapters, then audio fingerprint across episodes | Chromaprint-style matching (same song every week) | Also feeds "skip intro" later |
| Tempo | Audio | Existing `audio_analysis` tempo | Already shipped |
| On-screen text | Keyframes per shot | Text detector (DBNet/PaddleOCR) + Japanese OCR (manga-ocr) | Only on shots where text is detected |
| Sign motion | Frames | OpenCV feature tracking + homography (KLT/ORB, RANSAC) | Gives the per-frame quad; classical CV is better at this than any model |
| Sign colours | Frames | k-means on the detected region | |
| Where faces and text are | Frames | Anime face detector + the text boxes above | For placement |
| Scene mood | Keyframes | Small VLM or SigLIP tags (battle, comedy, calm, flashback) | Optional, phase 4 |

## 5. Model options

**A. Detectors + rules.** Features per line go through a hand-written rule table ("loud and ends in ! → Shout; loud and long and caps → Tremor"). Cheap, explainable, no training. Ceiling: no sense of meaning (sarcasm, an attack name said quietly, a joke).

**B. Detectors + a small model as the "director" (recommended end state).** Each line becomes a feature record (text, ±5 neighbouring lines, speaker, loudness, pitch, emotion, shot, song/no song, scene tags). A small model picks the plan fields. Two sizes worth trying:
- a tiny classifier (gradient-boosted trees or a ~100M-parameter text encoder) for intent and intensity: fast, trains on a laptop, often as good as an LLM for this kind of choice;
- a 3–4B instruction model (Qwen3-4B-class, quantised, via llama.cpp) with **grammar-constrained JSON output** over the plan schema, so it can never emit something invalid. Zero-shot first, then a LoRA fine-tune on our own labels.

**C. One big vision-language model that watches the video and writes ASS.** Not recommended: there is almost no clean paired data, outputs are long and must be pixel-precise, it would be slow on our GPUs, and it is the option most likely to memorise and replay a specific group's work. A VLM is useful only as one more tagger in B.

So the shape is: **classical CV for geometry, audio models for who/how, a small model for "what kind of moment is this", Pharos templates for the look.**

## 6. Training data, and copyright, plainly

The obvious dataset is old fansub `.ass` releases paired with raw video: fansubs encode exactly the decisions we want (this line is shouted, this sign is here). Technically it works: sync each script to the raw with audio alignment, reverse-compile its tags into plan labels (big `\fs` + `\pos` on a non-dialogue style → sign; `\k` → karaoke; scale `\t` on a line → emphasis), and train B on those labels.

The legal position, in plain words (not legal advice):

- The **anime and raws are copyrighted**. Fansub scripts are **unlicensed translations** of that copyrighted dialogue, so they are themselves infringing derivative works, and the **typesetting is the fansubbers' own creative work** with its own copyright and no licence granted.
- Text-and-data-mining exceptions don't fit well. In the UK, s29A covers non-commercial research **with lawful access** to the material; downloaded fansubs and raws are not lawful access. The EU exceptions also assume lawful access. In the US, fair use for training is being litigated and is unsettled.
- Practical risk for a private home server that never distributes the model or its output is low, but it is **not clean**, and it is the route most likely to make the model reproduce one group's signature effects, which is the thing we said we won't do.

**Recommendation: don't train on fansub scripts.** Use this instead:

1. **Labels we make ourselves.** The rule pipeline (phase 1) produces a plan for every episode. A small "Effects review" panel in the player lets Alison (and anyone she invites) mark an effect *good / too much / wrong / should be X*. A few evenings of watching gives a few thousand labelled lines from her own library.
2. **Synthetic labels.** A local LLM labels intent from the text alone; we keep the ones the audio agrees with. Cheap bootstrap, no third-party creative work involved.
3. **Corrections become preference pairs** for the LoRA fine-tune.

If she still wants fansub-derived data later, the least-bad use is *aggregate statistics* (how often loud lines get bigger), never training targets, and that call is hers.

## 7. Hardware

What the repo says (not live-checked; the desktop was offline when I looked):

- **Home k3s node: NVIDIA GTX 1070, 8 GB** (driver 580, `runtimeClassName: nvidia`; specs 003/006). It is the transcode GPU. Pascal has no tensor cores and slow fp16, so it can run inference for small models but would compete with playback. **Not a good place for this.**
- **Dev host with an AMD RX 9070 XT, 16 GB** (spec 003 research). I'm **assuming that is ali-desktop**; please confirm. RDNA4 is supported by recent ROCm releases and by llama.cpp/whisper.cpp through Vulkan, so all the inference above fits. Some PyTorch extras (bitsandbytes 4-bit) are patchier on AMD, so LoRA training is plain bf16 LoRA on a 3–4B model, which fits in 16 GB; if it fights us, a rented cloud GPU for one afternoon (roughly £5–20) finishes it.

Estimated cost per 24-minute episode on the 9070 XT (assumptions, to be measured): vocal separation ~1 min, Whisper (only if there are no subs) ~1–2 min, alignment + diarization + emotion ~2 min, OCR on text-bearing shots ~1–2 min, tracking (CPU) ~2–4 min, the director model ~2–5 min. **About 10–15 minutes per episode.** One Piece's ~1,100 episodes would be ~10 days of GPU time, so run it **on demand**: the next few episodes of whatever someone is watching, plus newly added episodes overnight.

Where it runs: a separate, crash-isolated worker (V6 spirit), shipped as its own OCI image like the karaoke aligner, that pulls jobs from Pharos's API and writes plans back. It can run on ali-desktop when it's on and fall back to a CPU-only rules pass on the cluster. Python is acceptable inside that worker because the models live there; the plan format, compiler and serving stay in Rust.

## 8. Rendering path

- **Web (now):** the compiled `.ass` appears as an extra subtitle track, "English (Pharos styled)", next to the originals, served at `stream.ass` and drawn by JASSUB. Player subtitle menu gets the track; Appearance gets **Effects: Off / Subtle / Full** (default Subtle) and honours reduced motion.
- **Android TV (later):** the TV plan already uses a WebView with hls.js, so JASSUB works there unchanged. The risk is the TV's slow CPU with heavy blur or per-frame sign lines; the compiler's **device budget** drops blur, caps events per second and simplifies tracking for TV. If that's still too slow, native libass (C, built for Android, called from Rust) rendering into an overlay is the step after.
- **Jellyfin clients:** get the same `.ass` track. Clients that can't render ASS get it burned in by the existing burn path.

## 8a. Performance on weak devices (Alison's requirement)

Rule: **all thinking happens once, on the server, before anyone presses play. The client only draws.** Nothing model-related ever runs on a phone, TV or browser.

Server side:
- Analysis and the director model run once per episode, offline, as a background job that yields to playback: it only takes the GPU when `pharos_transcode_pending_jobs` is zero and backs off as soon as a transcode is queued. Small quantised models only (≤4B, Q4), one episode at a time, results cached against the file signature so nothing is ever recomputed.
- The compiler emits **one `.ass` per device tier** (desktop / TV / low), cached; each is a few hundred KB, so that's free.

Client side, cheapest to dearest, the compiler picks per tier:
- **Low tier (weak TV, old phone): no animation.** Each line keeps its colour, size, position and box from the plan, but no blur, no `\t` transforms, no per-frame lines. libass renders a static line once and reuses the bitmap, so this costs about the same as plain subtitles. Signs become a static, pinned English caption.
- **TV tier:** only cheap effects: fades, scale pops and moves done with a single `\t`/`\move` (libass interpolates, no extra events), border instead of blur, karaoke as `\kf` fill (cheap) instead of per-syllable animation, at most ~3 animated events on screen, tracked signs reduced to a few keyframed `\move` segments instead of one line per frame.
- **Desktop tier:** the full library.
- **Zero-cost fallback:** a client that can't run JASSUB at all gets the styled track pre-rendered by the server as an **image subtitle track** (the spec 019 image-cue path the player already draws), static frames only, or burned in when it's transcoding anyway.

Known expensive libass features we budget hard: `\blur` and `\be` (worst), large `\bord`, vector `\clip`, many simultaneous animated events, and per-frame line spam. The compiler enforces a per-tier budget (events per second, animated events on screen, blur allowed y/n) and a property test asserts every compiled file stays inside it.

Self-correcting: the player measures JASSUB render time and dropped frames (opt-in telemetry, plus a local check that needs no telemetry). If a device can't keep up over a few seconds, it switches itself down a tier for the rest of the session and remembers that for the device. Default tier comes from the device class, so a TV starts on TV tier, not desktop.

## 8b. Contract with the Rust subtitle renderer (spec 026)

The "Render subtitles in our own Rust code" thread owns spec 026, ADR-0064, B572–B577 and V485–V490, so this work must not use those ids. Its contract: this pipeline delivers **plain ASS v4+**, served like any sidecar `.ass`, no custom format on the wire. That matches this design: the effect plan is internal to the server and only compiled ASS leaves it. The renderer's animation tier covers `\t \move \fad/\fade \k/\kf/\ko \frx/\fry/\frz \fscx/\fscy \blur/\be \bord/\shad`, rect and vector `\clip`, `\p` drawings and `\org`, which is every tag the effect library above uses; JASSUB draws them all today, so compiled output can be tested now. The image-cue fallback in §8a uses the existing spec 019 path; anything beyond that goes to that thread first.

## 9. Observability (named before building)

- `pharos_subtitle_styling_total{outcome,stage}`: per stage (`asr`, `align`, `diarize`, `ocr`, `track`, `direct`, `compile`), `ok/failed/skipped` with the reason logged with the offending value.
- `pharos_subtitle_effect_total{intent,source}` where source is `rule` or `model`: the effect mix per library.
- One log line per plan entry carrying its inputs, verdict and reason (`intent=Shout reason="lufs=-9.1 > -14, ends with !"`), so "why did this line shake?" is answerable by query.
- UI telemetry (opt-in): JASSUB dropped frames and render time per frame, by device class. This is the number that decides how heavy TV effects can be.
- Query to prove it's working after deploy: `sum by (intent, source) (increase(pharos_subtitle_effect_total[1d]))`.

## 10. Phased plan, effort and cost

| Phase | What | Effort (thread-weeks) | Needs from Alison |
|---|---|---|---|
| **0. Prerequisites** | External `.ass` through JASSUB; use embedded MKV fonts; ship an OFL font pack | ~0.5–1 | nothing |
| **1. MVP: rules, no ML** | Effect plan + compiler + 10–12 original effects; inputs only from what Pharos already has: existing subs, loudness/pitch per line, punctuation and caps, shot cuts, chapters, tempo; karaoke where lines already carry `\k` or the aligner gives timings; Effects setting | ~2–3 | watch some episodes, say what's too much |
| **2. Detectors** | Worker image; vocal separation, diarization + speaker colour, speech emotion, OP/ED fingerprinting, sign detection + OCR + tracking + translation | ~3–4 | OK for a ~6–10 GB model image; confirm the desktop GPU |
| **3. Learned director** | Zero-shot LLM director with constrained output; "Effects review" panel; LoRA fine-tune on her labels; A/B against rules | ~3–5 + a few evenings of labelling | labelling time |
| **4. Optional** | VLM scene tags; native libass on TV; per-show palette from key art; Whisper subs for shows with none | open | |

Running cost: hardware is already owned. Electricity at ~300 W for ~15 min per episode is about 0.075 kWh, so pennies per episode and around £20 for the whole One Piece back catalogue. Training: a few hours locally, or £5–20 rented once. Storage: under 1 MB of `.ass` per episode; models ~6–10 GB.

Main risks: sign translation quality (OCR + MT of stylised Japanese is the hardest piece; it can ship last), effects that get tiring (hence Subtle by default and the review panel), TV render cost (hence the budget), and noisy speaker detection in big casts (hence opt-in colours).

## 11. Recommendation

Build phase 0 and phase 1 now, with the device tiers from §8a from day one so weak devices pay almost nothing. They need no models, no new hardware and no training data, they fix two existing subtitle bugs on the way, and they will show within a few weeks whether reactive subtitles are fun to watch or tiring. Design the effect plan and compiler properly in phase 1, since everything later plugs into them. Then add detectors (phase 2) and a small learned "director" trained on **our own labels**, not fansub scripts (phase 3). Skip the "one big model writes ASS" route.

Per CLAUDE.md, phase 1 becomes a new spec (`specs/0NN-styled-subtitles/`) and the plan format + compiler gets an ADR when it lands.
