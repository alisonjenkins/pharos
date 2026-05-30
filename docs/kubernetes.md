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

The chart is single-replica by design: SQLite is single-writer and the
in-process caches + transcode scheduler are not HA-safe. Use an external
Postgres before considering scale-out (and note the app is not yet horizontally
scalable regardless).

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

## Database: SQLite vs Postgres

Default is SQLite on the db PVC. Switch to Postgres by setting
`config.database.url` to a `postgres://…` URL (the db PVC is then not created):

```sh
helm install pharos charts/pharos -n pharos \
  --set config.database.url='postgres://pharos:pw@postgres:5432/pharos'
```

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
- Prometheus metrics `GET /metrics`. Enable a ServiceMonitor:
  `--set serviceMonitor.enabled=true --set serviceMonitor.labels.release=<prom-release>`.
- OTLP traces: `--set config.obs.otlpEndpoint=http://otel-collector:4317`.

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
`gpu`, `resources`, `*SecurityContext`, `nodeSelector`/`tolerations`/`affinity`.
