#!/usr/bin/env bash
# Scenario 05 — History fidelity under light load (Tier 2, M3)
# Spec: docs/TESTING.md row 05 + SPEC §6.2. Under light load the adaptive
# capture controller stays at rung 0 ("purity"): literally every save becomes a
# version. This scenario proves that N sequential edits yield exactly N
# retrievable, byte-identical versions on BOTH machines, that history is
# per-path independent, and that `tomo restore` undoes to the previous version
# and re-syncs it.
#
# Pacing note (no bare sleeps): each edit waits until BOTH sides have recorded
# the new version before the next edit lands. That both serializes the writes
# (keeping load light ⇒ rung 0 ⇒ every save versioned) and is a genuine poll,
# not a sleep-to-mask-a-race.

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

# Saved copies of every version live OUTSIDE the synced tree so they can never
# be perturbed by sync and serve as the ground truth for byte comparisons.
SAVED="$WORK/saved"
STAGE="$WORK/stage"
mkdir -p "$SAVED" "$STAGE"

# Publish one whole version atomically. Building the bytes in a staging file
# outside the watched tree and renaming into place means the watcher observes
# exactly ONE state per logical version — no intermediate truncate/partial-write
# bytes leak onto the live-sync path (which, being latency-first per invariant
# #3, would otherwise ship a transient state that the peer would faithfully
# version, inflating its history beyond the author's). This mirrors how real
# editors save; it is the correct way to drive "one edit = one version".
publish() { # DEST_ABS  SRC_STAGE
  mv "$2" "$1"
}

N=8

# --- 1. N sequential versions of doc.txt (distinct content, distinct sizes) ---
for i in $(seq 1 "$N"); do
  # Distinct content AND a distinct (increasing) size per version.
  { printf 'doc version %02d :' "$i"; for _ in $(seq 1 "$i"); do printf 'X'; done; printf '\n'; } \
    > "$STAGE/doc"
  cp "$STAGE/doc" "$SAVED/doc.$i"
  publish "$A/doc.txt" "$STAGE/doc"
  wait_for 15 "A records doc.txt version $i" hist_count_eq "$A" doc.txt "$i"
  wait_for 15 "B records doc.txt version $i" hist_count_eq "$B" doc.txt "$i"
  wait_for 15 "doc.txt v$i content reaches B" assert_same_content "$B/doc.txt" "$SAVED/doc.$i"
done

# --- 2. A second file with 3 versions proves per-path independence ---
M=3
for i in $(seq 1 "$M"); do
  printf 'note revision %d unique\n' "$i" > "$STAGE/note"
  cp "$STAGE/note" "$SAVED/note.$i"
  publish "$A/note.txt" "$STAGE/note"
  wait_for 15 "A records note.txt version $i" hist_count_eq "$A" note.txt "$i"
  wait_for 15 "B records note.txt version $i" hist_count_eq "$B" note.txt "$i"
done
# doc.txt history is untouched by note.txt activity (independent ledgers).
hist_count_eq "$A" doc.txt "$N" || fail "doc.txt history changed to $(hist_count "$A" doc.txt) while editing note.txt (expected $N)"
hist_count_eq "$A" note.txt "$M" || fail "note.txt should have $M versions, has $(hist_count "$A" note.txt)"

# --- 3. Capture the N-version snapshot of doc.txt on both sides ---
A_LOG="$(hist_json "$A" doc.txt)"
B_LOG="$(hist_json "$B" doc.txt)"

[[ "$(printf '%s' "$A_LOG" | jq 'length')" == "$N" ]] || fail "A doc.txt log length != $N"
[[ "$(printf '%s' "$B_LOG" | jq 'length')" == "$N" ]] || fail "B doc.txt log length != $N"

# Origins: A authored every version locally; B received every version remotely.
printf '%s' "$A_LOG" | jq -e 'all(.[]; .origin == "local")'  >/dev/null \
  || fail "A doc.txt has a non-local origin"
printf '%s' "$B_LOG" | jq -e 'all(.[]; .origin == "remote")' >/dev/null \
  || fail "B doc.txt has a non-remote origin"

# Version ids on A are strictly ascending in chronological (oldest→newest) order.
mapfile -t A_IDS < <(printf '%s' "$A_LOG" | jq -r 'sort_by(.id) | .[].id')
[[ "${#A_IDS[@]}" == "$N" ]] || fail "expected $N ids, got ${#A_IDS[@]}"
for ((k = 1; k < N; k++)); do
  (( A_IDS[k] > A_IDS[k-1] )) || fail "doc.txt ids not strictly ascending: ${A_IDS[*]}"
done
# log is emitted newest-first: the descending id order must be the reverse.
mapfile -t A_IDS_LOGORDER < <(printf '%s' "$A_LOG" | jq -r '.[].id')
for ((k = 0; k < N; k++)); do
  [[ "${A_IDS_LOGORDER[k]}" == "${A_IDS[N-1-k]}" ]] || fail "log not newest-first"
done

# --- 4. Every version restores byte-identical, with the right size, on A ---
# A_IDS is oldest→newest, i.e. A_IDS[i-1] is the version written at step i.
for i in $(seq 1 "$N"); do
  id="${A_IDS[i-1]}"
  ( cd "$A" && "$TOMO_BIN" restore doc.txt --version "$id" --stdout ) > "$WORK/restored.a" \
    || fail "restore --stdout failed for doc.txt v$id on A"
  cmp -s "$WORK/restored.a" "$SAVED/doc.$i" \
    || fail "A restore of version $id (step $i) is not byte-identical to what was written"
  want_size="$(stat_size "$SAVED/doc.$i")"
  got_size="$(printf '%s' "$A_LOG" | jq -r --argjson id "$id" '.[] | select(.id == $id) | .size')"
  [[ "$got_size" == "$want_size" ]] \
    || fail "A version $id size wrong: log says $got_size, file is $want_size"
done

# --- 5. B holds the same content: identical content-hash set, byte-identical restores ---
A_HASHES="$(printf '%s' "$A_LOG" | jq -r '[.[].content_hash] | sort | .[]')"
B_HASHES="$(printf '%s' "$B_LOG" | jq -r '[.[].content_hash] | sort | .[]')"
[[ "$A_HASHES" == "$B_HASHES" ]] \
  || { diff <(printf '%s\n' "$A_HASHES") <(printf '%s\n' "$B_HASHES") >&2; \
       fail "doc.txt content-hash sets differ between A and B"; }

mapfile -t B_IDS < <(printf '%s' "$B_LOG" | jq -r 'sort_by(.id) | .[].id')
for i in $(seq 1 "$N"); do
  id="${B_IDS[i-1]}"
  ( cd "$B" && "$TOMO_BIN" restore doc.txt --version "$id" --stdout ) > "$WORK/restored.b" \
    || fail "restore --stdout failed for doc.txt v$id on B"
  cmp -s "$WORK/restored.b" "$SAVED/doc.$i" \
    || fail "B restore of version $id (step $i) is not byte-identical to what A wrote"
done

# --- 6. `tomo restore` (undo default) returns version 7 content and syncs to B ---
# The undo default targets the version *before* the current newest — step N-1.
( cd "$A" && "$TOMO_BIN" restore doc.txt >/dev/null ) || fail "undo-default restore failed"
cmp -s "$A/doc.txt" "$SAVED/doc.$((N-1))" \
  || fail "undo-default restore did not reproduce step $((N-1)) content on disk"
# The restore is an ordinary local change: it becomes a new version and syncs.
wait_for 15 "restored (step $((N-1))) content reaches B" \
  assert_same_content "$B/doc.txt" "$SAVED/doc.$((N-1))"
wait_for 15 "A records the restore as version $((N+1))" hist_count_eq "$A" doc.txt "$((N+1))"

# --- 7. History DB integrity green on both sides ---
db_check_ok "$A" || fail "db check failed on A"
db_check_ok "$B" || fail "db check failed on B"

# --- 8. Final convergence ---
wait_for 15 "index roots converge" roots_equal "$A" "$B"
assert_converged "$A" "$B"
pass
