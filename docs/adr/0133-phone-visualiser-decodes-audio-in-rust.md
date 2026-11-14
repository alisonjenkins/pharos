# ADR-0133: The phone visualiser decodes the song in Rust

- **Status:** Accepted
- **Date:** 2026-11-14T13:21:00Z
- **Deciders:** Alison (no new permission, no audio capture)

## Context

The phone plays music on a native `MediaPlayer` (ADR-0127, ADR-0131). The
Dioxus UI runs outside the WebView. The page's canvas draws the visualiser
(ADR-0104). Today that page has no `<audio>` element to analyse, so the
phone's visualiser shows nothing.

The visualiser needs two things per frame: spectrum bins in dBFS and a
window of waveform samples (`AudioTap`, `visualiser.rs`). The web build
gets both from an `AnalyserNode` (FFT 2048, smoothing 0).

Constraints:

- RAM is scarce and battery matters on the phone.
- The app must not ask for a new permission just to draw bars.
- Web, desktop and TV must not change.

## Decision

We will decode the song a second time in Rust and analyse that.

1. **`pharos-vis` decodes and analyses.** It uses `symphonia` with default
   features off and only mp3, flac, aac, isomp4, ogg, vorbis, wav and alac.
   It uses `realfft` for a 2048-point FFT (Blackman window, 1024 bins in
   dBFS) and a 2048-sample waveform window per frame.
2. **It runs only when it is needed.** A background thread works while the
   visualiser is visible and the song is playing. It stops on hide, pause
   or end. It keeps 2 s of frames at 30 fps (60 frames, about 180 KB, bins
   as `u8` in 0.5 dB steps and waveform as `i8`).
3. **It runs just ahead of the playhead.** Frame index = (snapshot
   position + time since the snapshot) x 30 fps. A seek moves the decoder
   and empties the ring.
4. **Source.** The whole cached file when there is one. Otherwise a ranged
   HTTP read of the stream URL through the content crate's own client.
5. **The port.** `NativeAudio` gains `visualiser_enable(on)` and a pull
   `visualiser_frame() -> Option<VisFrame>`. The host pushes each frame
   (about 4 KB as base64) into the page at about 30 Hz. The page's wasm
   reads it through a `JsFrameTap`, an `AudioTap` over a window global. Only
   a page started for the host does this. The TV page is unchanged.
6. **Failure is quiet and logged.** An unsupported codec (for example Opus)
   or a decode error gives no frames, and the visualiser shows its idle
   state. One line says why:
   `visualiser source=decode verdict=on|off|unsupported codec= reason=`.

## Consequences

- No `RECORD_AUDIO` permission and no `android.media.audiofx.Visualizer`.
- The picture is a second decode, so it can drift from what is heard by the
  player's buffering and the timer. The position snapshots resync it. A
  small offset is invisible in a visualiser.
- Decode costs CPU only while the visualiser is on screen and playing.
  Without the visualiser, the cost is zero.
- The binary grows by the codecs listed above.
- Opus (and any codec not listed) has no visualiser on the phone.
- A streamed song is fetched twice while the visualiser is on (player and
  decoder), unless it is cached. The extra read is only the ahead window,
  but it is real data use.
- The volume slider does not change the picture: the decode is before the
  player's gain. The page's `volume()` is 1.

## Alternatives considered

- **`android.media.audiofx.Visualizer` with `RECORD_AUDIO`.** The platform
  does the FFT for the player's session, with no second decode. It needs a
  runtime permission that reads as microphone access, the capture is limited
  to 8-bit and 1024 samples, and some devices (and DRM or offloaded output)
  return silence. Rejected: the permission.
- **Server analysis.** The server analyses once per song and ships a track
  of frames. Phone cost is near zero. It needs a new analysis job, storage,
  an API and offline copies of the data; streaming songs the server has not
  analysed show nothing. Rejected for now: the scope is a new server
  feature. Revisit if decode cost on old phones is too high.
- **Local decode (chosen).** Works offline and online, adds no permission,
  and keeps the server unchanged.

## References

- ADR-0104 (the page runs the visualiser as wasm)
- ADR-0127, ADR-0131 (native player, one APK)
- `crates/pharos-ui/src/native/visualiser.rs` (`AudioTap`, `Analysis`)
