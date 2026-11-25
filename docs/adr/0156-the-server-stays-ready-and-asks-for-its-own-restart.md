# ADR-0156: The server stays Ready and asks for its own restart when its media share breaks

- **Status:** Accepted. Supersedes ADR-0155 in part: the server no longer goes unready as its first answer to a broken share.
- **Date:** 2026-11-24T18:30:00Z
- **Deciders:** Alison Jenkins

## Context

ADR-0155 fails the server's `/readyz` once its media share is broken, and
the `selfHeal` reaper then rolls out a fresh pod. While the pod is
NotReady, all of Pharos is out of its Service. That includes browsing,
sign-in, search and the API, none of which need the share. With the
defaults the outage lasts about 6 to 10 minutes plus the new pod's startup,
and a `RollingUpdate` strategy does not shorten it, because the old pod
already serves nothing.

The reaper has to wait because it sees only readiness. The server itself
knows the moment its share breaks.

## Decision

When its media share breaks, the server asks Kubernetes for its own
rollout restart straight away, and it stays Ready while that is pending.

- The chart gives the server pod a service account whose Role may only
  `get` and `patch` the server's own Deployment (`resourceNames`). It also
  sets `PHAROS_SELF_RESTART_DEPLOYMENT` and `POD_NAMESPACE`. This is on
  whenever `selfHeal` is, through `selfHeal.serverSelfRestart` (default
  `true`).
- On the first share fault in a process, the server reads its Deployment
  through the in-cluster API. It then sets the pod template's
  `kubectl.kubernetes.io/restartedAt`, as `kubectl rollout restart` does.
- Media routes still answer 503 for the broken share. Everything else keeps
  working.
- `/readyz` keeps answering 200 while all of these hold. Its body carries
  the cause under `media` and `media_restart`, and `pharos_media_share_ok`
  stays 0. `media_restart` is `requesting` while the API call is in flight,
  then `requested` once it succeeded.
  - The fault is a broken share: a read's stale handle, lost connection or
    I/O error, or a share-level error listing a root. A slow listing is not
    a broken share, and it still fails `/readyz`.
  - This process has seen the share whole.
  - The restart request succeeded, or is in flight, less than 15 minutes
    ago.
- In every other case `/readyz` fails as ADR-0155 decided, and the reaper
  heals the pod. That covers: no self-restart configured, the API call
  failing, a Deployment restarted in the last 10 minutes, a pod that never
  read its share, and a replacement that is not Ready within 15 minutes.
- With the self-restart on, a share-level error while listing a root now
  marks the share broken for the life of the process, as a failed read
  does. With it off, a listing error fails only that check, as before.
- Each request counts `pharos_media_share_restart_total{outcome}`, where
  `outcome` is `restarted`, `recently_restarted` or `failed`. Each request
  also logs a `self-restart:` line naming the Deployment, the outcome and
  the cause.

## Consequences

- With `RollingUpdate` (Postgres, `strategy.type: RollingUpdate`) and the
  chart's default `maxUnavailable: 0` and `maxSurge: 1`, browsing never
  goes down. A `maxUnavailable` above 0 or a `maxSurge` of 0 lets the
  Deployment stop the old pod first, which brings back an outage. The old pod
  keeps serving everything but media until the new pod is Ready, and the
  Deployment then retires the old pod. Media is down from the fault until
  then, which is about one pod startup.
- With `Recreate` (SQLite), the old pod stops as soon as the restart
  lands. All of Pharos is down for one restart, not for the reaper's
  5-minute wait first.
- The server's pod can now patch its own Deployment. The Role names only
  that Deployment, so no other object can be touched. RBAC cannot limit a
  verb to some fields, though: `patch` covers the whole Deployment (image,
  replicas, volumes), not just the restart annotation. A server that is
  taken over could change its own Deployment. Limiting the fields needs an
  admission policy, which is left to the cluster's operator.
- If the NAS is down, the new pod never sees its share whole. Its
  `/readyz` fails, so the rollout stalls and the old pod keeps serving
  browsing. After 15 minutes the old pod gives up and goes unready too, and
  the reaper retries as before.
- The 10-minute check on `restartedAt` stops a new pod whose share breaks
  at once from restarting the Deployment again in a loop. The reaper
  handles that pod instead.
- Outside Kubernetes, or with `serverSelfRestart` off, nothing changes
  from ADR-0155.

## Alternatives considered

- **Keep failing `/readyz` (ADR-0155 alone).** This is simpler and needs no
  API access from the server pod, but browsing is down for the whole heal.
- **Let the reaper run more often.** That shortens the wait but not the
  outage, because the pod is still NotReady until it is replaced.
- **Exit the process.** A container restart keeps the pod's stale mount
  (ADR-0154), so it would not fix the share.
