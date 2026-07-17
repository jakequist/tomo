#!/usr/bin/env bash
# Scenario 06 — Adaptive degradation under storm (Tier 2, M3)
# Spec: docs/TESTING.md row 06 + SPEC §6.2. A synthetic storm (a hot file
# rewritten thousands of times, plus a spray of ~200 new files) must NOT be
# allowed to sacrifice sync latency or lose the final state:
#   - the process stays responsive DURING the storm (`tomo status` < 2s);
#   - the pressure controller escalates off rung 0 (coalescing checkpoints);
#   - AFTER the storm the hot file has FAR fewer versions than writes, yet its
#     newest version is exactly the final on-disk content, mirrored on B;
#   - every sprayed file lands on B with the right content and ≥1 version on A;
#   - history integrity is green and the network falls quiet.
#
# Storm scale note: a bare tight loop rewrites the hot file hundreds of
# thousands of times in 5s and buries the link under an unbounded backlog that
# is not the property under test. We pace the hot loop lightly so the storm is a
# genuine ~5s burst of *thousands* of events (well past the ≥1000 bar) while
# staying bounded enough to converge deterministically. The pacing is storm
# *generation*, not a convergence wait — every assertion below still polls.

source "$(dirname "$0")/lib/harness.sh"
scenario_init
require_cli
[[ "${TOMO_LINK_MODE:-local}" == "ssh" ]] && ensure_self_ssh

# This scenario characterizes storm + adaptive history; network lag is a
# separate axis (row 05 carries the Tier-2 lag requirement). Refuse to run under
# injected lag rather than assert against a moving target.
[[ -n "${TOMO_SCENARIO_LAG:-}" ]] && skip "06 is a storm scenario; lag variants are covered by 05"

A="$(make_machine a)"
B="$(make_machine b)"

link_machines "$A" "$B" >/dev/null

SMALL=200   # distinct sprayed files
HOT="$A/hot.txt"

# --- 1. Launch the storm in the background --------------------------------
# Spray SMALL distinct files up front (an instantaneous burst of events), then
# hammer one hot file for ~5s. The write count is recorded for the assertions.
storm() {
  local i
  for ((i = 1; i <= SMALL; i++)); do
    printf 'small file %d contents\n' "$i" > "$A/small_$i.txt"
  done
  local count=0 end
  end=$(( $(date +%s) + 5 ))
  while (( $(date +%s) < end )); do
    count=$((count + 1))
    printf 'hot version %d\n' "$count" > "$HOT"
    sleep 0.002   # storm generation pacing (see header) — not a convergence wait
  done
  printf '%s\n' "$count" > "$WORK/hot_writes"
}
storm &
STORM_PID=$!

# --- 2. DURING the storm: responsiveness + rung escalation ----------------
# Time `tomo status --json` repeatedly; the max must stay under 2s (the
# "process responsive" acceptance). Track the highest rung observed; it must
# leave 0 at least once (the controller degraded under pressure).
max_lat_ms=0
max_rung=0
while kill -0 "$STORM_PID" 2>/dev/null; do
  t0=$(date +%s%N)
  s="$( ( cd "$A" && "$TOMO_BIN" status --json 2>/dev/null ) )"
  t1=$(date +%s%N)
  lat=$(( (t1 - t0) / 1000000 ))
  (( lat > max_lat_ms )) && max_lat_ms=$lat
  (( lat < 2000 )) || fail "tomo status took ${lat}ms during storm (>= 2s): process not responsive"
  r="$(printf '%s' "$s" | jq -r '.history.rung // 0' 2>/dev/null)"
  [[ "$r" =~ ^[0-9]+$ ]] && (( r > max_rung )) && max_rung=$r
done
wait "$STORM_PID"

WRITES="$(cat "$WORK/hot_writes")"
FINAL_HOT="$(cat "$HOT")"
log "storm: $WRITES hot writes, max status latency ${max_lat_ms}ms, max rung $max_rung"

(( WRITES >= 1000 )) || fail "storm produced only $WRITES writes (< 1000): not a storm"
(( max_lat_ms < 2000 )) || fail "status latency peaked at ${max_lat_ms}ms during storm"
(( max_rung > 0 )) || fail "pressure controller never left rung 0 under storm (no degradation observed)"

# --- 3. AFTER the storm: converge, then assert coalescing + fidelity ------
# Live sync ships the final bytes; wait for the hot file to mirror on B.
wait_for 30 "final hot.txt content reaches B" assert_file_content "$B/hot.txt" "$FINAL_HOT"

# The final burst state is always versioned (invariant #4): poll until the
# newest recorded version of hot.txt equals the on-disk final bytes. This also
# drains the debounced final flush deterministically (no sleep).
hot_final_versioned() {
  local newest
  newest="$( hist_json "$A" hot.txt | jq -r '.[0].id // empty' )"
  [[ -n "$newest" ]] || return 1
  ( cd "$A" && "$TOMO_BIN" restore hot.txt --version "$newest" --stdout 2>/dev/null ) \
    | cmp -s - "$HOT"
}
wait_for 20 "final hot.txt state is versioned on A" hot_final_versioned

# Coalescing: FAR fewer versions than writes.
HOT_VERSIONS="$(hist_count "$A" hot.txt)"
log "hot.txt: $WRITES writes coalesced into $HOT_VERSIONS versions"
(( HOT_VERSIONS >= 1 ))  || fail "hot.txt has no versions (final state lost!)"
(( HOT_VERSIONS < 50 ))  || fail "hot.txt has $HOT_VERSIONS versions (>= 50): storm was not coalesced"
(( HOT_VERSIONS < WRITES )) || fail "no coalescing: $HOT_VERSIONS versions for $WRITES writes"

# The newest version's content == on-disk final == what B holds.
NEWEST_ID="$(hist_json "$A" hot.txt | jq -r '.[0].id')"
( cd "$A" && "$TOMO_BIN" restore hot.txt --version "$NEWEST_ID" --stdout ) > "$WORK/hot.newest" \
  || fail "restore of newest hot.txt version failed"
cmp -s "$WORK/hot.newest" "$HOT"      || fail "newest hot.txt version != on-disk final content"
cmp -s "$HOT" "$B/hot.txt"            || fail "hot.txt final content differs between A and B"

# --- 4. Every sprayed file: present on B with right content, versioned on A ---
for ((i = 1; i <= SMALL; i++)); do
  rel="small_$i.txt"
  want="$(printf 'small file %d contents\n' "$i")"
  wait_for 30 "$rel reaches B" assert_file_content "$B/$rel" "$want"
  wait_for 20 "$rel versioned on A" hist_count_ge "$A" "$rel" 1
done

# --- 5. History DB integrity green on both sides ---
db_check_ok "$A" || fail "db check failed on A"
db_check_ok "$B" || fail "db check failed on B"

# --- 6. Convergence + quiet network ---
wait_for 30 "index roots converge" roots_equal "$A" "$B"
assert_quiet_network "$A" 3
assert_converged "$A" "$B"
pass
