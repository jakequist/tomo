#!/usr/bin/env bash
# Scenario 08 — Delete-vs-edit conflict (Tier 2, M4)
# Spec: docs/TESTING.md row 08; docs/SPEC.md §5.3 — delete-vs-edit is a conflict
# like any other, resolved by the deterministic LWW rule "Present beats
# Tombstone": the EDIT wins on both sides, the delete is preserved as the losing
# head, and the edited content is retained in history regardless of outcome.
#
# PLAN:
#  1. link A↔B, seed victim files, converge + settle.
#  2. Partition (SIGSTOP the serve child, as in 07).
#  3. One side deletes while the other edits the same path (edit written first so
#     the concurrent edit is recorded before the incoming delete/frame applies).
#     A second path runs the REVERSED orientation to prove side-independence.
#  4. Heal (SIGCONT) → both sides converge to the EDIT (Present beats Tombstone),
#     file present with the editor's bytes on BOTH sides.
#  5. Conflict recorded; the delete survives as the losing tombstone head in
#     `tomo log`; the conflict is visible/unresolved.
#  6. Quiet network + assert_converged.

source "$(dirname "$0")/lib/harness.sh"
scenario_init
require_cli
[[ "${TOMO_LINK_MODE:-local}" == "ssh" ]] && skip "08 partitions the local serve child; ssh link mode not supported"

A="$(make_machine a)"
B="$(make_machine b)"

if [[ -n "${TOMO_SCENARIO_LAG:-}" ]]; then
  netem_delay "$TOMO_SCENARIO_LAG" || skip "netem unavailable for lag variant"
fi

WATCH="$(link_machines "$A" "$B")"

SERVE="$(pgrep -P "$WATCH" -x tomo || true)"
[[ -n "$SERVE" ]] || fail "could not find serve child of watch pid $WATCH"
cleanup_serve() { [[ -n "${SERVE:-}" ]] && { kill -CONT "$SERVE" 2>/dev/null; kill -KILL "$SERVE" 2>/dev/null; } || true; }
register_cleanup_fn cleanup_serve
part() { kill -STOP "$SERVE"; }
heal() { kill -CONT "$SERVE"; }

EDIT_B="victim-edited-on-B"   # B edits victim.txt (A deletes it)
EDIT_A="other-edited-on-A"    # A edits other.txt  (B deletes it) — reversed orientation

# --- 1. seed + converge ---
echo "victim-original" > "$A/victim.txt"
echo "other-original"  > "$A/other.txt"
wait_for 10 "victim seed A→B" assert_file_content "$B/victim.txt" "victim-original"
wait_for 10 "other seed A→B"  assert_file_content "$B/other.txt"  "other-original"
wait_for 15 "seed converged" converged_and_settled "$A" "$B"
settle_status "$A" "$B"

# --- 2/3. partition, then delete-vs-edit (edit written first) ---
part
# victim.txt: B edits, A deletes.  other.txt: A edits, B deletes (reversed).
echo "$EDIT_B" > "$B/victim.txt"   # edit first (B side)
echo "$EDIT_A" > "$A/other.txt"    # edit first (A side)
rm "$A/victim.txt"                 # delete second (A side)
rm "$B/other.txt"                  # delete second (B side)
# --- 4. heal ---
heal

# Convergence oracle first (generous: heal triggers a burst on B).
wait_for 30 "reconverged after heal" converged_and_settled "$A" "$B"

# Present beats Tombstone: the EDIT wins on BOTH sides for both orientations.
wait_for 10 "victim edit wins on A"  assert_file_content "$A/victim.txt" "$EDIT_B"
wait_for 10 "victim edit wins on B"  assert_file_content "$B/victim.txt" "$EDIT_B"
wait_for 10 "other edit wins on A"   assert_file_content "$A/other.txt"  "$EDIT_A"
wait_for 10 "other edit wins on B"   assert_file_content "$B/other.txt"  "$EDIT_A"
log "delete-vs-edit: edit won on both sides for both orientations (Present beats Tombstone, side-independent)"

# --- 5. conflict recorded; delete preserved as the losing tombstone head ---
wait_for 15 "B records conflicts" \
  bash -c "[[ \"\$( cd '$B' && '$TOMO_BIN' status --json | jq -r .conflicts_unresolved )\" -ge 1 ]]"

# The conflict's winner is Present (the edit), the loser is a tombstone (the delete).
row="$( cd "$B" && "$TOMO_BIN" conflicts list --json | jq -c '[.[] | select(.path=="victim.txt")][0]' )"
[[ "$row" != "null" ]] || fail "no victim.txt conflict recorded on B"
[[ "$( jq -r '.winner.present' <<<"$row" )" == "true"  ]] || fail "conflict winner is not Present (edit should win)"
[[ "$( jq -r '.loser.tombstone' <<<"$row" )" == "true" ]] || fail "conflict loser is not a tombstone (the delete should be the loser)"

# The delete survives in history as a tombstone version of victim.txt on B.
jq -e 'any(.[]; .tombstone == true)' \
  <(cd "$B" && "$TOMO_BIN" log victim.txt --json) >/dev/null \
  || fail "tombstone (the losing delete) not preserved in victim.txt history on B"
# The edited content is likewise retained in history (present version recorded).
jq -e 'any(.[]; .present == true)' \
  <(cd "$B" && "$TOMO_BIN" log victim.txt --json) >/dev/null \
  || fail "edited content not preserved in victim.txt history on B"
log "delete preserved as losing tombstone head; edited content retained in history"

# --- 6. quiet network + final convergence ---
assert_quiet_network "$A" 3
wait_for 15 "converged and settled (final)" converged_and_settled "$A" "$B"
assert_converged "$A" "$B"
pass
