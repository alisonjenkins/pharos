# Running pharos on Kubernetes (Helm chart + Tilt dev loop)

Pharos ships a Helm chart (`charts/pharos`) and a Tilt inner-loop (`Tiltfile`)
for local development against a throwaway [kind](https://kind.sigs.k8s.io)
cluster. The container image is the reproducible nix-built distroless OCI
(`nix build .#oci` → `pharos:latest`) which bundles the server, ffmpeg, and the
crash-isolated `transcode-worker` (so HLS transcoding uses the worker pool, not
the inline-ffmpeg fallback).

## Prerequisites

All tooling is in the nix devShell (`nix develop`): `helm`, `kind`, `kubectl`,
`tilt`, `ctlptl`, plus `docker` (the daemon must be running and your shell must
be in the `docker` group — `id` should list `docker`; if you were just added,
log out/in or reboot).

`ctlptl` (`ctlptl-cluster.yaml`) stands up the kind cluster **wired to a local
OCI registry** (`localhost:5005`): nix builds each image, Tilt pushes it to the
registry, and the kind node pulls from there. No `kind load`, no docker.io.

## Tilt dev loop

```sh
just kind-up      # ctlptl: create the kind cluster `pharos` + local registry (idempotent)
just tilt-up      # kind-up, then `tilt up`
# … iterate; edit Rust/flake/chart → Tilt rebuilds the image + redeploys …
just tilt-down            # tilt down (keeps the cluster + registry)
just tilt-down delete=1   # tilt down + delete the kind cluster AND its registry
```

What `tilt up` does:
1. Builds `.#oci`, `.#jellyfinWebOci`, and `.#testMediaOci` via nix; pushes each
   to the local registry; the kind node pulls them (Tilt rewrites the refs).
2. Deploys `charts/pharos` with `values-dev.yaml` (ephemeral storage, UI on). A
   `mediaSeed` initContainer copies the CC test-media corpus into the media
   volume; the running server's poll tier then indexes it (~30s after boot).
3. Seeds the `playwright` admin user, then waits for the library to populate and
   reports the item count (`scripts/tilt-seed.sh`).
4. Port-forwards `127.0.0.1:8096` (API) and `127.0.0.1:8097` (jellyfin-web UI).

The jellyfin-web image is **nginx** serving the bundle under `/web/` and
reverse-proxying every other path (the REST API + websocket) to the pharos
service (`$PHAROS_URL`, default the in-cluster `pharos:8096`). So the browser
sees one same-origin server — open `http://127.0.0.1:8097/` and it auto-connects
to pharos (no manual "add server" step), mirroring the compat suite's
`http-server --proxy` fixture.

Verify: `curl 127.0.0.1:8096/healthz` → `ok`; `curl
127.0.0.1:8096/System/Info/Public`; `curl 127.0.0.1:8097/System/Info/Public`
returns the same server info (proving the UI's same-origin proxy); open
`http://127.0.0.1:8097/` and it connects to pharos directly.

## Standalone install (any cluster)

```sh
just helm-lint
helm install pharos charts/pharos -n pharos --create-namespace \
  --set persistence.media.type=existingClaim \
  --set persistence.media.existingClaim=my-media-pvc
```

Before the first install, check these. Each one is a fault seen in a real
deployment:

1. **Media over NFS?** Give the export a fixed `fsid=` (see
   [NFS exports and stale handles](#nfs-exports-and-stale-handles)).
2. **Keep `selfHeal` on** (the default) so a pod whose mount goes bad is
   replaced, not left out of its Service (see [Self-healing](#self-healing)).
   It needs RBAC to patch pods and Deployments in the release's namespace.
3. **Running the aligner?** Its readiness probe reads the media root, so the
   media volume must be mounted and readable before it reports ready.

Replicas + deploy strategy depend on the database backend (ADR-0015):

- **SQLite** (default): single replica, and the chart **forces `Recreate`**
  regardless of `strategy.type` — SQLite is single-writer on an RWO PVC, so
  two pods must never overlap.
- **Postgres**: `strategy.type=RollingUpdate` with surge gives zero-downtime
  deploys (a second pod briefly overlaps the draining one). Multi-replica
  overlap is safe: SyncPlay groups are persisted and owned per-group via
  Postgres advisory locks with cross-replica command forwarding (ADR-0016).
  Steady-state `replicaCount` stays 1 on a single-node cluster — a second
  permanent replica buys no HA there; the value is the deploy overlap window.

## Storage

| Volume | Mount | Default | Notes |
|--------|-------|---------|-------|
| db     | `/var/lib/pharos/db` | PVC `…-db` (1Gi, RWO) | Only when `config.database.url` is SQLite. Omitted for Postgres. |
| cache  | `/var/lib/pharos/cache` | PVC `…-cache` (5Gi) | Image/trickplay/transcode caches. `persistence.cache.enabled=false` → emptyDir. |
| media  | `/var/lib/pharos/media` (ro) | `persistence.media.type` | `existingClaim` \| `pvc` \| `hostPath` \| `nfs` \| `emptyDir`. |

**Media** is yours to provide. Typical home setups point at an NFS share:

```yaml
persistence:
  media:
    type: nfs
    nfs: { server: 10.0.0.5, path: /export/media }
```

### NFS exports and stale handles

An NFS client names each export by a file handle the server hands out. The
server derives that handle from the exported filesystem's id. When the id
changes, every handle a client holds goes stale: each `stat` answers
`ESTALE` ("Stale file handle") until the share is mounted again.

The id can change under a running cluster when the server:

- re-exports after a reboot, and the filesystem has no stable UUID;
- exports a FUSE or union filesystem (mergerfs, for example), whose id can
  change each time it is mounted;
- moves the export to another disk or pool.

In Kubernetes the mount belongs to the pod, not the container. Restarting the
container keeps the stale mount. Only a new pod mounts the share again.

**Fix it on the server.** Give each export a fixed `fsid=` in
`/etc/exports`, so the handle stays the same across remounts:

```
/export/media  10.0.0.0/24(ro,fsid=1,no_subtree_check)
```

Use a different number for each export. For a FUSE export, check its own docs
for NFS settings too; mergerfs documents options for stable inode numbers.

**Then keep [`selfHeal`](#self-healing) on** as the safety net for anything
`fsid=` does not cover.

## Self-healing

Some faults belong to the pod rather than the process. A stale NFS mount is
one. Kubernetes probes do not replace a pod:

- a failing **readiness** probe takes the pod out of its Service;
- a failing **liveness** probe restarts the container in the same pod.

So the chart splits the work:

| Piece | What it does |
|-------|--------------|
| Aligner `GET /readyz` | Stats the media root and reads one entry, on its own thread with a 2 s limit. Answers 503 with the error when either fails. |
| Aligner readiness probe | Points at `/readyz`. Liveness stays on `/healthz`, because a restart would not help. |
| Server `GET /readyz` | Lists each media root on its own thread with a 2 s limit. From the first media read that hits a stale handle, lost connection or I/O error, the share counts as broken until the pod is replaced, and the cause shows under `media`. A missing or refused library folder does not fail it, so one deleted folder cannot take the whole server out of its Service. |
| Server self-restart | When the share breaks, the server rolls out its own Deployment straight away and stays Ready (`media_restart: requested`) until the new pod is Ready, so browsing keeps working while only media answers 503. It falls back to failing `/readyz` for the reaper only when it cannot ask for the restart (see below). |
| Server media routes | A file the share cannot read answers 503, not 404: the file is not missing, this pod cannot reach it. |
| `selfHeal` CronJob | Every 2 minutes, finds this release's Deployments labelled `pharos.io/self-heal=true` whose pod has been NotReady for `selfHeal.notReadyMinutes` (default 5) after starting, and rolls each one out again, as `kubectl rollout restart` does. The new pod mounts the share afresh. |

It is on by default. To tune it, or to turn it off where the namespace may
not grant a Role that patches pods and Deployments:

```yaml
selfHeal:
  enabled: true            # false renders no CronJob, Role or ServiceAccount
  notReadyMinutes: 5       # long enough for a short NFS server reboot to pass
  serverSelfRestart: true  # the server restarts its own Deployment (ADR-0156)
```

### The server restarts itself

The server does not wait for the reaper (ADR-0156, ADR-0158). Its pod runs as the
service account `<fullname>-self-restart`, whose Role may only `get`
and `patch` the server's own Deployment. On the first share fault, the
server sets that Deployment's `kubectl.kubernetes.io/restartedAt`, and
`/readyz` keeps answering 200 so browsing, sign-in and search stay up:

- With `RollingUpdate` (Postgres, `strategy.type: RollingUpdate`), the old
  pod serves everything but media until the new pod is Ready. Browsing
  never goes down as long as `strategy.rollingUpdate.maxUnavailable` is 0
  and `maxSurge` is above 0, the chart's defaults. Other values let the
  Deployment stop the old pod first. The old pod stays Ready however long
  the new one takes, so a new pod that cannot mount the share (the storage
  host down) leaves browsing up, not the site down.
- With `Recreate` (SQLite), the old pod stops as soon as the restart lands,
  so Pharos is down for one restart instead of the reaper's wait.

`/readyz` fails as before in each of these cases. The reaper then heals
the pod, but only when `selfHeal.enabled` is on and the pod belongs to a
Deployment labelled `pharos.io/self-heal=true` (the chart labels its own),
which the reaper finds pods through by selector. Otherwise the pod stays
unready until someone restarts it:

- `serverSelfRestart` is off, or the server runs outside Kubernetes;
- the API call fails, for example when the Role is missing;
- the Deployment is paused.

A Deployment restarted in the last 10 minutes is not restarted again at
once. The pod stays Ready (`media_restart: waiting`) and asks again when
the 10 minutes are up.

### An empty share

When the storage host loses its disks, it can keep exporting the bare,
empty mountpoint. Every media root then lists with no entries, or is
missing. The server calls the share absent (B1538):

- `/readyz` stays 200, with `the media share is absent` under `media`.
- `pharos_media_share_ok` is 0.
- Each video or song answers 503 `media share absent`, not 404.
- Nothing restarts, because a fresh mount would be just as empty.
- It logs `media share absent` once when this starts and
  `media share back` once a root holds entries again.

The library scan does not delete items while this lasts. A bare root, one
with no file of any kind and no folder left out for `.ignore`, keeps every
row. A root that still holds files, or whose media went under a `.ignore`
folder, is swept as usual, and the sweep guard there skips any pass that
would remove more than a quarter of a root (once that is at least 100
items).

Each request logs one `self-restart:` line with the Deployment, the outcome
and the cause, and counts
`pharos_media_share_restart_total{outcome="restarted"|"recently_restarted"|"paused"|"failed"}`.

#### Rolling back to a chart without the self-restart

Charts from before the self-restart leave `serviceAccountName` out of the
server Deployment. Kubernetes fills a missing name back in from the old
one, so a `helm rollback` (or Flux upgrade remediation) to such a chart
deletes the `<fullname>-self-restart` ServiceAccount but leaves the
Deployment naming it (`<fullname>` is the chart's full name: `pharos`
for a release named `pharos`). No new server pod can start: its ReplicaSet reports
`FailedCreate` with `serviceaccount "…-self-restart" not found` (B1533).
Charts with V1533 always name the account, so a rollback between them is
safe.

If it happens, do one of these, then let the rollback or next upgrade run:

```bash
# Put the name back to default:
kubectl -n <ns> patch deployment <fullname> --type=json \
  -p '[{"op":"replace","path":"/spec/template/spec/serviceAccountName","value":"default"},
       {"op":"replace","path":"/spec/template/spec/serviceAccount","value":"default"}]'

# Or recreate the account so a later Helm upgrade adopts it:
kubectl -n <ns> create serviceaccount <fullname>-self-restart
kubectl -n <ns> label sa <fullname>-self-restart app.kubernetes.io/managed-by=Helm
kubectl -n <ns> annotate sa <fullname>-self-restart \
  meta.helm.sh/release-name=<release> meta.helm.sh/release-namespace=<ns>
```

The rollout follows the Deployment's own strategy. With `RollingUpdate`, the
new pod is created alongside the old one where `maxSurge` allows, but the old
pod may still go before the new one is Ready: it is NotReady, so the
Deployment controller counts it unavailable and may scale it down at once.
Once the routing update reaches the Service proxy, new connections normally
stop going to the NotReady pod. Existing (kept-alive) connections, and
clients that reach the pod directly, can still carry requests to it and may
be cut off when it stops. With
`Recreate`, the old pod always stops first. The chart uses `Recreate` for
the server on SQLite (its database volume is ReadWriteOnce and
single-writer) and for an aligner with a GPU or a cache volume; otherwise
the server follows `strategy.type`. After a rollout, the reaper leaves that Deployment alone for
`notReadyMinutes`, so its new pod gets the whole limit to become Ready.

The CronJob runs with a namespace-scoped Role that can only get, list and
patch pods and Deployments. It deletes nothing. It skips pods still starting (the aligner's startup probe can
take minutes on a cold GPU cache) and pods already terminating. A pod's Ready
condition is False from before its startup probe passed, so the reaper does
not trust its age: the first run that sees a started pod NotReady stamps it
with the annotation `pharos.io/not-ready-since`, and the count runs from that
stamp. A container replaced since the stamp (it records each container's
ID), or a Ready change after it, starts the count again.

So, from the fault to the rollout, with the defaults:

| Step | Takes |
|------|-------|
| Readiness probe fails enough times to mark the pod NotReady | up to 30 s (aligner: 3 × 10 s) |
| The next reaper run stamps it | up to 2 min |
| The stamp ages past `notReadyMinutes` | 5 min |
| The next reaper run rolls out its Deployment | up to 2 min |

That is about 5 to 10 minutes, then the new pod's own startup. Each stamp,
each rollout and each Deployment left to settle logs one line:

```
self-heal: watching pod=<name> not_ready_since=<unix seconds> containers=<ids>
self-heal: restarting deployment=<name> pod=<name> not_ready_s=<seconds> limit_s=<seconds>
self-heal: waiting deployment=<name> restarted_s=<seconds>
```

The image is any one with `bash`, `jq` and `kubectl` (`selfHeal.image`).

**Signals to watch:**

- Aligner log line `media root unreadable`, with the error.
- Aligner log line `song file not found`, with the path. This repeating for
  files that exist means the mount is bad.
- Gauge `pharos_aligner_media_ok`: 1 when the last readiness check read the
  media root, 0 when it did not. The chart's ServiceMonitor scrapes only the
  server, so add a scrape for the aligner's `/metrics` to use it.
- Server log line `readyz: media share unreadable` and the one-off
  `media share broken; the pod needs a fresh mount`, each with the cause.
- Gauge `pharos_media_share_ok` on the server: 0 while `/readyz` finds the
  share unusable, even while the pod stays Ready for its own restart.
- Counter `pharos_media_share_restart_total{outcome}` and the log lines
  `self-restart: asked for a rollout restart; staying ready until replaced`
  or `self-restart: … going unready for the reaper`.
- Counter `pharos_source_unreadable_total{reason="stale_handle"}` (or
  `not_connected`, `io_error`) on the server, per media read that hit a share fault.
- `kube_pod_status_ready{condition="false"}` from kube-state-metrics, for the
  pod itself.

## Database: SQLite vs Postgres

Default is SQLite on the db PVC. Switch to Postgres by setting
`config.database.url` to a `postgres://…` URL (the db PVC is then not created):

```sh
helm install pharos charts/pharos -n pharos \
  --set config.database.url='postgres://pharos:pw@postgres:5432/pharos'
```

The reference deployment (home k3s) runs Postgres provisioned by
[CloudNativePG](https://cloudnative-pg.io/) with `RollingUpdate` for
zero-downtime deploys — see ADR-0015 for the migration story and why the old
sqlite PVC is retained as a rollback hatch.

## Library scan

`pharos serve` watches roots and periodically rescans
(`libraryPollIntervalSecs`, default 300s) but **skips the first poll tick**, so a
fresh deploy is empty until the first interval elapses. The chart can also run
`pharos scan` as an **initContainer** before `serve` (`scan.initContainer`,
default `false`) to populate the library on first boot — same pod + db volume,
so it's SQLite-safe (sequential, no concurrent writer). A separate `scan.cron`
CronJob is available but should only be enabled with Postgres or when you disable
the in-process poll (`libraryPollIntervalSecs=0`); a concurrent scan pod would
contend on the SQLite lock.

### kind dev caveat

`values-dev.yaml` sets `scan.initContainer=false` and a short
`libraryPollIntervalSecs` (30s) instead of the boot scan. The cold one-shot
`scan` **process** cannot establish its SQLite connection pool under kind's
containerd runtime (it times out on connection acquire — the same release binary
populates fine locally and on a real cluster). The long-running server does not
hit this: its pool is already warm from the `/readyz` store probe, so the poll
tier scans normally. On a production cluster (real PVC, not kind's overlayfs)
`scan.initContainer=true` works as intended.

## Observability

- Liveness `GET /healthz`, readiness `GET /readyz` (probes preconfigured).
  The aligner has both too; its `/readyz` also reads the media root (see
  [Self-healing](#self-healing)).
- Prometheus metrics `GET /metrics`. Enable a ServiceMonitor:
  `--set serviceMonitor.enabled=true --set serviceMonitor.labels.release=<prom-release>`.
- OTLP traces: set `config.obs.otlpEndpoint` to an OTLP/gRPC collector
  (e.g. `http://tempo:4317`). pharos exports spans via a batch processor
  (`pharos_server::obs`); when the endpoint is unset, no exporter is installed.

### Dev observability stack (Tilt)

`just tilt-up` also brings up a throwaway Grafana stack (`deploy/obs-dev.yaml`,
dev-only — not part of the chart): **Tempo** (traces, fed by pharos OTLP →
`http://tempo:4317`), **Prometheus** (scrapes pharos `/metrics`), **Loki** +
**Promtail** (pharos's JSON stdout logs), and **Grafana** with all three wired
as datasources (anonymous admin). Open `http://127.0.0.1:3000` — Explore →
Tempo for traces, Prometheus for metrics, Loki (`{namespace="pharos"}`) for
logs. Storage is ephemeral (emptyDir); images pull from docker.io.

## Ingress

```yaml
ingress:
  enabled: true
  className: traefik
  annotations:
    cert-manager.io/cluster-issuer: letsencrypt
  hosts:
    - host: pharos.example.com
      paths: [{ path: /, pathType: Prefix }]
  tls:
    - secretName: pharos-tls
      hosts: [pharos.example.com]
```

## Hardware transcoding (GPU)

- **VAAPI**: `--set config.server.hwaccel=VAAPI --set gpu.vaapi.enabled=true`
  mounts `/dev/dri` (schedule onto a node with the GPU via `nodeSelector`).
- **NVENC**: `--set config.server.hwaccel=NVENC --set gpu.nvidia.enabled=true`
  sets `runtimeClassName: nvidia`; also request the device via
  `--set resources.limits.'nvidia\.com/gpu'=1` and ensure the NVIDIA device
  plugin is installed.

## Values reference

`config.*` maps 1:1 to `config.toml` (`[server]`/`[obs]`/`[media]`/`[database]`)
— see `charts/pharos/values.yaml` for the full annotated list. Infra values:
`image`, `persistence`, `service`, `ingress`, `serviceMonitor`, `scan`, `ui`,
`gpu`, `resources`, `*SecurityContext`, `nodeSelector`/`tolerations`/`affinity`,
`aligner`, `selfHeal`.
