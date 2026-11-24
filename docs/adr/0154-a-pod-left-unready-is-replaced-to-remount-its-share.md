# ADR-0154: A pod left unready is rolled out again, so its media share is mounted afresh

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
- The chart's `selfHeal` CronJob runs every 2 minutes. For each of this
  release's Deployments labelled `pharos.io/self-heal=true`, it looks for a
  pod that meets all of these:
  - Running;
  - every container started;
  - not terminating;
  - NotReady for longer than `selfHeal.notReadyMinutes` (default 5),
    counted from the first run that saw it started and NotReady. That run
    stamps the pod with the annotation `pharos.io/not-ready-since`, because
    a new pod's Ready condition dates from before its startup probe passed.
    A container restart or a Ready change after the stamp restarts the count.

  It then sets the Deployment's `kubectl.kubernetes.io/restartedAt` pod
  template annotation, as `kubectl rollout restart` does. The Deployment
  rolls out a new pod by its own strategy, and the new pod mounts the share
  again. A Deployment rolled out within `notReadyMinutes` is left alone, so
  its new pod gets the whole limit to become Ready.
- `selfHeal` is on by default, so a new install heals without extra steps.
  An install whose namespace refuses a Role that patches pods and
  Deployments sets
  `selfHeal.enabled: false`.
- The CronJob runs under a ServiceAccount whose Role may only get, list and
  patch pods (for the stamp) and Deployments (for the rollout) in the
  release's namespace. It deletes nothing. It uses a small image with bash,
  jq and kubectl.

## Consequences

- A stale mount now heals with no one involved, about 5 to 10 minutes after
  the fault: up to 30 s for readiness to fail, up to one 2-minute run for
  the first stamp, `notReadyMinutes` (5), and up to one more run for the
  rollout; then the new pod starts.
- With `RollingUpdate`, the new pod is created alongside the old one where
  `maxSurge` allows. The old pod may still be removed before the new one is
  Ready: it is NotReady, so the Deployment controller counts it unavailable
  and may scale it down at once. A NotReady pod is already out of its
  Service, so no new requests reach it; requests already in flight on it,
  or a client connected to it directly, can still be cut off. With `Recreate` (the chart's choice for
  an aligner with a GPU or cache volume, and for the server on SQLite) the
  old pod always stops first.
- `pharos_aligner_media_ok` and `pharos_aligner_source_unreadable_total`
  show the fault, and the CronJob logs each Deployment it rolls out.
- The CronJob can restart Deployments. Its Role is limited to one namespace,
  and its selector to Deployments that opt in by label.
- A share that stays broken makes the new pod stay unready too, and the
  Deployment is rolled out again every 7 to 9 minutes until it reads.
- The root cause (the NFS server changing its file handles, for example an
  export without a fixed `fsid=`) is outside Pharos. `docs/kubernetes.md`
  covers it.

## Alternatives considered

- **Liveness probe on the mount.** It restarts the container onto the same
  stale mount, so it would loop and never heal.
- **Deleting the unready pod.** The first version did this. It bypasses the
  Deployment's strategy: even a `RollingUpdate` Deployment loses its only
  pod before a new one exists, and the cluster shows a deletion rather than
  a rollout. A restart through the Deployment keeps rollout history and
  surge where the strategy allows it, and needs no delete permission.
- **The aligner restarts its own Deployment.** It would need an HTTP client
  and API credentials inside the aligner. Keeping that power in a separate,
  small CronJob is easier to audit.
- **A `pharos admin` subcommand as the reaper.** It avoids a third-party
  image, but runs the whole server image for a list-and-patch. The script is
  covered by `scripts/test-chart-self-heal.sh` against a fake kubectl.

## References

- B1530, V1530.
- `crates/pharos-align/src/media.rs`, `charts/pharos/templates/self-heal.yaml`,
  `docs/kubernetes.md`.
