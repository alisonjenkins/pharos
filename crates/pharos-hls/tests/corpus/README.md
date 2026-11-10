# pharos-hls corpus

Streams recorded from a seeded pharos (spec 031 T031-01) and read by
`tests/it_corpus`. They are what the server sends a browser for the seeded
120 s `fixture-1.webm` and the test media tree's `05-carefree.mp3`:

| Directory | Master | Media kept |
|---|---|---|
| `h264-cmaf/` | `renditions=h264&abr=1`, browser user agent: demuxed H.264 fMP4 ladder (source and four capped rungs) plus one Opus group | init and segment 0 of the source rung, the 360p rung and the audio |
| `vp9/` | `renditions=vp9`: one VP9 fMP4 rung plus one Opus group | init and segment 0 of video and audio |
| `audio-only/` | the mp3: muxed MPEG-TS, v3, source rung as MP3 (`mp4a.40.34`) and five AAC rungs | segment 0 of the source and 64k rungs |

Every playlist is kept. Media segments past 0 are not, so the corpus stays
under 2 MB (about 1.3 MB today). Tokens are replaced with `REDACTED`.

Recorded 2026-11-10T00:10Z from main at 3278f409f, with the dev shell's
ffmpeg, software encode.

## Re-record

Start the seeded server that `just compat-dioxus-full on` starts. The
quickest way is a copy of the justfile with that recipe's `npx playwright
test` line replaced by
`python3 scripts/record-hls-corpus.py http://127.0.0.1:{{ port }} <dir>`.
Then copy `<dir>/h264-cmaf` and `<dir>/vp9` here and run
`cargo nextest run -p pharos-hls -E 'binary(it_corpus)'`.

The audio-only stream was recorded at 2026-11-10T00:15Z, by the same
recorder.
