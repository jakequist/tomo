#!/usr/bin/env bash
# Scenario 15 — Single-session lock (both sides)
#
# A project may host only ONE live sync/serve session at a time. Each session
# holds an exclusive flock on `.tomo/state/session.lock` for its lifetime; a
# second session for the same project is refused fast with a clear message, and
# the kernel releases the lock on process exit (even kill -9), so a killed
# session frees it with no staleness logic.
#
# PLAN:
#  a. Link A↔B (A `tomo sync --local-peer B`). A second `tomo sync` in A is
#     refused fast (nonzero, "already running"), and the first session keeps
#     syncing afterwards (proves no interference).
#  b. A second project C doing `tomo sync --local-peer B` is refused because B's
#     serve lock is held by A's session — C's output mentions the lock/another
#     session, and C never reaches connected while A holds the link.
#  c. Kill the first session cleanly → its lock (and B's serve lock) free, so an
#     A-retry and then a C-retry each succeed (serially).
#
# Local-peer link only: the lock contention under test is exercised identically
# whatever the transport, and the local link keeps the serve process (and thus
# B's lock) on this machine where the test can reason about it deterministically.

source "$(dirname "$0")/lib/harness.sh"
scenario_init
require_cli
[[ "${TOMO_LINK_MODE:-local}" == "ssh" ]] && skip "15 exercises the local-peer session lock; ssh link mode not applicable"
ensure_jq

A="$(make_machine a)"
B="$(make_machine b)"
C="$(make_machine c)"

# Bounded, deterministic stop: SIGTERM then wait for exit (escalate to KILL).
stop_pid() {
  local p="$1" deadline
  [[ -n "$p" ]] || return 0
  kill "$p" 2>/dev/null || true
  deadline=$(( $(now_ms) + 8000 ))
  while kill -0 "$p" 2>/dev/null && (( $(now_ms) < deadline )); do sleep 0.1; done
  kill -9 "$p" 2>/dev/null || true
}

# Predicate (wait_for-friendly): FILE contains PATTERN.
log_has() { grep -q "$2" "$1" 2>/dev/null; }
# Predicate: DIR reports NOT connected.
not_connected() {
  [[ "$( ( cd "$1" && "$TOMO_BIN" status --json 2>/dev/null ) | jq -r '.connected // false')" != "true" ]]
}

# --- bring up the A↔B link; C is initialized but idle for now. ---
FIRST="$(link_machines "$A" "$B")"
( cd "$C" && "$TOMO_BIN" init >/dev/null 2>&1 ) || fail "tomo init on C"

# Sanity: the live link actually syncs.
echo "hello from a" > "$A/one.txt"
wait_for 10 "baseline sync A→B" assert_file_content "$B/one.txt" "hello from a"

# --- (a) a second sync in A is refused fast, without disturbing the first. ---
log "CHECK a: second sync in A is refused fast"
A2LOG="$WORK/a-second.log"
if ( cd "$A" && "$TOMO_BIN" sync --local-peer "$B" ) >"$A2LOG" 2>&1; then
  cat "$A2LOG" >&2
  fail "a: a second sync in A unexpectedly succeeded (should be refused)"
fi
grep -q "already running" "$A2LOG" \
  || { cat "$A2LOG" >&2; fail "a: refusal message did not mention 'already running'"; }
log "a: second sync refused: $(grep 'already running' "$A2LOG" | head -1)"

# The first session is unaffected — it keeps syncing.
echo "still alive" > "$A/two.txt"
wait_for 10 "first session still syncs after the refusal" \
  assert_file_content "$B/two.txt" "still alive"

# --- (b) project C is refused because B's serve lock is held. ---
log "CHECK b: C --local-peer B is refused (B's serve lock held)"
CPID="$(start_sync "$C" --local-peer "$B")"
CLOG="$WORK/c.watch.log"
wait_for 15 "C's output reports the held lock" log_has "$CLOG" "already running"
log "b: C saw: $(grep 'already running' "$CLOG" | head -1)"
# C must never reach connected while A holds B's serve lock.
not_connected "$C" || fail "b: C reported connected despite B's serve lock being held"
# ...and A's first session is still fine.
echo "a is fine" > "$A/three.txt"
wait_for 10 "first session unaffected by C's attempt" \
  assert_file_content "$B/three.txt" "a is fine"
stop_pid "$CPID"

# --- (c) kill the first session → the locks free → retries succeed serially. ---
log "CHECK c: killing the first session frees the locks; retries succeed serially"
stop_pid "$FIRST"   # releases A's lock AND (via graceful shutdown) B's serve lock.

# A-retry: a fresh sync in A now acquires both locks and reconnects.
ARETRY="$(start_sync "$A" --local-peer "$B")"
wait_for 20 "A-retry reaches connected" status_connected "$A"
wait_for 20 "A-retry reaches connected (B side)" status_connected "$B"
echo "after retry" > "$A/four.txt"
wait_for 10 "A-retry syncs" assert_file_content "$B/four.txt" "after retry"
stop_pid "$ARETRY"  # free the locks again for C's turn (serial).

# C-retry: with A gone, C can now hold both locks and sync.
CRETRY="$(start_sync "$C" --local-peer "$B")"
wait_for 20 "C-retry reaches connected" status_connected "$C"
wait_for 20 "C-retry reaches connected (B side)" status_connected "$B"
echo "c can sync now" > "$C/five.txt"
wait_for 10 "C-retry syncs" assert_file_content "$B/five.txt" "c can sync now"

# Final convergence check on the live C↔B pair.
wait_for 10 "index roots converge (C↔B)" roots_equal "$C" "$B"
assert_converged "$C" "$B"
pass
