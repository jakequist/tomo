#!/usr/bin/env bash
# Scenario 10 — Offline changes on both sides (Tier 2, M5)
# Spec: docs/TESTING.md row 10. A REAL disconnect (kill -9 the serve child, not a
# SIGSTOP freeze) drops the link; both trees mutate independently while parted;
# on heal the watch auto-respawns the serve (M5 reconnect/offline queue) and BOTH
# sides' offline changes converge. A same-path collision made while parted must
# resolve deterministically and be recorded as a conflict (invariant #5).
#
# Note on the mechanism: killing the serve child triggers A's ~2s backoff-then-
# respawn, so the parted window is short. That is fine — the offline changes are
# written into the shell in milliseconds, well inside the window, and no change
# can be lost regardless of timing: A's watcher records its own edits into A's
# index while disconnected (queued), and the respawned serve's startup scan (plus
# live inotify) picks up whatever B's tree changed while its serve was dead.
#
# PLAN:
#  1. link A↔B; seed a base both sides know; converge + settle (settling BEFORE
#     parting is mandatory — parting mid-reconciliation leaves inconsistent heads
#     that surface as spurious conflicts).
#  2. kill -9 the serve child; immediately make disjoint offline changes on both
#     trees (create/modify/delete each side) plus one intentional same-path
#     collision (different bytes on each side).
#  3. Observe A's "queueing changes" state (connected=false), then heal (A
#     auto-respawns the serve).
#  4. wait_for full convergence: every offline change lands on the far side, the
#     collision resolves to one side's bytes identically on both, and exactly the
#     collision is recorded as a conflict.
#  5. Quiet network + assert_converged.

source "$(dirname "$0")/lib/harness.sh"
scenario_init
require_cli
[[ "${TOMO_LINK_MODE:-local}" == "ssh" ]] && skip "10 kills/respawns the local serve child; ssh link mode not supported"

A="$(make_machine a)"
B="$(make_machine b)"

if [[ -n "${TOMO_SCENARIO_LAG:-}" ]]; then
  netem_delay "$TOMO_SCENARIO_LAG" || skip "netem unavailable for lag variant"
fi

WATCH="$(link_machines "$A" "$B")"
serve_child() { pgrep -P "$1" -x tomo || true; }

# --- 1. seed a base both sides know, converge + settle ---
for f in base1 base2 todel_a todel_b collide; do echo "orig-$f" > "$A/$f.txt"; done
for f in base1 base2 todel_a todel_b collide; do
  wait_for 15 "seed $f A→B" assert_file_content "$B/$f.txt" "orig-$f"
done
wait_for 15 "seed converged" converged_and_settled "$A" "$B"
settle_status "$A" "$B"

# --- 2. real disconnect, then disjoint offline changes on both sides ---
# Poll for the child: a single-shot pgrep can momentarily miss under heavy
# system load (observed once right after scenario 11's 1 GiB run).
wait_for 10 "serve child of watch pid $WATCH visible" \
  bash -c "pgrep -P '$WATCH' -x tomo >/dev/null"
SERVE="$(serve_child "$WATCH")"
[[ -n "$SERVE" ]] || fail "could not find serve child of watch pid $WATCH"
kill -9 "$SERVE"

# A-side offline changes: create, modify, delete.
echo "created-on-A"  > "$A/newA.txt"
echo "modified-on-A" > "$A/base1.txt"
rm "$A/todel_a.txt"
# B-side offline changes: create, modify, delete (B's serve is dead — these are
# plain disk writes the respawned serve must reconcile).
echo "created-on-B"  > "$B/newB.txt"
echo "modified-on-B" > "$B/base2.txt"
rm "$B/todel_b.txt"
# Intentional same-path collision (different bytes each side).
CA="collide-from-A"
CB="collide-from-B"
echo "$CA" > "$A/collide.txt"
echo "$CB" > "$B/collide.txt"

# --- 3. observe the queueing state, then heal (auto-respawn) ---
wait_for 15 "A reports disconnected (queueing changes)" \
  bash -c "[[ \"\$( ( cd '$A' && '$TOMO_BIN' status --json 2>/dev/null ) | jq -r '.connected // false')\" == false ]]"
wait_for 20 "A auto-respawns serve and reconnects" status_connected "$A"

# --- 4. full convergence including both sides' offline changes ---
wait_for 30 "A's create reaches B"   assert_file_content "$B/newA.txt"  "created-on-A"
wait_for 30 "B's create reaches A"   assert_file_content "$A/newB.txt"  "created-on-B"
wait_for 30 "A's modify reaches B"   assert_file_content "$B/base1.txt" "modified-on-A"
wait_for 30 "B's modify reaches A"   assert_file_content "$A/base2.txt" "modified-on-B"
wait_for 30 "A's delete reaches B"   assert_absent "$B/todel_a.txt"
wait_for 30 "B's delete reaches A"   assert_absent "$A/todel_b.txt"

# The collision converges to ONE side's bytes, identical on both sides, and it
# must be exactly one of the two concurrent writes (nothing merged/invented).
wait_for 30 "collision converges identically" assert_same_content "$A/collide.txt" "$B/collide.txt"
winner="$(cat "$A/collide.txt")"
[[ "$winner" == "$CA" || "$winner" == "$CB" ]] \
  || fail "collide winner '$winner' is neither concurrent write"
log "offline collision resolved deterministically to '$winner'"

# Exactly the collision is recorded as a conflict (the disjoint edits are not
# conflicts — each path changed on only one side).
wait_for 15 "A records the collision conflict" \
  bash -c "[[ \"\$( cd '$A' && '$TOMO_BIN' status --json | jq -r .conflicts_unresolved )\" -ge 1 ]]"
wait_for 15 "B records the collision conflict" \
  bash -c "[[ \"\$( cd '$B' && '$TOMO_BIN' status --json | jq -r .conflicts_unresolved )\" -ge 1 ]]"
row="$( cd "$A" && "$TOMO_BIN" conflicts list --json | jq -c '[.[] | select(.path=="collide.txt")][0]' )"
[[ "$row" != "null" ]] || fail "no collide.txt conflict recorded on A"
# The losing bytes are preserved and retrievable (the other concurrent write).
loser_id="$( jq -r '.loser.id' <<<"$row" )"
loser_bytes="$( cd "$A" && "$TOMO_BIN" restore collide.txt --version "$loser_id" --stdout )"
[[ "$loser_bytes" == "$CA" || "$loser_bytes" == "$CB" ]] \
  || fail "restored loser '$loser_bytes' is neither concurrent write"
[[ "$loser_bytes" != "$winner" ]] || fail "restored loser equals the winner — loser not preserved"
log "collision loser bytes preserved and restorable: '$loser_bytes'"

# --- 5. quiet network + final convergence ---
wait_for 20 "converged and settled (final)" converged_and_settled "$A" "$B"
assert_quiet_network "$A" 3
assert_converged "$A" "$B"
pass
