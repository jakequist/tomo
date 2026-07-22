#!/usr/bin/env bash
# Scenario 28 — Pause / resume (Tier 4, UX-V2 §3; SPEC §13.5)
# Spec: docs/TESTING.md row 28. Pause is a SESSION state: while paused the
# session keeps observing and versioning local changes and stays connected, but
# ships nothing outbound and applies nothing inbound — both directions queue
# (the offline-queue model) until resume drains and reconciles them. The peer is
# told (protocol-v4 Pause/Resume frame) so it queues too instead of shipping into
# a void.
#
# PLAN:
#  1. link A↔B (local mode); converge + settle; attach `tomo events --json` to A.
#  2. `tomo pause` on A → status.json.paused true on A, a heartbeat/paused event
#     on A's feed; B surfaces peer-pause (status.json.peer_paused true).
#  3. Edit the SAME file on both sides (different bytes) + a disjoint file each.
#     Assert NOTHING crosses (disjoint files never appear on the far side; the
#     network stays quiet) yet BOTH histories still capture their own local edits.
#  4. `tomo resume` on A → both directions drain, assert_converged, and the
#     concurrent same-file edit surfaces as an ordinary non-blocking conflict.
#  5. Idempotence: a second pause says "already paused", a second resume "already
#     syncing"; pausing via `tomo dev ctl` matches the CLI reply.
#  6. Crash: `kill -9` a paused session, restart → it comes up UNPAUSED and
#     re-converges (the defined crash behavior). assert_converged.

source "$(dirname "$0")/lib/harness.sh"
scenario_init
require_cli
ensure_jq
[[ "${TOMO_LINK_MODE:-local}" == "ssh" ]] && skip "28 kills/restarts the local sync process; ssh link mode not supported"

A="$(make_machine a)"
B="$(make_machine b)"

if [[ -n "${TOMO_SCENARIO_LAG:-}" ]]; then
  netem_delay "$TOMO_SCENARIO_LAG" || skip "netem unavailable for lag variant"
fi

# --- status.json boolean readers (real CLI only) -----------------------------
status_bool() { # DIR FIELD → the field's value ("true"/"false"/"" )
  ( cd "$1" && "$TOMO_BIN" status --json 2>/dev/null ) | jq -r ".$2 // false"
}
is_paused()      { [[ "$(status_bool "$1" paused)" == "true" ]]; }
not_paused()     { [[ "$(status_bool "$1" paused)" == "false" ]]; }
peer_is_paused() { [[ "$(status_bool "$1" peer_paused)" == "true" ]]; }

# jq predicate over the line-delimited JSON event log (slurp; tolerate a partial
# trailing line — wait_for retries).
ev_has() { jq -e -s "any(.[]; $2)" "$1" >/dev/null 2>&1; }

# --- 1. link, converge, attach an events subscriber to A ---------------------
WATCH="$(link_machines "$A" "$B")"

echo "base" > "$A/shared.txt"
wait_for 15 "seed shared A→B" assert_file_content "$B/shared.txt" "base"
wait_for 15 "seed converged" converged_and_settled "$A" "$B"
settle_status "$A" "$B"

EVLOG="$WORK/a.events.jsonl"
( cd "$A" && exec "$TOMO_BIN" events --json ) >"$EVLOG" 2>&1 &
register_pid "$!"
wait_for 10 "events subscriber attached" test -S "$A/.tomo/state/ctl.sock"

# --- 2. pause A --------------------------------------------------------------
out="$( cd "$A" && "$TOMO_BIN" pause )"
log "pause: $out"
[[ "$out" == *"paused syncing"* ]] || fail "pause message unexpected: $out"

wait_for 10 "A reports paused in status.json" is_paused "$A"
wait_for 10 "A stays connected while paused" status_connected "$A"
wait_for 10 "a 'paused' event reached A's feed" ev_has "$EVLOG" '.event=="paused"'
wait_for 10 "a heartbeat carries paused:true" ev_has "$EVLOG" '.event=="heartbeat" and .paused==true'
# The peer learns of the pause and surfaces it (status + queues its own edits).
wait_for 10 "B reports peer_paused in status.json" peer_is_paused "$B"

# --- 3. edit both sides while paused; nothing crosses; histories still grow ---
# Same-file concurrent edit (different bytes) → a conflict on resume.
echo "edit-from-A" > "$A/shared.txt"
echo "edit-from-B" > "$B/shared.txt"
# Disjoint creates — the clean "did it cross?" signal.
echo "only-on-A" > "$A/a_only.txt"
echo "only-on-B" > "$B/b_only.txt"

# Local history keeps capturing each side's own edits while paused (invariant #4).
wait_for 15 "A still versions its own edit while paused" hist_count_ge "$A" a_only.txt 1
wait_for 15 "B still versions its own edit while paused" hist_count_ge "$B" b_only.txt 1
wait_for 15 "A re-versions shared.txt while paused"      hist_count_ge "$A" shared.txt 2
wait_for 15 "B versions its shared.txt edit while paused" hist_count_ge "$B" shared.txt 1

# NOTHING crosses while paused: the disjoint files never reach the far side, and
# the network stays quiet (a bounded observation — the frame counters must not
# move even though both trees are churning). assert_quiet_network settles first.
assert_quiet_network "$A" 3
assert_absent "$B/a_only.txt" || fail "A's edit crossed to B while paused"
assert_absent "$A/b_only.txt" || fail "B's edit crossed to A while paused"
# The concurrent shared.txt edits stayed put on their own sides.
assert_file_content "$A/shared.txt" "edit-from-A" || fail "A's shared.txt changed unexpectedly"
assert_file_content "$B/shared.txt" "edit-from-B" || fail "B's shared.txt changed unexpectedly"
log "confirmed: nothing crossed while paused, both histories kept capturing"

# --- 4. resume → both directions drain and converge; conflict surfaces --------
out="$( cd "$A" && "$TOMO_BIN" resume )"
log "resume: $out"
[[ "$out" == *"resumed syncing"* ]] || fail "resume message unexpected: $out"

wait_for 10 "A reports unpaused after resume" not_paused "$A"
wait_for 10 "B clears peer_paused after resume" bash -c \
  "[[ \"\$( ( cd '$B' && '$TOMO_BIN' status --json 2>/dev/null ) | jq -r '.peer_paused // false')\" == false ]]"

# Both directions drain: each side's disjoint edit reaches the other.
wait_for 30 "A's queued edit drains to B" assert_file_content "$B/a_only.txt" "only-on-A"
wait_for 30 "B's queued edit drains to A" assert_file_content "$A/b_only.txt" "only-on-B"

# The concurrent same-file edit resolves as an ordinary non-blocking conflict,
# identically on both sides, and is recorded (loser preserved — invariant #5).
wait_for 30 "shared.txt converges identically" assert_same_content "$A/shared.txt" "$B/shared.txt"
winner="$(cat "$A/shared.txt")"
[[ "$winner" == "edit-from-A" || "$winner" == "edit-from-B" ]] \
  || fail "shared.txt winner '$winner' is neither concurrent edit"
wait_for 15 "A records the concurrent conflict" bash -c \
  "[[ \"\$( cd '$A' && '$TOMO_BIN' status --json | jq -r .conflicts_unresolved )\" -ge 1 ]]"
wait_for 15 "B records the concurrent conflict" bash -c \
  "[[ \"\$( cd '$B' && '$TOMO_BIN' status --json | jq -r .conflicts_unresolved )\" -ge 1 ]]"
row="$( cd "$A" && "$TOMO_BIN" conflicts list --json | jq -c '[.[] | select(.path=="shared.txt")][0]' )"
[[ "$row" != "null" ]] || fail "no shared.txt conflict recorded on A"
log "resume drained both queues; concurrent edit surfaced as a conflict (winner '$winner')"

wait_for 20 "converged and settled after resume" converged_and_settled "$A" "$B"
settle_status "$A" "$B"

# --- 5. idempotence + ctl parity --------------------------------------------
out="$( cd "$A" && "$TOMO_BIN" resume )"
[[ "$out" == *"already syncing"* ]] || fail "second resume not idempotent: $out"
out="$( cd "$A" && "$TOMO_BIN" pause )"
[[ "$out" == *"paused syncing"* ]] || fail "pause after resume unexpected: $out"
out="$( cd "$A" && "$TOMO_BIN" pause )"
[[ "$out" == *"already paused"* ]] || fail "second pause not idempotent: $out"
wait_for 10 "A paused again" is_paused "$A"

# Pausing/resuming via the command channel matches the CLI (already paused now).
reply="$( cd "$A" && "$TOMO_BIN" dev ctl '{"type":"pause"}' )"
log "ctl pause reply: $reply"
[[ "$( jq -r '.ok'     <<<"$reply" )" == "true" ]] || fail "ctl pause not ok: $reply"
[[ "$( jq -r '.paused' <<<"$reply" )" == "true" ]] || fail "ctl pause paused!=true: $reply"
[[ "$( jq -r '.already' <<<"$reply" )" == "true" ]] || fail "ctl pause already!=true: $reply"

# --- 6. crash while paused → restart comes up UNPAUSED and re-converges -------
# Queue one more edit on A while paused, then kill -9 the sync process.
echo "written-while-paused" > "$A/crash.txt"
wait_for 15 "A versions the pre-crash edit" hist_count_ge "$A" crash.txt 1
kill -9 "$WATCH" 2>/dev/null || true
# Its local-peer serve child EOFs and exits with it; wait for the lock to free.
wait_for 15 "A's session lock released after kill -9" bash -c \
  "! ( cd '$A' && '$TOMO_BIN' status --json 2>/dev/null | jq -e '.connected==true' >/dev/null )"

WATCH2="$(start_sync "$A" --local-peer "$B")"
wait_for 20 "A reconnects after restart" status_connected "$A"
wait_for 20 "B reconnects after restart" status_connected "$B"
# The defining crash behavior: a restarted session is UNPAUSED.
wait_for 10 "restarted session comes up unpaused" not_paused "$A"

# The edit queued before the crash was never lost (it was in the index/history)
# and now drains to B, and both sides converge.
wait_for 30 "the pre-crash edit drains to B" assert_file_content "$B/crash.txt" "written-while-paused"
wait_for 20 "converged after crash recovery" converged_and_settled "$A" "$B"
assert_quiet_network "$A" 3
assert_converged "$A" "$B"
pass
