# ADR-0155: The server is unready while its media share is broken

- **Status:** Accepted. Superseded in part by ADR-0156: where the server can restart its own Deployment, it stays Ready instead.
- **Date:** 2026-11-24T12:45:00Z
- **Deciders:** Alison Jenkins

## Context

ADR-0154 rolls out a fresh pod for an aligner whose media share has gone stale. The
server mounts the same share and has the same fault. A stale handle there
makes every direct-play and HLS request fail, and the server used to answer
each one 404 "media source unreadable", as if the file were gone. Its
`/readyz` checked only the process and the store, so the pod stayed Ready
and nothing replaced it (B1531).

The server is one pod on SQLite. While it is out of its Service, nothing in
Pharos answers: browsing, sign-in and metadata go down too, not only
playback.

## Decision

The server's `/readyz` fails while its media share is broken, and the
chart lets `selfHeal` roll out a fresh server pod as it does the aligner's
(a `restartedAt` rollout restart of the Deployment, ADR-0154).

- A media read that fails with `ESTALE`, `ENOTCONN` or `EIO` answers 503,
  not 404, and marks the share broken until the process ends.
- `/readyz` also lists each configured media root on its own thread within
  2 s. It fails on a timeout or a share-level error.
- A root that is missing or refused does not fail `/readyz`. A fresh mount
  would not change it, and one deleted library folder must not take the
  whole server down.
- The server Deployment carries `pharos.io/self-heal=true`.

## Consequences

- Once the share is broken the server is NotReady, so all of Pharos is out
  of its Service, not only playback, until a fresh pod is Ready. With the
  defaults that is about 6 to 10 minutes (readiness fails within 60 s, then
  the reaper's stamp, its 5-minute limit and its rollout, as
  `docs/kubernetes.md` tabulates), plus the new pod's startup. The strategy
  does not shorten it: the old pod serves nothing either way. With
  `RollingUpdate` (Postgres) the new pod is created alongside the old one;
  with `Recreate` (SQLite, forced by the chart) the old one stops first.
- Before this, playback was down until someone recreated the pod by hand,
  while the rest of Pharos kept working.
- If the NAS itself is down, the new pod fails `/readyz` too. The reaper
  rolls it out again about every 7 to 9 minutes until the share answers,
  and Pharos is unreachable for that time.
- A single file with an I/O error (`EIO`) marks the whole share broken. On
  NFS that error almost always means the share, and the cost of being wrong
  is one pod replacement.
- With `selfHeal` off, a broken share leaves the server unready until someone
  recreates the pod, so all of Pharos is unreachable rather than only
  playback. `selfHeal` is on by default for this reason.
