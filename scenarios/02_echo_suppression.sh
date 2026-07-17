#!/usr/bin/env bash
# Scenario 02 — Echo suppression / quiet network (Tier 1, M1)
# Spec: docs/TESTING.md — the quiet-network invariant. After a converged burst
# of two-way activity the link must fall silent: no echo loops, no phantom
# history growth, no resurrected deletes.
#
# PLAN:
#  1. link A↔B, drive a full cycle of changes both directions, converge.
#  2. Snapshot net frame counters + files/tombstones on both sides.
#  3. Observe a fixed window (assert_quiet_network) — counters must not move.
#  4. Assert no new files/tombstones appeared on either side (no echo storm).
#  5. Delete-resurrection: delete on A → gone on B → hold a window → assert it
#     did NOT reappear on either side.
#  6. assert_converged.

source "$(dirname "$0")/lib/harness.sh"
scenario_init
require_cli
[[ "${TOMO_LINK_MODE:-local}" == "ssh" ]] && ensure_self_ssh

A="$(make_machine a)"
B="$(make_machine b)"

if [[ -n "${TOMO_SCENARIO_LAG:-}" ]]; then
  netem_delay "$TOMO_SCENARIO_LAG" || skip "netem unavailable for lag variant"
fi

link_machines "$A" "$B" >/dev/null

status_field() { # DIR FIELD → integer from status --json
  ( cd "$1" && "$TOMO_BIN" status --json 2>/dev/null ) | jq -r ".$2"
}

# --- 1. full cycle of changes, both directions ---
echo "a-one"   > "$A/a1.txt"
echo "a-two"   > "$A/sub/a2.txt" 2>/dev/null || { mkdir -p "$A/sub"; echo "a-two" > "$A/sub/a2.txt"; }
echo "b-one"   > "$B/b1.txt"
wait_for 10 "a1 A→B" assert_file_content "$B/a1.txt" "a-one"
wait_for 10 "a2 A→B" assert_file_content "$B/sub/a2.txt" "a-two"
wait_for 10 "b1 B→A" assert_file_content "$A/b1.txt" "b-one"

echo "a-one-edited" > "$A/a1.txt"          # modify
echo "b-one-edited" > "$B/b1.txt"          # modify other direction
rm "$A/sub/a2.txt"                          # delete
wait_for 10 "a1 edit A→B"  assert_file_content "$B/a1.txt" "a-one-edited"
wait_for 10 "b1 edit B→A"  assert_file_content "$A/b1.txt" "b-one-edited"
wait_for 10 "a2 delete A→B" assert_absent "$B/sub/a2.txt"

# --- 2. converge, then snapshot ---
wait_for 10 "index roots converge" roots_equal "$A" "$B"

a_files_0="$(status_field "$A" files)";       b_files_0="$(status_field "$B" files)"
a_tombs_0="$(status_field "$A" tombstones)";  b_tombs_0="$(status_field "$B" tombstones)"

# --- 3. quiet network over the observation window (>= 3s) ---
assert_quiet_network "$A" 3

# --- 4. no new files/tombstones appeared (no echo storm) ---
a_files_1="$(status_field "$A" files)";       b_files_1="$(status_field "$B" files)"
a_tombs_1="$(status_field "$A" tombstones)";  b_tombs_1="$(status_field "$B" tombstones)"
[[ "$a_files_1" == "$a_files_0" && "$b_files_1" == "$b_files_0" ]] \
  || fail "file count changed during quiet window (A $a_files_0→$a_files_1, B $b_files_0→$b_files_1)"
[[ "$a_tombs_1" == "$a_tombs_0" && "$b_tombs_1" == "$b_tombs_0" ]] \
  || fail "tombstone count changed during quiet window (A $a_tombs_0→$a_tombs_1, B $b_tombs_0→$b_tombs_1)"

# --- 5. delete-resurrection ---
rm "$A/a1.txt"
wait_for 10 "a1 delete A→B" assert_absent "$B/a1.txt"
# Hold a window and confirm the deleted file does not come back on either side.
resurrect_check() { [[ ! -e "$A/a1.txt" && ! -e "$B/a1.txt" ]]; }
end=$(( $(date +%s) + 3 ))
while (( $(date +%s) < end )); do
  resurrect_check || fail "deleted file a1.txt resurrected during observation window"
  sleep 0.2
done

# --- 6. final convergence ---
wait_for 10 "index roots converge (post-delete)" roots_equal "$A" "$B"
assert_converged "$A" "$B"
pass
