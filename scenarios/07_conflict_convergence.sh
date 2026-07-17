#!/usr/bin/env bash
# Scenario 07 — Conflict convergence (Tier 2, M4)
# Spec: docs/TESTING.md row 07; docs/SPEC.md §5.3 (MVR heads, deterministic LWW:
# Present beats Tombstone, then higher content hash). Concurrent edits to the
# same path on both sides must converge to the IDENTICAL winner with zero
# negotiation, the loser preserved in history and surfaced non-blockingly.
#
# PLAN:
#  1. link A↔B, seed files both sides know, converge + settle.
#  2. Partition: SIGSTOP the local serve child (parts the link; A's live watch
#     keeps recording and queues its frame into the pipe buffer, B's serve is
#     frozen so B's inotify events queue in the kernel).
#  3. While partitioned write DIFFERENT content to the same paths on B then A
#     (B first so B's queued inotify is drained ahead of A's incoming frame on
#     heal — otherwise the incoming apply overwrites B's bytes before B records
#     them). A third path writes the SAME content pair with the sides REVERSED.
#  4. SIGCONT to heal → wait_for identical content + roots both sides.
#  5. Assert winner is one of the two writes, identical on both sides, and
#     SIDE-INDEPENDENT: the reversed-role third path selects the SAME winning
#     content — the observable signature of the content-hash tiebreak.
#  6. conflicts_unresolved ≥ 1 both sides; conflicts list --json carries
#     id/path/winner/loser; loser bytes retrievable via `restore --stdout`.
#  7. `conflicts resolve <id> --keep-current` acks one → unresolved drops; the
#     status ⚠ badge appears while unresolved and clears once all are acked.
#  8. Quiet network + assert_converged.

source "$(dirname "$0")/lib/harness.sh"
scenario_init
require_cli
[[ "${TOMO_LINK_MODE:-local}" == "ssh" ]] && skip "07 partitions the local serve child; ssh link mode not supported"

A="$(make_machine a)"
B="$(make_machine b)"

if [[ -n "${TOMO_SCENARIO_LAG:-}" ]]; then
  netem_delay "$TOMO_SCENARIO_LAG" || skip "netem unavailable for lag variant"
fi

WATCH="$(link_machines "$A" "$B")"

# --- Partition control: SIGSTOP/SIGCONT the served peer (child of the watch). --
# The serve child is the process that watches B and applies A's frames; freezing
# it parts the link. Register a cleanup fn so a still-stopped child is CONTinued
# and killed on teardown even if we fail mid-partition (a stopped process ignores
# SIGTERM until it runs again).
SERVE="$(pgrep -P "$WATCH" -x tomo || true)"
[[ -n "$SERVE" ]] || fail "could not find serve child of watch pid $WATCH"
cleanup_serve() { [[ -n "${SERVE:-}" ]] && { kill -CONT "$SERVE" 2>/dev/null; kill -KILL "$SERVE" 2>/dev/null; } || true; }
register_cleanup_fn cleanup_serve
part()  { kill -STOP "$SERVE"; }
heal()  { kill -CONT "$SERVE"; }

# Two contents whose winner we do NOT hardcode: the deterministic hash tiebreak
# picks one, and the scenario asserts side-independence rather than a fixed side.
CA="alpha-edit-from-A-side"
CB="bravo-edit-from-B-side"
CA2="second-content-from-A"
CB2="second-content-from-B"

# --- 1. seed files both sides know, converge + settle ---
echo "base-original"   > "$A/base.txt"
echo "second-original" > "$A/second.txt"
echo "third-original"  > "$A/third.txt"
wait_for 10 "base seed A→B"   assert_file_content "$B/base.txt"   "base-original"
wait_for 10 "second seed A→B" assert_file_content "$B/second.txt" "second-original"
wait_for 10 "third seed A→B"  assert_file_content "$B/third.txt"  "third-original"
wait_for 15 "seed converged" converged_and_settled "$A" "$B"
settle_status "$A" "$B"

# --- 2/3. partition and write concurrent, conflicting edits ---
part
# B side first (its inotify must be recorded before A's incoming frame applies).
echo "$CB"  > "$B/base.txt"    # base:  B writes CB
echo "$CB2" > "$B/second.txt"  # second: B writes CB2
echo "$CA"  > "$B/third.txt"   # third (REVERSED roles): B writes what A writes to base
# A side second.
echo "$CA"  > "$A/base.txt"    # base:  A writes CA
echo "$CA2" > "$A/second.txt"  # second: A writes CA2
echo "$CB"  > "$A/third.txt"   # third (REVERSED roles): A writes what B writes to base
# --- 4. heal ---
heal

# --- 5. converge to an identical winner on both sides ---
# Generous timeout: on heal B processes a burst (its queued inotify + A's frames).
wait_for 30 "reconverged after heal" converged_and_settled "$A" "$B"

for f in base.txt second.txt third.txt; do
  cmp -s "$A/$f" "$B/$f" || fail "sides disagree on winner for $f: A='$(cat "$A/$f")' B='$(cat "$B/$f")'"
done

base_winner="$(cat "$A/base.txt")"
second_winner="$(cat "$A/second.txt")"
third_winner="$(cat "$A/third.txt")"

# winner must be exactly one of the two concurrent writes (nothing invented/merged).
[[ "$base_winner"   == "$CA"  || "$base_winner"   == "$CB"  ]] || fail "base winner '$base_winner' is neither concurrent write"
[[ "$second_winner" == "$CA2" || "$second_winner" == "$CB2" ]] || fail "second winner '$second_winner' is neither concurrent write"

# SIDE-INDEPENDENCE (the content-hash tiebreak, observed without computing hashes):
# base.txt and third.txt raced the SAME pair {CA,CB} with the sides swapped. The
# winner must be the same CONTENT regardless of which side authored it.
[[ "$third_winner" == "$CA" || "$third_winner" == "$CB" ]] || fail "third winner '$third_winner' is neither concurrent write"
[[ "$third_winner" == "$base_winner" ]] \
  || fail "winner is NOT side-independent: base picked '$base_winner' but reversed-role third picked '$third_winner'"
log "deterministic winner for {CA,CB} = '$base_winner' (side-independent: base and reversed-role third agree)"

# --- 6. conflicts recorded, surfaced, loser retrievable ---
unresolved_a() { ( cd "$A" && "$TOMO_BIN" status --json 2>/dev/null ) | jq -r '.conflicts_unresolved'; }
unresolved_b() { ( cd "$B" && "$TOMO_BIN" status --json 2>/dev/null ) | jq -r '.conflicts_unresolved'; }
wait_for 15 "A records conflicts" bash -c "[[ \"\$( cd '$A' && '$TOMO_BIN' status --json | jq -r .conflicts_unresolved )\" -ge 1 ]]"
wait_for 15 "B records conflicts" bash -c "[[ \"\$( cd '$B' && '$TOMO_BIN' status --json | jq -r .conflicts_unresolved )\" -ge 1 ]]"

# conflicts list --json structure: id/path/winner/loser with the head metadata.
list_a="$( cd "$A" && "$TOMO_BIN" conflicts list --json )"
[[ "$( jq 'length' <<<"$list_a" )" -ge 1 ]] || fail "A conflicts list empty"
jq -e '.[] | select(has("id") and has("path") and has("winner") and has("loser")
        and (.winner|has("id") and has("content_hash"))
        and (.loser |has("id")))' <<<"$list_a" >/dev/null \
  || fail "conflict rows missing id/path/winner/loser structure"

# Loser bytes must be retrievable byte-exact from history and must be the write
# that did NOT win (the preserved-loser guarantee, invariant #5).
base_row="$( jq -c '[.[] | select(.path=="base.txt")][0]' <<<"$list_a" )"
[[ "$base_row" != "null" ]] || fail "no base.txt conflict recorded on A"
loser_id="$( jq -r '.loser.id' <<<"$base_row" )"
loser_bytes="$( cd "$A" && "$TOMO_BIN" restore base.txt --version "$loser_id" --stdout )"
[[ "$loser_bytes" == "$CA" || "$loser_bytes" == "$CB" ]] || fail "restored loser '$loser_bytes' is neither concurrent write"
[[ "$loser_bytes" != "$base_winner" ]] || fail "restored loser equals the winner — loser not preserved"
log "loser bytes for base.txt preserved and byte-exact via restore --stdout: '$loser_bytes'"

# --- 7. resolve one → unresolved drops; badge appears then clears ---
u_before="$(unresolved_a)"
[[ "$u_before" -ge 1 ]] || fail "expected ≥1 unresolved on A before resolve"
# ⚠ badge present in human status while unresolved.
( cd "$A" && "$TOMO_BIN" status ) | grep -q '⚠' || fail "status badge (⚠) missing while conflicts unresolved"

conflict_id="$( jq -r '.[0].id' <<<"$list_a" )"
( cd "$A" && "$TOMO_BIN" conflicts resolve "$conflict_id" --keep-current ) >/dev/null \
  || fail "conflicts resolve --keep-current failed"
wait_for 10 "unresolved count drops after resolve" \
  bash -c "[[ \"\$( cd '$A' && '$TOMO_BIN' status --json | jq -r .conflicts_unresolved )\" -lt $u_before ]]"

# Ack the rest, then the badge must be gone (non-blocking surfacing, cleanly cleared).
( cd "$A" && "$TOMO_BIN" conflicts resolve --all --keep-current ) >/dev/null || fail "mass-ack failed"
wait_for 10 "all conflicts acked on A" \
  bash -c "[[ \"\$( cd '$A' && '$TOMO_BIN' status --json | jq -r .conflicts_unresolved )\" -eq 0 ]]"
( cd "$A" && "$TOMO_BIN" status ) | grep -q '⚠' && fail "status badge (⚠) still shown after acking all conflicts" || true

# --- 8. quiet network + final convergence ---
# Acking is a DB-only op (tree untouched, --keep-current) so the link stays quiet.
assert_quiet_network "$A" 3
wait_for 15 "converged and settled (final)" converged_and_settled "$A" "$B"
assert_converged "$A" "$B"
pass
