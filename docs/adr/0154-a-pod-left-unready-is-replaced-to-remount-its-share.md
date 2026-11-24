# ADR-0154: A pod left unready is replaced, so its media share is mounted afresh

- **Status:** Accepted
- **Date:** 2026-11-24T12:30:00Z
- **Deciders:** Alison Jenkins

## Context

The aligner mounts the media share over NFS. On 2026-11-23 the share's file
handles went stale under the running aligner pod. Every lookup then failed
with `ESTALE`, and the aligner answered every song "404 file not found" for
about a day. Nothing logged it, and karaoke fell back to guessed word times.

A pod's volumes belong to the pod, not to its container. A liveness restart
reuses the same stale mount. A failing readiness probe only takes the pod
out of its Service. Only a new pod mounts the share again. Recreating the pod
by hand fixed it.

## Decision

We will detect a broken share in the aligner and replace the pod
automatically.

- The aligner's `GET /readyz` answers 503 when either:
  - its media root cannot be listed within 2 s; or
  - a song's file failed to read with a stale handle, I/O error or lost
    connection. This stays set until the process ends.
- `/healthz` and the liveness probe never touch the mount, so a stale share
  does not cause a restart loop.
- The chart points the aligner's readiness probe at `/readyz`.
- The chart's `selfHeal` CronJob runs every 2 minutes. It deletes this
  release's pods labelled `pharos.io/self-heal=true` that meet all of these:
  - Running;
  - every container started;
  - not terminating;
  - NotReady for longer than `selfHeal.notReadyMinutes` (default 5),
    counted from the first run that saw it started and NotReady. That run
    stamps the pod with the annotation `pharos.io/not-ready-since`, because
    a new pod's Ready condition dates from before its startup probe passed.

  Their Deployment then starts a new pod, which mounts the share again.
- `selfHeal` is on by default, so a new install heals without extra steps.
  An install whose namespace refuses a Role that deletes pods sets
  `selfHeal.enabled: false`.
- The CronJob runs under a ServiceAccount whose Role may only get, list,
  patch (for the stamp) and delete pods in the release's namespace. It uses a small image with bash,
  jq and kubectl.

## Consequences

- A stale mount now heals in about 5 to 7 minutes, with no one involved.
- `pharos_aligner_media_ok` and `pharos_aligner_source_unreadable_total`
  show the fault, and the CronJob logs each pod it deletes.
- The CronJob can delete pods. Its Role is limited to one namespace, and its
  selector to pods that opt in by label.
- A share that stays broken makes the pods come back and stay unready, so
  they get replaced every few minutes. That is visible, and costs a model
  load each time.
- The root cause (the NFS server changing its file handles, for example an
  export without a fixed `fsid=`) is outside Pharos. `docs/kubernetes.md`
  covers it.

## Alternatives considered

- **Liveness probe on the mount.** It restarts the container onto the same
  stale mount, so it would loop and never heal.
- **The aligner deletes its own pod.** It would need an HTTP client and API
  credentials inside the aligner, and a pod able to delete pods. Keeping that
  power in a separate, small CronJob is easier to audit.
- **A `pharos admin` subcommand as the reaper.** It avoids a third-party
  image, but runs the whole server image for a list-and-delete. The script is
  covered by `scripts/test-chart-self-heal.sh` against a fake kubectl.

## References

- B1530, V1530.
- `crates/pharos-align/src/media.rs`, `charts/pharos/templates/self-heal.yaml`,
  `docs/kubernetes.md`.
