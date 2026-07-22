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

# Reap stale processes from PREVIOUS, abruptly-terminated runs: a killed runner
# skips scenario teardown, and a SIGSTOPped serve (partition scenarios) is
# immune to stdin EOF — 87 frozen serves once exhausted the kernel's inotify
# instances and failed unrelated scenarios with "Too many open files". Scoped
# strictly to processes whose CWD is inside a scenario tmpdir, so real sessions
# elsewhere on the machine are never touched.
for pid in $(pgrep -x tomo 2>/dev/null); do
  case "$(readlink "/proc/$pid/cwd" 2>/dev/null)" in
    /tmp/tomo-scenario-*)
      kill -CONT "$pid" 2>/dev/null
      kill -9 "$pid" 2>/dev/null
      ;;
  esac
done

pass=0; failed=0; skipped=0; failures=()

for s in $(ls [0-9][0-9]_*.sh 2>/dev/null | sort); do
  num="${s%%_*}"
  [[ -n "$ONLY" && "$num" != "$ONLY" ]] && continue
  # Force base-10: scenario prefixes 08/09 are not valid octal and would abort
  # the arithmetic comparison ("value too great for base 8").
  [[ "$QUICK" == 1 && "10#$num" -gt 4 ]] && continue

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
