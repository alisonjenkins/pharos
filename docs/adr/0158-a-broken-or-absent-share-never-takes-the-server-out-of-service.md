# ADR-0158: A broken or absent media share never takes the server out of its Service

- **Status:** Accepted. Supersedes ADR-0156 in part: the 15-minute give-up and the "never saw its share whole" fallback are gone, and a recent restart no longer makes the pod unready.
- **Date:** 2026-11-26T19:00:00Z
- **Deciders:** Alison Jenkins

## Context

On 2026-11-26 the storage host lost most of its disks and kept exporting
the bare, empty mountpoint (B1538).

1. The share went stale and the server restarted itself (ADR-0156).
2. The new pod mounted the empty export. Every media root was empty or
   missing. Each play answered 404 as if one file were gone, `/readyz`
   stayed green, and nothing named the cause.
3. The storage host rebooted, and the share broke in that pod. The pod
   had never seen its share whole, so ADR-0156 had it ask for no restart
   and go unready.
4. The reaper rolled out new pods. They could not mount the export, so
   none became Ready, and the whole site was down until the disks
   came back.

ADR-0156 named this case and accepted it: "after 15 minutes the old pod
gives up and goes unready too, and the reaper retries". Each retry is a
pod that cannot start either. Going unready never helps when the
replacement cannot mount, and browsing, sign-in and search never needed
the share.

An empty export is not a broken share in ADR-0155's sense. No read fails
with a stale handle, a lost connection or an I/O error. A fresh mount
of it is just as empty, so a restart cannot help.

## Decision

The server stays in its Service through any media share fault that a
restart could fix or that no restart can fix. Only a fault where its own
restart cannot be asked for still fails `/readyz` for the reaper.

- **Absent share.** When a `/readyz` check finds no media root holding a
  single entry (each one empty, missing or refused), the share is absent.
  - `/readyz` stays 200 and names the absence under `media`.
  - `pharos_media_share_ok` is 0.
  - The server logs `media share absent` once when it starts and
    `media share back` once a root holds an entry again.
  - While absent, a media read that fails as missing answers 503
    `media share absent`, not 404.
  - Nothing is latched and no restart is asked for. The next check with
    entries clears it.
- **Broken share, self-restart on.** The first fault asks for the
  restart whether or not a check ever saw the share whole. The pod then
  stays Ready (`media_restart: requested`) until it is replaced, with no
  deadline.
- **Recent restart.** A Deployment restarted in the last 10 minutes is
  still left alone. The pod stays Ready (`media_restart: waiting`) and
  asks again 5 s after the 10 minutes are up.
- **Still unready.** `/readyz` fails as ADR-0155 says only when the
  restart cannot be asked for: no restarter, a failed request, or a
  paused Deployment.

## Consequences

- With `RollingUpdate` and the chart's defaults, a storage outage leaves
  browsing up for as long as it lasts. The replacement pod waits in
  `ContainerCreating` until the export mounts, then takes over.
- A pod can stay Ready with a broken share indefinitely if its
  replacement never starts for an unrelated reason, such as an image
  pull failure. That pod serves everything but media, and the rollout
  shows as stalled (`kube_deployment_status_replicas_unavailable`).
  That was chosen over a dark site.
- A fresh install with no media at all reads as absent:
  `pharos_media_share_ok` is 0 until a root holds an entry. Nothing it
  could play is catalogued, so the 503s never reach anyone.
- The player shows a server problem for a 503, not "this browser can't
  play this video". A message that says storage is offline is a UI
  follow-up.
- The library scan's sweep guard (B98) already keeps an empty pass from
  deleting items. On 2026-11-26 it held on every empty pass.

## Alternatives considered

- **Restart on an absent share.** A fresh mount of an empty export is
  just as empty, so this only churns pods.
- **Keep the 15-minute give-up but skip the reaper while a rollout is
  stalled.** The old pod is out of its Service either way, so the site
  is still down.
- **Fail `/readyz` on an absent share.** That takes browsing down for a
  fault no new pod can fix.

## References

- ADR-0155, ADR-0156; B1531, B1538; V1531, V1532, V1538, V1539.
- `crates/pharos-server/src/media_share.rs`, `self_restart.rs`, `health.rs`.
