#!/usr/bin/env bash
# Scenario 13 — Wall-clock skew immunity (Tier 2, M5)
# Spec: docs/TESTING.md row 13; invariant #7 (never trust wall clocks for
# ordering — vector clocks only; wall time is display-only). Run the watch (and
# thus, in local mode, the serve child it spawns — both inherit libfaketime's
# LD_PRELOAD + offset) three years in the past. Everything must still converge,
# and history ordering must follow the vector clock, not the bogus wall time.
#
# Local-mode note: the watch spawns the serve, so wrapping the watch in faketime
# skews BOTH processes by the same -3y offset. That is the right lever here — the
# invariant under test is "ordering is by vector clock, not wall time", and it is
# exercised identically whether the two peers are equally skewed or skewed
# relative to each other: the recorded wall_unix_ms is years wrong, yet the
# version order the CLI reports is driven entirely by causality.
#
# We skew only the WALL clock (CLOCK_REALTIME) via FAKETIME_DONT_FAKE_MONOTONIC=1
# and deliberately leave CLOCK_MONOTONIC real. That is not a workaround — it is
# precisely the condition invariant #7 promises to survive: tomo records wall
# time only for display (`wall_unix_ms`) and drives every DECISION (ordering via
# vector clocks; debounce/throttle/recv timeouts via the monotonic clock) off
# clocks that a wrong wall time must never perturb. (Faking the monotonic clock
# too would instead break Rust's own `recv_timeout` condvar wait — a libfaketime
# artifact that says nothing about tomo; the clean split below tests the real
# invariant, that a bogus wall clock changes nothing that matters.)
#
# PLAN:
#  1. Bring the link up with A's watch (and its serve child) under `faketime -3y`.
#  2. Scenario-01-style pass: create/modify/delete in both directions; converge;
#     roots equal.
#  3. Edit one file alternately from each side. Assert `tomo log` on BOTH sides
#     reports the SAME version order (ids ascending), that restoring by ascending
#     id replays the exact content sequence on both sides (ordering is causal),
#     and that the recorded wall_unix_ms is years off real time (display-only).
#  4. assert_converged.

source "$(dirname "$0")/lib/harness.sh"
scenario_init
require_cli
[[ "${TOMO_LINK_MODE:-local}" == "ssh" ]] && skip "13 runs the local-peer watch under faketime; ssh link mode not supported"
ensure_jq

# faketime is the whole point of this scenario — skip cleanly if unavailable.
if ! command -v faketime >/dev/null 2>&1; then
  log "installing faketime (sandbox VM; safe)"
  sudo apt-get install -y -qq faketime >/dev/null 2>&1 || true
fi
command -v faketime >/dev/null 2>&1 || skip "faketime not available"
faketime -f '-3y' true 2>/dev/null || skip "faketime cannot intercept time on this host"

A="$(make_machine a)"
B="$(make_machine b)"

if [[ -n "${TOMO_SCENARIO_LAG:-}" ]]; then
  netem_delay "$TOMO_SCENARIO_LAG" || skip "netem unavailable for lag variant"
fi

# --- 1. bring up the link with A's watch (+ serve child) three years in the past.
( cd "$A" && "$TOMO_BIN" init >/dev/null 2>&1 ) || fail "init A"
( cd "$B" && "$TOMO_BIN" init >/dev/null 2>&1 ) || fail "init B"

# Launch the faketimed watch. `faketime` is the parent; the real `tomo watch`
# runs as its child and inherits the fake clock. Register both for teardown:
# SIGTERM to the tomo child triggers the graceful shutdown that reaps the serve
# grandchild; the faketime wrapper exits once its child does.
( cd "$A" && exec env FAKETIME_DONT_FAKE_MONOTONIC=1 faketime -f '-3y' \
    "$TOMO_BIN" watch --local-peer "$B" ) \
  >"$WORK/a.watch.log" 2>&1 &
FT_PID=$!
register_pid "$FT_PID"
WATCH=""
for _ in $(seq 1 60); do
  WATCH="$(pgrep -P "$FT_PID" -x tomo || true)"
  [[ -n "$WATCH" ]] && break
  sleep 0.1
done
[[ -n "$WATCH" ]] || fail "faketimed tomo watch did not start under pid $FT_PID"
register_pid "$WATCH"
# Backstop: the registered-pid teardown SIGTERMs WATCH (graceful shutdown reaps
# the serve grandchild) and FT_PID, but force-kill the whole faketimed tree too
# so a mid-scenario failure can never leak a lingering process (a leaked, CPU-
# hungry watch would poison later runs).
cleanup_faketime_tree() {
  local sc; sc="$(pgrep -P "${WATCH:-0}" -x tomo 2>/dev/null || true)"
  kill -9 ${WATCH:-} ${sc:-} ${FT_PID:-} 2>/dev/null || true
}
register_cleanup_fn cleanup_faketime_tree

wait_for 20 "A connected (under faketime)" status_connected "$A"
wait_for 20 "B connected (under faketime)" status_connected "$B"

# Confirm the skew is actually in effect: A's status wall clock is years behind.
now_ms() { printf '%s000\n' "$(date +%s)"; }
YEAR_MS=$(( 365 * 24 * 60 * 60 * 1000 ))
a_wall="$( ( cd "$A" && "$TOMO_BIN" status --json 2>/dev/null ) | jq -r '.updated_unix_ms')"
[[ -n "$a_wall" && "$a_wall" != null ]] || fail "no status wall clock on A"
(( a_wall < $(now_ms) - 2 * YEAR_MS )) \
  || fail "faketime offset not in effect: A status wall=$a_wall vs real now=$(now_ms)"
log "clock skew confirmed: A's wall clock reads ~$(( ($(now_ms) - a_wall) / YEAR_MS )) years in the past"

# --- 2. scenario-01-style create/modify/delete both directions ---
echo "hello from a" > "$A/src.txt"
wait_for 15 "create A→B" assert_file_content "$B/src.txt" "hello from a"
echo "edited on a" > "$A/src.txt"
wait_for 15 "modify A→B" assert_file_content "$B/src.txt" "edited on a"
echo "born on b" > "$B/artifact.bin"
wait_for 15 "create B→A" assert_file_content "$A/artifact.bin" "born on b"
rm "$A/src.txt"
wait_for 15 "delete A→B" assert_absent "$B/src.txt"
mkdir -p "$A/deep/nested"
echo "deep" > "$A/deep/nested/file.txt"
wait_for 15 "nested create A→B" assert_file_content "$B/deep/nested/file.txt" "deep"
wait_for 15 "index roots converge" roots_equal "$A" "$B"
log "basic propagation converges normally under a 3-year clock skew"

# --- 3. alternating edits → identical causal version order on both sides ---
# Edit note.txt from each side in turn, waiting for each version to be recorded
# before the next so adaptive debouncing can't coalesce the sequence.
SEQ=("v1-from-A" "v2-from-B" "v3-from-A" "v4-from-B")
edit_side() { # SIDE(a|b) CONTENT
  local dir; [[ "$1" == a ]] && dir="$A" || dir="$B"
  printf '%s\n' "$2" > "$dir/note.txt"
}
n=0
for entry in "a:${SEQ[0]}" "b:${SEQ[1]}" "a:${SEQ[2]}" "b:${SEQ[3]}"; do
  side="${entry%%:*}"; content="${entry#*:}"; n=$((n + 1))
  edit_side "$side" "$content"
  # It converges on both trees...
  wait_for 15 "note.txt='$content' reaches A" assert_file_content "$A/note.txt" "$content"
  wait_for 15 "note.txt='$content' reaches B" assert_file_content "$B/note.txt" "$content"
  # ...and both sides record the new version before we move on.
  wait_for 20 "A records version $n of note.txt" hist_count_ge "$A" "note.txt" "$n"
  wait_for 20 "B records version $n of note.txt" hist_count_ge "$B" "note.txt" "$n"
done

# Restore each version in ASCENDING id order and confirm the content sequence is
# exactly the order the edits were authored — identically on BOTH sides. This is
# the observable proof that ordering follows the vector clock, not wall time.
replay_by_id() { # DIR → newline-joined contents ordered by ascending version id
  local dir="$1" ids id out=""
  ids="$( ( cd "$dir" && "$TOMO_BIN" log note.txt --json ) | jq -r 'sort_by(.id) | .[].id' )"
  while IFS= read -r id; do
    [[ -z "$id" ]] && continue
    out+="$( cd "$dir" && "$TOMO_BIN" restore note.txt --version "$id" --stdout )"$'\n'
  done <<< "$ids"
  printf '%s' "$out"
}
EXPECTED="$(printf '%s\n' "${SEQ[@]}")"
replay_a="$(replay_by_id "$A")"
replay_b="$(replay_by_id "$B")"
[[ "$replay_a" == "$EXPECTED"$'\n' || "$replay_a" == "$EXPECTED" ]] \
  || fail "A's id-ordered content replay does not match the authored sequence:"$'\n'"$replay_a"
[[ "$replay_b" == "$replay_a" ]] \
  || fail "sides disagree on version order — A and B replay differently:"$'\n'"A:$replay_a"$'\n'"B:$replay_b"
log "history order is identical on both sides and matches the causal edit sequence"

# The recorded wall_unix_ms is years wrong (display-only), yet ordering held.
newest_wall="$( ( cd "$A" && "$TOMO_BIN" log note.txt --json ) | jq -r 'sort_by(.id) | last | .wall_unix_ms')"
(( newest_wall < $(now_ms) - 2 * YEAR_MS )) \
  || fail "expected skewed wall_unix_ms in history, got $newest_wall (real now $(now_ms))"
log "history wall_unix_ms is ~$(( ($(now_ms) - newest_wall) / YEAR_MS )) years off yet order is correct (wall time is display-only)"

# --- 4. final convergence ---
wait_for 20 "converged and settled (final)" converged_and_settled "$A" "$B"
assert_converged "$A" "$B"
pass
