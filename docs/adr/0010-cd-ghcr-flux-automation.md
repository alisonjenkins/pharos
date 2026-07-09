# ADR-0010: CI on self-hosted builder → GHCR → Flux image automation

- **Status:** Accepted
- **Date:** 2026-07-09T00:00:00Z
- **Deciders:** Alison

## Context

The build needs the Nix toolchain + a warm store (ADR-0009); a cold hosted runner
would rebuild libav/ffmpeg every time. The deployment target is a home k3s
cluster running Flux (GitOps), alongside Jellyfin. Manually bumping the image tag
in the cluster repo after every push is friction that pulls deploys into the
development loop.

## Decision

- **CI runs on a self-hosted Nix builder** (GitHub ARC runner labelled
  `pharos-nix-builder-amd64`, a 16-vCPU VM) using `setup-host-nix` (host daemon +
  persistent store), not the DeterminateSystems installer.
- **Jobs:** `build + test + lint`, `supply-chain` (cargo-audit/deny), `package +
  oci image` (builds `.#oci` + uploads the tarball as a GH artifact — validates
  on PRs, publishes nothing), `publish images (ghcr)` (main only — pushes to
  GHCR via skopeo with a write-scoped PAT), and `jellyfin-web crawl` (Playwright).
  Image publish is isolated to the `publish` job so the GHCR secret is not
  exposed to build jobs.
- **Tags:** `latest`, the full commit SHA, and a **sortable
  `main-<commit-unix-ts>-<shortsha>`** tag. Flux's image automation orders tags
  numerically and a bare SHA doesn't sort; the timestamp gives a monotonic
  ordering while the short SHA stays traceable.
- **Auto-deploy:** Flux **image automation** (ImageRepository / ImagePolicy /
  ImageUpdateAutomation) watches GHCR, picks the newest `main-<ts>-<sha>` by the
  numeric timestamp, and commits the bump into the cluster repo's HelmRelease —
  so a `git push` to pharos `main` auto-builds and auto-deploys with no manual
  tag bump.

## Consequences

- Push-to-deploy: the deploy is out of the dev loop; iterate on code + tests and
  the running cluster follows.
- The `publish images (ghcr)` job is the only thing that must be green to
  deploy; unrelated red jobs (e.g. the `jellyfin-web crawl` E2E) do **not** block
  a publish. Watch the specific job, not the run-level status.
- GHCR package visibility is a UI-only setting (no REST/GraphQL mutation), so the
  public/private flip is a manual one-time action.
- The single-master k3s apiserver is occasionally flaky; long-running
  `kubectl exec`s can be reset mid-stream — prefer in-process server actions
  (e.g. `POST /Library/Refresh`) over exec for long operations.

## References

- `.github/workflows/ci.yml`; home-cluster `clusters/…/image-automation/pharos.yaml`
- memory `reference_pharos_ci_home_builder`, `project_pharos_home_cluster_deploy`,
  `reference_ghcr_visibility_no_api`
