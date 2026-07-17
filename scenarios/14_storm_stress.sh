#!/usr/bin/env bash
# Scenario 14 — Unthrottled storm stress (Tier 2, post-M6)
# Spec: docs/NOTES.md "Storm cluster" remaining item #2 — a permanent
# unthrottled-storm stress test. Where scenario 06 *paces* its hot loop to keep
# the burst bounded, this one does NOT: a tight `printf 'v%d' > hot.txt` loop
# with no sleep at all, for 4 s, producing many thousands of rewrites as fast as
# the shell can issue them. This is the exact repro that once fabricated 1,440
# phantom conflicts per storm (watcher-thread hashing racing the session's own
# applies). It must now:
#   - stay responsive DURING the storm (`tomo status` < 2 s), and
#   - converge (equal roots + byte-identical hot.txt) within 60 s of storm end,
#   - with ZERO conflict rows on BOTH sides,
#   - the hot file coalesced to < 50 versions (bounded history under the flood),
#   - and history DB integrity green on both sides.
# Phase 2 repeats the storm while a 20 MiB file transfers, so the chunked
# large-file path runs concurrently with the hot-file flood (head-of-line and
# apply-vs-watch races under maximum pressure).
#
# No pacing here is deliberate: the whole point is bounded convergence under an
# unbounded write rate. Every assertion still polls (wait_for) — the only
# non-poll wait is the fixed 4 s storm-generation window itself.

source "$(dirname "$0")/lib/harness.sh"
scenario_init
require_cli
[[ "${TOMO_LINK_MODE:-local}" == "ssh" ]] && ensure_self_ssh

# Storm scenario: network lag is a separate axis (covered by 05). Refuse to run
# under injected lag rather than assert against a moving target.
[[ -n "${TOMO_SCENARIO_LAG:-}" ]] && skip "14 is a storm scenario; lag variants are covered by 05"

A="$(make_machine a)"
B="$(make_machine b)"

link_machines "$A" "$B" >/dev/null

HOT="$A/hot.txt"
STORM_SECS=4

# conflict_rows DIR → number of recorded conflict rows (all, resolved or not);
# 0 when there is no store or none recorded. Real CLI only.
conflict_rows() {
  local n
  n="$( ( cd "$1" && "$TOMO_BIN" conflicts list --all --json 2>/dev/null ) | jq 'length' 2>/dev/null )"
  printf '%s\n' "${n:-0}"
}

# hot_final_versioned DIR — the newest recorded version of hot.txt equals the
# current on-disk final bytes (drains the debounced final flush deterministically).
hot_final_versioned() {
  local dir="$1" newest
  newest="$( hist_json "$dir" hot.txt | jq -r '.[0].id // empty' )"
  [[ -n "$newest" ]] || return 1
  ( cd "$dir" && "$TOMO_BIN" restore hot.txt --version "$newest" --stdout 2>/dev/null ) \
    | cmp -s - "$HOT"
}

# run_storm LABEL — hammer hot.txt with an UNTHROTTLED tight loop for STORM_SECS,
# sampling status latency the whole time. Records the write count to
# $WORK/writes.LABEL and the max status latency to $WORK/lat.LABEL.
run_storm() {
  local label="$1"
  (
    c=0
    end=$(( $(date +%s) + STORM_SECS ))
    while (( $(date +%s) < end )); do
      printf 'v%d' $((++c)) > "$HOT"    # no pacing — as fast as the shell allows
    done
    printf '%s\n' "$c" > "$WORK/writes.$label"
  ) &
  local storm_pid=$!

  local max_lat_ms=0 t0 t1 lat
  while kill -0 "$storm_pid" 2>/dev/null; do
    t0=$(date +%s%N)
    ( cd "$A" && "$TOMO_BIN" status --json >/dev/null 2>&1 )
    t1=$(date +%s%N)
    lat=$(( (t1 - t0) / 1000000 ))
    (( lat > max_lat_ms )) && max_lat_ms=$lat
    (( lat < 2000 )) || fail "$label: tomo status took ${lat}ms during storm (>= 2s): not responsive"
  done
  wait "$storm_pid"
  printf '%s\n' "$max_lat_ms" > "$WORK/lat.$label"
}

# assert_storm_outcome LABEL — after a storm, converge and assert the invariants.
assert_storm_outcome() {
  local label="$1" writes final max_lat versions ca cb
  writes="$(cat "$WORK/writes.$label")"
  max_lat="$(cat "$WORK/lat.$label")"
  final="$(cat "$HOT")"
  log "$label: $writes unthrottled hot writes, max status latency ${max_lat}ms"

  (( writes >= 1000 )) || fail "$label: only $writes writes (< 1000) — not a real storm"
  (( max_lat < 2000 )) || fail "$label: status latency peaked at ${max_lat}ms (>= 2s)"

  # Converge within 60 s of storm end: final bytes reach B, roots equal.
  wait_for 60 "$label: final hot.txt reaches B" assert_file_content "$B/hot.txt" "$final"
  wait_for 60 "$label: index roots converge" roots_equal "$A" "$B"
  wait_for 30 "$label: final hot.txt versioned on A" hot_final_versioned "$A"

  # Coalescing: the flood collapses to few versions (bounded history).
  versions="$(hist_count "$A" hot.txt)"
  log "$label: hot.txt coalesced $writes writes into $versions versions"
  (( versions >= 1 ))  || fail "$label: hot.txt has no versions (final state lost!)"
  (( versions < 50 ))  || fail "$label: hot.txt has $versions versions (>= 50): storm not coalesced"

  # ZERO conflict rows on both sides (the phantom-conflict regression guard).
  ca="$(conflict_rows "$A")"; cb="$(conflict_rows "$B")"
  (( ca == 0 )) || fail "$label: A recorded $ca conflict row(s) under the storm (expected 0)"
  (( cb == 0 )) || fail "$label: B recorded $cb conflict row(s) under the storm (expected 0)"

  # History integrity green both sides.
  db_check_ok "$A" || fail "$label: db check failed on A"
  db_check_ok "$B" || fail "$label: db check failed on B"

  # Full convergence + quiet network.
  wait_for 30 "$label: converged and settled" converged_and_settled "$A" "$B"
  assert_quiet_network "$A" 3
  assert_converged "$A" "$B"
}

# ---- Phase 1: bare unthrottled storm -------------------------------------
log "phase 1: unthrottled hot-file storm (${STORM_SECS}s, no pacing)"
run_storm p1
assert_storm_outcome p1

# ---- Phase 2: same storm while a 20 MiB file transfers -------------------
# The 20 MiB file is far above the inline threshold, so it ships via the chunked
# manifest/pull path — exercised here CONCURRENTLY with the hot-file flood.
log "phase 2: unthrottled storm overlapping a 20 MiB chunked transfer"
SCRATCH="$WORK/scratch"; mkdir -p "$SCRATCH"
dd if=/dev/urandom of="$SCRATCH/big.bin" bs=1M count=20 status=none
mv "$SCRATCH/big.bin" "$A/big.bin"     # atomic: A presents it whole
run_storm p2
# The 20 MiB file arrives byte-identical (chunked path survived the storm).
wait_for 60 "20 MiB file arrives byte-identical" assert_same_content "$A/big.bin" "$B/big.bin"
assert_storm_outcome p2

pass
