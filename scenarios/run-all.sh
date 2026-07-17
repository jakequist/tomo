#!/usr/bin/env bash
# Run all Tomo e2e scenarios and report a summary.
#
#   ./scenarios/run-all.sh              # everything
#   ./scenarios/run-all.sh --quick      # Tier 1 only (01–04), no lag variants
#   ./scenarios/run-all.sh --lag 50ms   # re-run applicable scenarios with lag
#   ./scenarios/run-all.sh 07           # single scenario by number prefix
#
# Exit code: 0 iff no scenario FAILed (skips are allowed but reported).

set -uo pipefail
cd "$(dirname "$0")"

QUICK=0; LAG=""; ONLY=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --quick) QUICK=1 ;;
    --lag)   LAG="$2"; shift ;;
    *)       ONLY="$1" ;;
  esac
  shift
done

pass=0; failed=0; skipped=0; failures=()

for s in $(ls [0-9][0-9]_*.sh 2>/dev/null | sort); do
  num="${s%%_*}"
  [[ -n "$ONLY" && "$num" != "$ONLY" ]] && continue
  [[ "$QUICK" == 1 && "$num" -gt 04 ]] && continue

  printf '=== %s ===\n' "$s"
  TOMO_SCENARIO_LAG="$LAG" bash "$s"
  rc=$?
  case $rc in
    0)  pass=$((pass+1)) ;;
    77) skipped=$((skipped+1)) ;;
    *)  failed=$((failed+1)); failures+=("$s") ;;
  esac
done

printf '\n==== summary: %d passed, %d failed, %d skipped ====\n' \
  "$pass" "$failed" "$skipped"
for f in "${failures[@]:-}"; do [[ -n "$f" ]] && printf 'FAILED: %s\n' "$f"; done
[[ $failed -eq 0 ]]
