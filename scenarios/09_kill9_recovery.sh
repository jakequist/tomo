#!/usr/bin/env bash
# Scenario 09 — kill -9 mid-transfer recovery (Tier 2, M5)
# Spec: docs/TESTING.md row 09; invariant #8 (crash safety via staging + atomic
# rename — a partially transferred file must never be visible at its final path;
# kill -9 at any moment must not corrupt the tree or the history DB) and the M5
# reconnect/offline-queue path (watch survives peer death, respawns the
# local-peer serve child, re-transfers, converges).
#
# PLAN:
#  1. link A↔B; write a ~200 MiB random file on A ATOMICALLY (build outside the
#     synced tree, then mv into place — so A presents it fully-formed and B's
#     chunked assembly is the only thing in flight; an in-place dd would sync a
#     legitimate 0-byte create first and muddy the "no partial" assertion).
#  2. Poll B's staging until the chunked transfer is demonstrably in flight.
#  3a. kill -9 the WATCH side mid-transfer. Assert at MULTIPLE sampled instants
#      (during flight and after the kill) that B's final path is never a partial
#      — only absent or byte-complete; interrupted assembly is staging garbage.
#      Wait for the orphaned serve child to exit, restart watch → converge
#      byte-identical, staging clean, `db check` green both sides.
#  3b. Repeat with a fresh transfer, this time kill -9 the SERVE child: A goes to
#      the "queueing changes" state (connected=false), auto-respawns the serve,
#      re-transfers, converges. Staging clean, db check green.
#  4. assert_converged.

source "$(dirname "$0")/lib/harness.sh"
scenario_init
require_cli
[[ "${TOMO_LINK_MODE:-local}" == "ssh" ]] && skip "09 kills/respawns the local serve child; ssh link mode not supported"

A="$(make_machine a)"
B="$(make_machine b)"
SCRATCH="$WORK/scratch"; mkdir -p "$SCRATCH"

if [[ -n "${TOMO_SCENARIO_LAG:-}" ]]; then
  netem_delay "$TOMO_SCENARIO_LAG" || skip "netem unavailable for lag variant"
fi

WATCH="$(link_machines "$A" "$B")"

BIG_MIB=200
BIG_BYTES=$(( BIG_MIB * 1024 * 1024 ))

# The serve child of a watch pid (the process that watches B / applies A's frames).
serve_child() { pgrep -P "$1" -x tomo || true; }

# Build a random file of BIG_MIB outside any synced tree, then move it into A
# atomically so A never exposes an intermediate state.
stage_big() { # DEST_RELPATH
  local tmp="$SCRATCH/build.$$"
  dd if=/dev/urandom of="$tmp" bs=1M count="$BIG_MIB" status=none
  mv "$tmp" "$A/$1"
}

# wait_for-friendly: B has chunk-assembly bytes staged (transfer in flight).
transfer_in_flight() { [[ -n "$(find "$B/.tomo/staging" -type f 2>/dev/null)" ]]; }

# The load-bearing crash-safety check: B's final path for RELPATH is NEVER a
# partial — it is either absent or exactly the full byte count. A staged, half-
# assembled file lives under .tomo/staging and is renamed into place atomically
# only once complete (invariant #8).
assert_no_partial() { # RELPATH
  local f="$B/$1" sz
  [[ -e "$f" ]] || return 0
  sz="$(stat -c%s "$f" 2>/dev/null || echo -1)"
  [[ "$sz" == "$BIG_BYTES" ]] \
    || fail "PARTIAL at final path on B: $1 is $sz bytes (expected absent or $BIG_BYTES)"
}

# Sample the no-partial invariant repeatedly over a short window.
sample_no_partial() { # RELPATH COUNT
  local i
  for (( i = 0; i < $2; i++ )); do assert_no_partial "$1"; sleep 0.05; done
}

# ===========================================================================
# 3a. kill -9 the WATCH side mid-transfer.
# ===========================================================================
log "part A: kill -9 the WATCH mid-transfer of big1.bin"
SERVE="$(serve_child "$WATCH")"
[[ -n "$SERVE" ]] || fail "could not find serve child of watch pid $WATCH"
stage_big big1.bin
wait_for 20 "big1 transfer in flight to B" transfer_in_flight
sample_no_partial big1.bin 10          # during flight, before the kill

kill -9 "$WATCH"
sample_no_partial big1.bin 20          # around/after the crash

# The orphaned serve child loses its stdin pipe and exits on its own; wait for
# it so the restart brings up a single, clean served peer.
wait_for 15 "orphaned serve child exits after watch kill" \
  bash -c "! kill -0 $SERVE 2>/dev/null"
sample_no_partial big1.bin 10          # still no partial after the child is gone

WATCH="$(start_watch "$A" --local-peer "$B")"
wait_for 15 "A reconnected after restart" status_connected "$A"
wait_for 15 "B reconnected after restart" status_connected "$B"
wait_for 60 "big1 re-transfers byte-identical after restart" \
  assert_same_content "$A/big1.bin" "$B/big1.bin"
wait_for 15 "staging clean on B after big1" \
  bash -c "[[ -z \"\$(find '$B/.tomo/staging' -type f 2>/dev/null)\" ]]"
db_check_ok "$A" || fail "db check failed on A after watch-kill recovery"
db_check_ok "$B" || fail "db check failed on B after watch-kill recovery"
log "part A ok: no partial ever visible; big1 recovered byte-identical, staging clean, db green"

# ===========================================================================
# 3b. kill -9 the SERVE child mid-transfer (A queues + auto-respawns per M5).
# ===========================================================================
log "part B: kill -9 the SERVE child mid-transfer of big2.bin"
SERVE="$(serve_child "$WATCH")"
[[ -n "$SERVE" ]] || fail "could not find serve child of restarted watch"
stage_big big2.bin
wait_for 20 "big2 transfer in flight to B" transfer_in_flight
sample_no_partial big2.bin 10

kill -9 "$SERVE"
sample_no_partial big2.bin 10
# A must survive the peer death and surface the queueing state before healing.
wait_for 15 "A reports disconnected (queueing changes)" \
  bash -c "[[ \"\$( ( cd '$A' && '$TOMO_BIN' status --json 2>/dev/null ) | jq -r '.connected // false')\" == false ]]"
wait_for 20 "A auto-respawns serve and reconnects" status_connected "$A"
wait_for 60 "big2 re-transfers byte-identical after respawn" \
  assert_same_content "$A/big2.bin" "$B/big2.bin"
wait_for 15 "staging clean on B after big2" \
  bash -c "[[ -z \"\$(find '$B/.tomo/staging' -type f 2>/dev/null)\" ]]"
db_check_ok "$A" || fail "db check failed on A after serve-kill recovery"
db_check_ok "$B" || fail "db check failed on B after serve-kill recovery"
log "part B ok: watch survived serve death, respawned, big2 recovered byte-identical"

# ===========================================================================
# 4. final convergence
# ===========================================================================
wait_for 30 "converged and settled (final)" converged_and_settled "$A" "$B"
assert_converged "$A" "$B"
pass
