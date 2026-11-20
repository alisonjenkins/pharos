# ADR-0148: Downloads run from one shared queue and resume across play sessions

- **Status:** Accepted
- **Date:** 2026-11-20T21:00:00Z
- **Deciders:** Alison Jenkins

## Context

ADR-0143 makes an offline video the player's HLS stream, stored under the
exact URLs the player asks for; ADR-0145 does the same on the phone, on disk.
Every one of those URLs carries the `PlaySessionId` of the grant the Download
control negotiated, and that id is the capability that lets the server answer
them (`play_capability.rs`).

The grant belongs to the RPC connection that negotiated it. When the
connection drops, the grant is released and held for 30 s
(`pending_release::DEFAULT_RELEASE_GRACE`); after that the session ends and
every URL of the download answers 401. The playback audit of 2026-11-20 found
what follows from that:

- After 30 s offline a download fails with a 401 (B1486).
- Retry negotiates a new grant. Its URLs carry a new `PlaySessionId`, so no
  stored key matches, the stored files are dropped, and the download starts
  from zero.
- Resuming after the app closed hits the same wall: the old session is gone.
- The Download control kept every grant until the app closed, so a finished
  download still held a play session.
- Starting several downloads ran them all at once, each with its own
  transcode on a RAM-scarce server.

Viewers want a queue: add several videos, let them download one by one, and
have them carry on by themselves after a dropped connection, a server
rollout or closing the app.

## Decision

1. **One queue, shared.** The rules live once, in `pharos-ui`
   `native/download_queue.rs`, with no I/O: the saved `Queue` (one entry per
   video the viewer asked for, in order), `next_action`, the backoff
   (`Tries`) and the Downloads page's rows. A driver
   (`native/download_driver.rs`) runs them once a second on the web and the
   phone alike. The stores (`kept_video_web.rs`, `kept_video_disk.rs`) only
   fetch and store bytes.
2. **One download at a time.** Only the front entry that is not paused or
   failed downloads; files within it keep the stores' small concurrency.
   One transcode per viewer, and a RAM bound on both ends.
3. **Negotiate when an entry reaches the front, not when it is added.**
   Adding checks room against the cap and the 2 GiB free-space floor with an
   estimate from runtime and the chosen quality, counting everything already
   queued. The grant is taken when the download starts and dropped as soon
   as its record stops downloading. Nothing holds a grant while it waits.
4. **Store keys stay the first plan's URLs; fetches follow the live
   session.** A start whose master URL differs from a stopped download's
   only in `PlaySessionId` (same version, same quality) renews it: the record
   keeps its master URL and stored keys (`renewal`, `renewed_record`), and
   each missing key is fetched with its `PlaySessionId` swapped for the new
   one (`pharos_hls::offline::rebind`). Playlists, the service worker, the
   phone proxy and offline playback are untouched.
5. **A renewed stream must be the stored one.** Before fetching, the run
   compares each stored playlist with the renewed one once the session ids
   are put back (`same_playlist`; the phone localizes the renewed text first,
   as it does when it stores), and each stored init segment byte for byte.
   A segment stored without its init cannot be checked, so it counts as a
   difference. Any difference stops the run with the halt `stream_changed`, naming the
   first difference with the session hidden; the driver then removes that
   download and starts it afresh.
6. **Only what a retry cannot mend is a failure.** A fetch that ends on 401,
   403 or 410 halts as `session_ended`; no answer, 404 or 5xx past the
   store's retries halts as `unreachable`. The halt is saved on the record
   (`KeptVideo.halt`, absent in earlier builds, which read it as a plain
   failure). A halted download is tried again by itself after 2, 5, 15, 30
   and then every 60 seconds. Tries the server answered count towards giving
   up (10); tries that could not reach it do not, so a viewer offline for a
   day keeps the queue. Any stored progress starts the count again.
7. **Resume is by file.** A segment is a few seconds of video; a cut-off one
   is fetched again whole. No byte ranges.
8. **Signals.** `video keep renewal … verdict=renewing|fresh files_kept=`,
   `video keep renewed … verdict=same_stream|stream_changed`, `video keep
   failed … halt=`, `video keep paused`, `video queue action=start|wait|restart
   … reason=`, `video queue grant released … reason=`. Server side,
   `pharos_play_capability_total{outcome="no_session"}` counts the 401s.

The 4 GiB default cap and the 2 GiB free-space floor do not change.

## Consequences

- A dropped connection, a server rollout or a closed app costs one
  re-negotiation and the segments not yet saved, never the saved ones.
- A finished, paused or queued download holds no play session.
- The server sees at most one download transcode per viewer.
- A renewal fetches each playlist and init segment once more to compare
  them: a few hundred kilobytes, against a download of a gigabyte or more.
- If a renewed stream does not match (another encoder, another segment
  grid), that download starts from zero, as before, and the log line says
  why. How often that happens is to be read from those lines.
- The phone downloads only while the app's process lives. Downloading with
  the app in the background on Android (a foreground service) is separate
  work.

## Alternatives considered

- **Key the store by URL with `PlaySessionId` removed.** Lost: the service
  worker (JavaScript) and the phone proxy would both have to strip the same
  parameters from every request, and a mismatch would silently miss.
- **Download grants that outlive the connection.** Lost for now: a server
  change for every client, and a grant still ends at the 6 h cap or a
  restart, so renewal is needed anyway. Renewal alone covers both.
- **Run several downloads at once.** Lost: one transcode per download on a
  RAM-scarce server, and no faster on one viewer's link.

## References

- ADR-0143, ADR-0145, spec 048.
- B1486, V1477.
