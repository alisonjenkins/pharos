#!/usr/bin/env bash
# Run only the tests a change can affect.
#
#   scripts/test-changed.sh [--plan] [BASE]
#
# BASE defaults to origin/main (falling back to main). The change set is
# everything since the merge-base with BASE, plus staged, unstaged and
# untracked files, so a stale local main never drags unrelated work in.
#
# Each changed path is classified, and the verdict for every path is printed
# with its reason, so "why did/didn't this run?" is never a guess:
#   crates/<pkg>/...         -> rdeps(<pkg>): the crate and everything that
#                               depends on it, transitively
#   workspace-wide inputs    -> full workspace (Cargo.toml, Cargo.lock,
#                               workspace-hack/, toolchain, flake, nextest config)
#   docs / specs / deploy    -> nothing to test
#   anything else            -> full workspace (unknown = assume the worst)
# A change under crates/pharos-store-sqlx also runs `just test-postgres`,
# because plain nextest skips the postgres arms (see CLAUDE.md).
#
# --plan prints the decision and exits without running anything.
#
# CI's `nix flake check` still runs the whole suite on every PR; this is the
# local gate, not the merge gate.
set -euo pipefail

plan=0
if [[ "${1:-}" == "--plan" ]]; then plan=1; shift; fi

base="${1:-}"
if [[ -z "$base" ]]; then
  if git rev-parse --verify -q origin/main >/dev/null; then base=origin/main; else base=main; fi
fi
merge_base=$(git merge-base "$base" HEAD)

changed=$( { git diff --name-only "$merge_base"; \
             git diff --name-only --cached; \
             git ls-files --others --exclude-standard; } | sort -u | grep -v '^$' || true)

if [[ -z "$changed" ]]; then
  echo "test-changed: no changes since $base ($merge_base); nothing to run"
  exit 0
fi

full=""
pkgs=()
postgres=0

while IFS= read -r f; do
  case "$f" in
    crates/*/*)
      pkg=$(cut -d/ -f2 <<<"$f")
      verdict="rdeps($pkg)"
      pkgs+=("$pkg")
      [[ "$pkg" == "pharos-store-sqlx" ]] && postgres=1
      ;;
    Cargo.toml|Cargo.lock|workspace-hack/*|rust-toolchain.toml|flake.nix|flake.lock|.config/nextest.toml|.cargo/*)
      verdict="full (workspace-wide input)"
      full="${full:-$f}"
      ;;
    docs/*|specs/*|.specify/*|.claude/*|.github/*|charts/*|deploy/*|web-patches/*|compat-playwright/*|*.md|renovate.json|release-please-config.json|deny.toml|Tiltfile|ctlptl-cluster.yaml|config.example.toml|justfile|scripts/*)
      verdict="skip (not a test input)"
      ;;
    *)
      verdict="full (unclassified path)"
      full="${full:-$f}"
      ;;
  esac
  printf '  %-60s %s\n' "$f" "$verdict"
done <<<"$changed"

if [[ -n "$full" ]]; then
  echo "test-changed: decision=full reason=$full"
  cmd=(nix develop --command cargo nextest run --workspace)
elif (( ${#pkgs[@]} == 0 )); then
  echo "test-changed: decision=none reason=no test inputs changed"
  exit 0
else
  expr=$(printf '%s\n' "${pkgs[@]}" | sort -u | sed 's/^/rdeps(/; s/$/)/' | paste -sd '+' -)
  echo "test-changed: decision=selective filter=$expr"
  cmd=(nix develop --command cargo nextest run --workspace -E "$expr")
fi
(( postgres )) && echo "test-changed: postgres=yes reason=crates/pharos-store-sqlx changed"

if (( plan )); then exit 0; fi

xattr -dr com.apple.quarantine target/debug/ 2>/dev/null || true
"${cmd[@]}"
if (( postgres )); then just test-postgres; fi
