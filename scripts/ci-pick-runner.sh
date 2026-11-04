#!/usr/bin/env bash
# Pick the runner label for the heavy CI jobs; prints it on stdout.
#
#   scripts/ci-pick-runner.sh
#
# GitHub cannot overflow a queued job from one runner label to another, so the
# `changes` job decides up front: when at least BURST_QUEUE_THRESHOLD jobs in
# this repo are already waiting for the home label, send this run to the
# Hetzner burst label instead. Every failure path prints the home label.
#
# Environment:
#   BURST_ENABLED          must be exactly "true", else the home label (default off)
#   BURST_QUEUE_THRESHOLD  queued-job count that triggers the burst label (default 12)
#   IS_FORK_PR             "true" for a fork PR: never leaves the home label
#   GITHUB_REPOSITORY      owner/repo (set by Actions)
#   GH_TOKEN               token with actions:read (set by the workflow)
set -euo pipefail

HOME_LABEL=pharos-nix-builder-amd64
BURST_LABEL=pharos-hetzner-builder-amd64

log() { printf 'ci-pick-runner: %s\n' "$*" >&2; }

# Number of jobs queued for the home label, across runs that are queued or
# running (a running run can still have jobs waiting for a slot).
count_queued() {
  local status runs run total=0 n
  for status in queued in_progress; do
    runs="$(gh api --paginate "repos/${GITHUB_REPOSITORY}/actions/runs?status=${status}&per_page=100" \
      --jq '.workflow_runs[].id')" || return 1
    for run in $runs; do
      n="$(gh api --paginate "repos/${GITHUB_REPOSITORY}/actions/runs/${run}/jobs?per_page=100" \
        --jq ".jobs[] | select(.status == \"queued\" and (.labels | index(\"${HOME_LABEL}\"))) | .id")" || return 1
      if [[ -n "$n" ]]; then
        total=$((total + $(wc -l <<<"$n")))
      fi
    done
  done
  echo "$total"
}

pick() {
  if [[ "${BURST_ENABLED:-}" != "true" ]]; then
    log "burst disabled (BURST_ENABLED != true)"
    echo "$HOME_LABEL"; return
  fi
  if [[ "${IS_FORK_PR:-}" == "true" ]]; then
    log "fork PR stays on the home label"
    echo "$HOME_LABEL"; return
  fi
  local threshold="${BURST_QUEUE_THRESHOLD:-12}"
  if [[ ! "$threshold" =~ ^[0-9]+$ ]]; then
    log "BURST_QUEUE_THRESHOLD='${threshold}' is not a number, using 12"
    threshold=12
  fi
  local queued
  if ! queued="$(count_queued)" || [[ ! "$queued" =~ ^[0-9]+$ ]]; then
    log "could not count the queue, falling back to the home label"
    echo "$HOME_LABEL"; return
  fi
  log "queued=${queued} threshold=${threshold}"
  if ((queued >= threshold)); then
    echo "$BURST_LABEL"
  else
    echo "$HOME_LABEL"
  fi
}

# `|| echo` is the fail-safe: no error path may leave the output empty.
pick || echo "$HOME_LABEL"
