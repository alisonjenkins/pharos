# pharos-hls corpus

Streams recorded from a seeded pharos (spec 031 T031-01) and read by
`tests/it_corpus`. They are what the server sends a browser for the seeded
120 s `fixture-1.webm`:

| Directory | Master | Media kept |
|---|---|---|
| `h264-cmaf/` | `renditions=h264&abr=1`, browser user agent: demuxed H.264 fMP4 ladder (source and four capped rungs) plus one Opus group | init and segment 0 of the source rung, the 360p rung and the audio |
| `vp9/` | `renditions=vp9`: one VP9 fMP4 rung plus one Opus group | init and segment 0 of video and audio |

Every playlist is kept. Media segments past 0 are not, so the corpus stays
under 2 MB (about 1 MB today). Tokens are replaced with `REDACTED`.

Recorded 2026-11-10T00:10Z from main at 3278f409f, with the dev shell's
ffmpeg, software encode.

## Re-record

Start the seeded server that `just compat-dioxus-full on` starts. The
quickest way is a copy of the justfile with that recipe's `npx playwright
test` line replaced by
`python3 scripts/record-hls-corpus.py http://127.0.0.1:{{ port }} <dir>`.
Then copy `<dir>/h264-cmaf` and `<dir>/vp9` here and run
`cargo nextest run -p pharos-hls -E 'binary(it_corpus)'`.

## Not here yet

There is no audio-only stream yet. The seeded "audio" item (`fixture-4.webm`)
is a copy of the video base, so its master is a video master.
`05-carefree.mp3` in the test media tree is not in a library that the seeded
user can list.
