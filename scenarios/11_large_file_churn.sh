#!/usr/bin/env bash
# Scenario 11 — Large file + small-file churn (Tier 2, M5)
# Spec: docs/TESTING.md row 11; invariant #3 (sync latency is never sacrificed —
# the live path always ships the latest bytes immediately). A 1 GiB file ships as
# an interleaved chunk stream (4 chunks ≈ 1 MiB per pump before the loop returns
# to recv), so a flood of small-file changes must NOT wait head-of-line behind
# the bulk transfer: every small file lands with bounded per-file latency, the
# 1 GiB file arrives byte-identical, and the process stays responsive throughout.
#
# Binary choice: this is a THROUGHPUT test, so it runs against the RELEASE binary.
# The debug build's unoptimized BLAKE3/chunk hashing (and an O(chunks) receiver
# scan per chunk) inflate a 1 GiB transfer from ~17 s to ~110 s and push small-
# file latency to tens of seconds — an artifact of `-O0`, not of the sync design.
# Measuring interleaving on the optimized build is the honest test; every other
# scenario still exercises the debug build via run-all.
#
# PLAN:
#  1. df guard (skip cleanly under 4 GiB free); build/use the release binary.
#  2. link A↔B; start the 1 GiB transfer (atomic mv so A presents it whole) — the
#     scenario's long pole, left running while the churn overlaps it.
#  3. Spray 2000 small files in 8 batches of 250 (so at most 250 are ever in
#     flight). Per batch: write with a recorded timestamp, wait for the whole
#     batch on B, then derive each file's latency from B's mtime (no busy-poll
#     competing for CPU). Assert every small-file latency < 10 s, sampling
#     `status --json` each batch to prove it answers in < 2 s under the load.
#  4. Assert the 1 GiB file arrives byte-identical; db check green both sides.
#  5. Quiet network + assert_converged.

source "$(dirname "$0")/lib/harness.sh"
scenario_init
require_cli
[[ "${TOMO_LINK_MODE:-local}" == "ssh" ]] && skip "11 is a local-transport throughput test; ssh link mode not exercised here"
ensure_jq

# --- 1. disk guard + release binary ---
avail_kb="$(df -P "$WORK" | awk 'NR==2 {print $4}')"
[[ -n "$avail_kb" && "$avail_kb" -ge 4194304 ]] \
  || skip "need ≥4 GiB free under $WORK for the 1 GiB + churn workload (have ${avail_kb:-?} KiB)"

REL="$REPO_ROOT/target/release/tomo"
if [[ ! -x "$REL" ]]; then
  log "building the release binary (throughput scenario; debug -O0 is not representative)"
  ( cd "$REPO_ROOT" && cargo build --release ) >/dev/null 2>&1 \
    || skip "could not build the release binary for the throughput scenario"
fi
# Point every harness helper at the optimized binary for this scenario only.
TOMO_BIN="$REL"
log "using release binary: $TOMO_BIN"

A="$(make_machine a)"
B="$(make_machine b)"

if [[ -n "${TOMO_SCENARIO_LAG:-}" ]]; then
  netem_delay "$TOMO_SCENARIO_LAG" || skip "netem unavailable for lag variant"
fi

WATCH="$(link_machines "$A" "$B")"

# --- 2. start the 1 GiB transfer (atomic: build outside the tree, then mv in) ---
SCRATCH="$WORK/scratch"; mkdir -p "$SCRATCH"
HUGE_MIB=1024
log "building the ${HUGE_MIB} MiB file (the long pole)"
dd if=/dev/urandom of="$SCRATCH/huge.bin" bs=1M count="$HUGE_MIB" status=none
mv "$SCRATCH/huge.bin" "$A/huge.bin"
# Confirm the bulk transfer is genuinely in flight before we start the churn, so
# the early batches are measured under real head-of-line pressure.
wait_for 20 "1 GiB transfer in flight to B" \
  bash -c "[[ -n \"\$(find '$B/.tomo/staging' -type f 2>/dev/null)\" ]]"

# --- 3. spray 2000 small files in batches of 250; measure per-file latency ---
BATCHES=8
BATCH_SIZE=250
# Env-tunable: CI runners (2 cores, shared IO) need a looser bound than the
# dev VM; the head-of-line-blocking property holds relatively either way.
LAT_BOUND_MS="${TOMO_CHURN_LAT_BOUND_MS:-10000}"
STATUS_BOUND_MS="${TOMO_STATUS_BOUND_MS:-2000}"
max_lat=0
max_status=0
sprayed=0
huge_still_running_batches=0

for b in $(seq 1 "$BATCHES"); do
  declare -A wrote_ns=()
  base=$(( (b - 1) * BATCH_SIZE ))
  # Write the batch as fast as the shell allows, recording each write instant.
  for k in $(seq 1 "$BATCH_SIZE"); do
    idx=$(( base + k ))
    printf 'small-%d\n' "$idx" > "$A/s_$idx.txt"
    wrote_ns[$idx]=$(date +%s%N)
  done
  # Wait (bounded) until the ENTIRE batch has landed on B.
  deadline=$(( $(date +%s%N)/1000000 + ${TOMO_CHURN_BATCH_DEADLINE_MS:-30000} ))
  while :; do
    missing=0
    for k in $(seq 1 "$BATCH_SIZE"); do
      idx=$(( base + k ))
      [[ "$(cat "$B/s_$idx.txt" 2>/dev/null)" == "small-$idx" ]] || { missing=1; break; }
    done
    (( missing == 0 )) && break
    (( $(date +%s%N)/1000000 < deadline )) \
      || fail "batch $b did not fully land on B within 30s (head-of-line blocking under bulk load?)"
    sleep 0.05
  done
  # Derive per-file latency from B's mtime — no busy-poll skewing the numbers.
  for k in $(seq 1 "$BATCH_SIZE"); do
    idx=$(( base + k ))
    arr_ns="$(stat -c'%.9Y' "$B/s_$idx.txt" 2>/dev/null | tr -d .)"
    lat=$(( ( arr_ns - ${wrote_ns[$idx]} ) / 1000000 ))
    (( lat < 0 )) && lat=0
    (( lat > max_lat )) && max_lat=$lat
    (( lat < LAT_BOUND_MS )) \
      || fail "small-file s_$idx.txt latency ${lat}ms exceeded ${LAT_BOUND_MS}ms bound under bulk load"
    sprayed=$(( sprayed + 1 ))
  done
  # The process must stay responsive: status --json answers quickly under load.
  t0=$(date +%s%N)
  ( cd "$A" && "$TOMO_BIN" status --json >/dev/null 2>&1 ) || fail "status --json failed under load"
  st=$(( ( $(date +%s%N) - t0 ) / 1000000 ))
  (( st > max_status )) && max_status=$st
  (( st < STATUS_BOUND_MS )) || fail "status --json took ${st}ms (> ${STATUS_BOUND_MS}ms) under load"
  cmp -s "$A/huge.bin" "$B/huge.bin" 2>/dev/null || huge_still_running_batches=$(( huge_still_running_batches + 1 ))
  log "batch $b: ${BATCH_SIZE} files landed; max latency so far=${max_lat}ms; 1 GiB still transferring=$([[ $huge_still_running_batches -ge $b ]] && echo yes || echo no)"
  unset wrote_ns
done

(( sprayed == BATCHES * BATCH_SIZE )) || fail "expected $(( BATCHES * BATCH_SIZE )) small files, sprayed $sprayed"
(( huge_still_running_batches >= 1 )) \
  || fail "the 1 GiB transfer finished before any churn batch — churn was not measured under bulk load"
log "churn done: ${sprayed} small files, MAX per-file latency=${max_lat}ms (bound ${LAT_BOUND_MS}ms), MAX status latency=${max_status}ms, overlapped bulk transfer for ${huge_still_running_batches} batch(es)"

# --- 4. the 1 GiB file arrives byte-identical; history db intact both sides ---
wait_for 90 "1 GiB file arrives byte-identical" assert_same_content "$A/huge.bin" "$B/huge.bin"
cmp "$A/huge.bin" "$B/huge.bin" || fail "1 GiB file differs after convergence"
db_check_ok "$A" || fail "history db check failed on A"
db_check_ok "$B" || fail "history db check failed on B"
log "1 GiB byte-identical (cmp clean); db check green both sides"

# --- 5. quiet network + final convergence ---
wait_for 60 "converged and settled (final)" converged_and_settled "$A" "$B"
assert_quiet_network "$A" 3
assert_converged "$A" "$B"
pass
