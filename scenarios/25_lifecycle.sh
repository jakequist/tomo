#!/usr/bin/env bash
# Scenario 25 — Session lifecycle: detach / attach / stop / logs (Tier 4, UX-V2 §1)
# Spec: docs/SPEC.md §13 (control channel + lifecycle, graduated from UX-V2 §1).
# A session is a server; `tomo sync -d` runs it in the background, `tomo attach`
# joins it, `tomo stop` shuts it down, `tomo logs` tails its log.
#
# PLAN (local link mode — the detached session drives a `--local-peer` served B):
#  1. `tomo sync -d --local-peer B` returns promptly, prints "session started
#     (pid N)" + the attach hint; the session lock and control socket appear.
#  2. A file created on B syncs to A while A is detached.
#  3. `tomo logs` shows A's `synced <path>` line from the session log.
#  4. `tomo attach --json` streams a live `synced` event; Ctrl-C (SIGINT to the
#     attach client) leaves the session running and connected.
#  5. A second `tomo sync -d` is refused cleanly by the flock ("already running").
#  6. `tomo stop` stops the session: pid dead, lock released, socket gone.
#  7. `tomo stop` again is a clean no-op; `tomo logs` still works after stop.
#  8. kill -9 the detached session → `tomo stop` reports no session (the flock is
#     kernel-released) and a fresh `tomo sync -d` starts clean despite the stale
#     socket/lock left behind.

source "$(dirname "$0")/lib/harness.sh"
scenario_init
require_cli
ensure_jq
[[ "${TOMO_LINK_MODE:-local}" == "ssh" ]] && skip "25 exercises the detached local link; ssh link mode not covered here"

A="$(make_machine a)"
B="$(make_machine b)"
( cd "$A" && "$TOMO_BIN" init >/dev/null 2>&1 ) || fail "tomo init on A"
( cd "$B" && "$TOMO_BIN" init >/dev/null 2>&1 ) || fail "tomo init on B"

lock_pid() { sed -n 's/^pid=//p' "$1/.tomo/state/session.lock" 2>/dev/null; }

# --- 1. sync -d returns promptly with pid + attach hint ----------------------
start_out="$( cd "$A" && "$TOMO_BIN" sync -d --local-peer "$B" )" \
  || fail "sync -d exited non-zero: $start_out"
log "sync -d said: $start_out"
grep -q "session started (pid " <<<"$start_out" || fail "no pid in sync -d output: $start_out"
grep -q "tomo attach"           <<<"$start_out" || fail "no attach hint in sync -d output: $start_out"

wait_for 10 "control socket bound" test -S "$A/.tomo/state/ctl.sock"
[[ -f "$A/.tomo/state/session.lock" ]] || fail "session lock file missing"
PID="$(lock_pid "$A")"
[[ -n "$PID" ]] || fail "no pid recorded in session.lock"
register_pid "$PID"
wait_for 15 "A reports connected while detached" status_connected "$A"

# --- 2. a file syncs while detached ------------------------------------------
echo "born-detached" > "$B/from_b.txt"
wait_for 10 "file propagates B→A while detached" assert_file_content "$A/from_b.txt" "born-detached"

# --- 3. tomo logs shows the synced line --------------------------------------
wait_for 10 "logs shows A's synced line" bash -c \
  "( cd '$A' && '$TOMO_BIN' logs -n 200 ) | grep -q 'synced from_b.txt'"

# --- 4. attach --json streams a live event; Ctrl-C leaves the session running -
ATLOG="$WORK/a.attach.jsonl"
( cd "$A" && exec "$TOMO_BIN" attach --json ) >"$ATLOG" 2>&1 &
AT=$!
register_pid "$AT"
wait_for 10 "attach subscriber attached (socket present)" test -S "$A/.tomo/state/ctl.sock"
echo "attach-witness" > "$B/witness.txt"
wait_for 10 "witness propagates B→A" assert_file_content "$A/witness.txt" "attach-witness"
wait_for 15 "attach --json streamed a live synced event" bash -c \
  "jq -e -s 'any(.[]; .event==\"synced\" and .path==\"witness.txt\")' '$ATLOG' >/dev/null 2>&1"

# Ctrl-C the attach client: it must detach (exit) WITHOUT touching the session.
kill -INT "$AT" 2>/dev/null || true
wait_for 10 "attach client exits on Ctrl-C" bash -c "! kill -0 $AT 2>/dev/null"
kill -0 "$PID" 2>/dev/null || fail "session died when attach detached (Ctrl-C leaked to the session)"
status_connected "$A" || fail "session dropped its connection after attach Ctrl-C"

# --- 5. a second sync -d is refused cleanly (single-session flock) -----------
if second="$( cd "$A" && "$TOMO_BIN" sync -d --local-peer "$B" 2>&1 )"; then
  fail "second sync -d unexpectedly succeeded: $second"
fi
grep -q "already running" <<<"$second" || fail "second sync -d did not report already-running: $second"
log "second sync -d refused cleanly"
status_connected "$A" || fail "original session disturbed by the refused second start"

# --- 6. tomo stop: pid dead, lock released, socket gone ----------------------
stop_out="$( cd "$A" && "$TOMO_BIN" stop )" || fail "stop exited non-zero: $stop_out"
log "stop said: $stop_out"
grep -q "stopped session (pid $PID)" <<<"$stop_out" || fail "stop did not report pid $PID: $stop_out"
wait_for 10 "session pid $PID dead after stop" bash -c "! kill -0 $PID 2>/dev/null"
wait_for 10 "control socket removed after stop" bash -c "[[ ! -e '$A/.tomo/state/ctl.sock' ]]"
if status_connected "$A"; then fail "status still reports connected after stop"; fi

# --- 7. stop is idempotent; logs still work after stop -----------------------
noop_out="$( cd "$A" && "$TOMO_BIN" stop )" || fail "idempotent stop exited non-zero: $noop_out"
grep -q "no running session" <<<"$noop_out" || fail "idempotent stop message unexpected: $noop_out"
( cd "$A" && "$TOMO_BIN" logs -n 5 >/dev/null ) || fail "logs failed after stop"

# --- 8. kill -9 recovery: stale lock is kernel-released; fresh start is clean -
start2="$( cd "$A" && "$TOMO_BIN" sync -d --local-peer "$B" )" || fail "restart sync -d failed: $start2"
PID2="$(lock_pid "$A")"
[[ -n "$PID2" ]] || fail "no pid after restart"
register_pid "$PID2"
wait_for 15 "restarted session connects" status_connected "$A"
SERVE2="$(pgrep -P "$PID2" -x tomo || true)"
kill -9 "$PID2" 2>/dev/null || true
[[ -n "$SERVE2" ]] && kill -9 "$SERVE2" 2>/dev/null || true
wait_for 10 "kill -9'd session is dead" bash -c "! kill -0 $PID2 2>/dev/null"

# The flock was released by the kernel on the kill -9, so stop sees nothing.
kill9_stop="$( cd "$A" && "$TOMO_BIN" stop )" || fail "stop after kill -9 exited non-zero: $kill9_stop"
grep -q "no running session" <<<"$kill9_stop" || fail "stop after kill -9 should report no session: $kill9_stop"

# A fresh detached session starts clean despite the stale socket/lock file.
start3="$( cd "$A" && "$TOMO_BIN" sync -d --local-peer "$B" )" || fail "fresh sync -d after kill -9 failed: $start3"
PID3="$(lock_pid "$A")"
[[ -n "$PID3" ]] || fail "no pid after fresh start"
register_pid "$PID3"
wait_for 15 "fresh session connects after kill -9" status_connected "$A"

# --- final convergence -------------------------------------------------------
wait_for 15 "converged after the full lifecycle" converged_and_settled "$A" "$B"
# Stop cleanly so teardown is quiet, then assert convergence over the quiesced tree.
( cd "$A" && "$TOMO_BIN" stop ) >/dev/null 2>&1 || true
assert_converged "$A" "$B"
pass
