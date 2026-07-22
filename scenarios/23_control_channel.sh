#!/usr/bin/env bash
# Scenario 23 — Control channel (Tier 4, UX-V2 §2)
# Spec: docs/SPEC.md §10 (control channel + event schema, graduated from
# UX-V2 §2). Every session serves a unix socket at .tomo/state/ctl.sock with a
# versioned event stream and a command channel.
#
# PLAN:
#  1. link A↔B (local mode), start `tomo events --json` streaming A's feed.
#  2. Create a file on B → assert a `synced` event with the right path reaches
#     A's feed, and the file converges.
#  3. Assert a `heartbeat` event carries last-sync recency (a small, non-null
#     last_sync_ms_ago shortly after a sync).
#  4. Create a concurrent conflict (partition the serve child, edit both sides,
#     heal — idiom from scenario 07) → assert a `conflict` event carries a
#     numeric id matching the history DB.
#  5. Resolve that conflict via the COMMAND channel (`tomo dev ctl`) → assert it
#     shows resolved in `tomo conflicts list --json`, the session stays
#     connected, and both sides stay converged.
#  6. Assert a SECOND concurrent events subscriber also receives events.
#  7. Assert the socket file is gone after a clean (SIGTERM) shutdown.
#  8. Assert a kill -9'd session's stale socket does not break the next
#     session's startup (it is removed and rebound).

source "$(dirname "$0")/lib/harness.sh"
scenario_init
require_cli
ensure_jq
[[ "${TOMO_LINK_MODE:-local}" == "ssh" ]] && skip "23 partitions the local serve child; ssh link mode not supported"

A="$(make_machine a)"
B="$(make_machine b)"

if [[ -n "${TOMO_SCENARIO_LAG:-}" ]]; then
  netem_delay "$TOMO_SCENARIO_LAG" || skip "netem unavailable for lag variant"
fi

# --- jq predicates over the (line-delimited JSON) event log -------------------
# Slurp mode so a match anywhere in the stream counts; tolerate a transient
# partial trailing line by discarding jq errors (wait_for retries).
ev_has() { # LOG JQ_ANY_FILTER
  jq -e -s "any(.[]; $2)" "$1" >/dev/null 2>&1
}

# --- 1. link, then attach an events subscriber to A --------------------------
WATCH="$(link_machines "$A" "$B")"

EVLOG="$WORK/a.events.jsonl"
( cd "$A" && exec "$TOMO_BIN" events --json ) >"$EVLOG" 2>&1 &
EV=$!
register_pid "$EV"
# The subscriber must actually be attached before we assert on its feed.
wait_for 10 "events subscriber attached (socket present)" \
  test -S "$A/.tomo/state/ctl.sock"

# --- 2. a file created on B appears as a `synced` event on A -----------------
echo "born-on-b" > "$B/from_b.txt"
wait_for 10 "file propagates B→A" assert_file_content "$A/from_b.txt" "born-on-b"
wait_for 10 "synced event for from_b.txt on A's feed" \
  ev_has "$EVLOG" '.event=="synced" and .path=="from_b.txt"'

# --- 3. heartbeat carries last-sync recency ----------------------------------
# Heartbeats fire ~1/s while a subscriber is attached; after a fresh sync one
# must report a small, non-null last_sync_ms_ago.
wait_for 10 "heartbeat carries recent last_sync_ms_ago" \
  ev_has "$EVLOG" '.event=="heartbeat" and (.last_sync_ms_ago != null) and (.last_sync_ms_ago < 30000)'

# --- 4. create a concurrent conflict (partition idiom from scenario 07) ------
wait_for 15 "seed converged" converged_and_settled "$A" "$B"
settle_status "$A" "$B"

SERVE="$(pgrep -P "$WATCH" -x tomo || true)"
[[ -n "$SERVE" ]] || fail "could not find serve child of watch pid $WATCH"
cleanup_serve() { [[ -n "${SERVE:-}" ]] && { kill -CONT "$SERVE" 2>/dev/null; kill -KILL "$SERVE" 2>/dev/null; } || true; }
register_cleanup_fn cleanup_serve

echo "base-original" > "$A/clash.txt"
wait_for 10 "clash seed A→B" assert_file_content "$B/clash.txt" "base-original"
wait_for 15 "clash seed converged" converged_and_settled "$A" "$B"
settle_status "$A" "$B"

kill -STOP "$SERVE"                       # partition
echo "edit-from-B" > "$B/clash.txt"       # B first (its inotify drains ahead)
echo "edit-from-A" > "$A/clash.txt"       # then A
kill -CONT "$SERVE"                       # heal

# A records the conflict and emits a `conflict` event carrying a numeric id.
wait_for 30 "conflict event carries a numeric id on A's feed" \
  ev_has "$EVLOG" '.event=="conflict" and (.id != null) and (.id | type=="number")'

# The id in the event must match a real unresolved conflict in A's history DB.
conflict_id="$(jq -s -r '[.[] | select(.event=="conflict" and (.id != null))][0].id' "$EVLOG")"
[[ -n "$conflict_id" && "$conflict_id" != "null" ]] || fail "no conflict id in A's event feed"
list_a="$( cd "$A" && "$TOMO_BIN" conflicts list --json )"
jq -e --argjson id "$conflict_id" 'any(.[]; .id == $id)' <<<"$list_a" >/dev/null \
  || fail "conflict event id $conflict_id is not in A's conflicts list"
log "conflict event id from feed = $conflict_id (matches history DB)"

# --- 5. resolve via the COMMAND channel (tomo dev ctl) -----------------------
reply="$( cd "$A" && "$TOMO_BIN" dev ctl "{\"type\":\"conflicts_resolve\",\"id\":$conflict_id,\"action\":\"keep\"}" )"
log "ctl resolve reply: $reply"
[[ "$( jq -r '.ok' <<<"$reply" )" == "true" ]] || fail "ctl conflicts_resolve did not report ok: $reply"

# The resolution takes effect: the id no longer appears among UNRESOLVED conflicts.
wait_for 10 "conflict $conflict_id shows resolved via CLI" bash -c \
  "! ( cd '$A' && '$TOMO_BIN' conflicts list --json | jq -e --argjson id $conflict_id 'any(.[]; .id == \$id)' >/dev/null )"

# The session stays connected and converged throughout.
status_connected "$A" || fail "A dropped its session after a control-channel resolve"
wait_for 15 "still converged after resolve" converged_and_settled "$A" "$B"

# --- 6. a second concurrent events subscriber also works ---------------------
EVLOG2="$WORK/a.events2.jsonl"
( cd "$A" && exec "$TOMO_BIN" events --json ) >"$EVLOG2" 2>&1 &
EV2=$!
register_pid "$EV2"
wait_for 10 "second subscriber attached" test -S "$A/.tomo/state/ctl.sock"
echo "second-sub-witness" > "$B/witness.txt"
wait_for 10 "witness propagates B→A" assert_file_content "$A/witness.txt" "second-sub-witness"
wait_for 10 "second subscriber sees the synced event" \
  ev_has "$EVLOG2" '.event=="synced" and .path=="witness.txt"'
# The first subscriber is still live and also saw it (concurrent fan-out).
wait_for 10 "first subscriber still receiving" \
  ev_has "$EVLOG" '.event=="synced" and .path=="witness.txt"'

# --- 7. clean shutdown removes the socket ------------------------------------
kill -TERM "$WATCH" 2>/dev/null || true
wait_for 10 "sync process exits on SIGTERM" bash -c "! kill -0 $WATCH 2>/dev/null"
wait_for 10 "control socket removed on clean shutdown" \
  bash -c "[[ ! -e '$A/.tomo/state/ctl.sock' ]]"
# The subscribers see EOF and exit on their own.
wait_for 10 "events subscriber exits at shutdown" bash -c "! kill -0 $EV 2>/dev/null"

# --- 8. a kill -9'd session's stale socket must not break the next startup ---
# Start a fresh session, kill it -9 (no clean-up runs → stale socket lingers),
# then start another and prove it starts and serves regardless.
S1="$(start_sync "$A" --local-peer "$B")"
wait_for 15 "S1 connects" status_connected "$A"
S1_SERVE="$(pgrep -P "$S1" -x tomo || true)"
kill -9 "$S1" 2>/dev/null || true
[[ -n "$S1_SERVE" ]] && kill -9 "$S1_SERVE" 2>/dev/null || true
wait_for 10 "S1 is dead" bash -c "! kill -0 $S1 2>/dev/null"
# The stale socket file survives a kill -9 (no Drop ran).
[[ -e "$A/.tomo/state/ctl.sock" ]] || log "note: stale socket already gone (still fine)"

S2="$(start_sync "$A" --local-peer "$B")"
wait_for 20 "S2 starts despite the stale socket" status_connected "$A"
# S2's control socket is live and answers a command.
reply2="$( cd "$A" && "$TOMO_BIN" dev ctl '{"type":"ping"}' )"
[[ "$( jq -r '.ok' <<<"$reply2" )" == "true" ]] || fail "S2 control socket not answering after stale-socket recovery: $reply2"
log "stale socket recovered: S2 bound a fresh socket and answered ping"

# --- final convergence -------------------------------------------------------
wait_for 15 "converged after recovery" converged_and_settled "$A" "$B"
assert_converged "$A" "$B"
pass
