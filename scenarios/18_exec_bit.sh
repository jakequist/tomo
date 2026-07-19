#!/usr/bin/env bash
# Scenario 18 — the executable bit syncs (Tier 1/2, protocol v2)
# Spec: docs/SPEC.md §12 (permissions v0 subset = the Unix user-execute bit,
# git's model). Motivates the flagship artifact-flowback use case: a build
# artifact created on the Linux server must arrive on the Mac still executable,
# and a `chmod +x` must propagate as a real change on its own.
#
# Four things must hold (all two-way-capable; here exercised in the directions
# that matter for the use case):
#   (a) a pre-link executable script on A arrives executable on B (and a plain
#       file arrives non-executable — the bit is not spuriously set);
#   (b) a chmod +x ALONE on an already-synced file propagates A→B (content
#       unchanged), and a chmod -x propagates back;
#   (c) simulated artifact flow: a B-side "build" writes an executable, and it
#       arrives executable on A (the killer B→A case);
#   (d) `tomo restore` of an older executable version restores the bit on disk.
#
# The harness's assert_converged compares file CONTENTS and index roots; the
# index root now encodes the exec bit (protocol v2), so a mode-only divergence
# would fail roots_equal — this scenario additionally asserts the on-disk mode
# directly with `test -x`, since content comparison alone would not catch it.

source "$(dirname "$0")/lib/harness.sh"
scenario_init
require_cli
ensure_jq

# SSH is only needed for the ssh link mode; the local M1 link uses stdio pipes.
[[ "${TOMO_LINK_MODE:-local}" == "ssh" ]] && ensure_self_ssh

# Predicates (wait_for-friendly): is / is-not executable for the current user.
is_exec()  { [[ -x "$1" ]]; }
not_exec() { [[ -f "$1" && ! -x "$1" ]]; }

A="$(make_machine a)"
B="$(make_machine b)"

# Optional lag variant (run-all --lag 50ms).
if [[ -n "${TOMO_SCENARIO_LAG:-}" ]]; then
  netem_delay "$TOMO_SCENARIO_LAG" || skip "netem unavailable for lag variant"
fi

# ---------------------------------------------------------------------------
# (a) A pre-link executable script arrives executable on B; a plain file does
#     not become executable.
# ---------------------------------------------------------------------------
log "(a) pre-link +x script propagates as executable; plain file stays plain"
printf '#!/bin/sh\necho hi\n' > "$A/run.sh"
chmod +x "$A/run.sh"
printf 'just data\n' > "$A/data.txt"   # deliberately NOT executable

link_machines "$A" "$B" >/dev/null

wait_for 20 "run.sh reaches B" assert_file_content "$B/run.sh" "$(cat "$A/run.sh")"
wait_for 20 "run.sh is executable on B" is_exec "$B/run.sh"
wait_for 20 "data.txt reaches B" assert_file_content "$B/data.txt" "just data"
# The plain file must NOT have gained the execute bit.
wait_for 10 "data.txt stays non-executable on B" not_exec "$B/data.txt"

# ---------------------------------------------------------------------------
# (b) chmod +x ALONE on a synced file propagates (content unchanged), then -x.
# ---------------------------------------------------------------------------
log "(b) chmod +x alone propagates A→B (content unchanged), then chmod -x back"
chmod +x "$A/data.txt"
wait_for 20 "chmod +x propagates A→B" is_exec "$B/data.txt"
# The bytes must be untouched by the mode-only change.
assert_file_content "$B/data.txt" "just data" || fail "content changed by a chmod-only sync"

chmod -x "$A/data.txt"
wait_for 20 "chmod -x propagates A→B" not_exec "$B/data.txt"
assert_file_content "$B/data.txt" "just data" || fail "content changed by a chmod -x sync"

# ---------------------------------------------------------------------------
# (c) Simulated artifact flow: a B-side build writes an executable → arrives
#     executable on A (the flagship B→A artifact-flowback case).
# ---------------------------------------------------------------------------
log "(c) B-side 'build' writes an executable artifact → arrives executable on A"
mkdir -p "$B/dist"
printf '#!/bin/sh\necho built-on-b\n' > "$B/dist/app"
chmod +x "$B/dist/app"
wait_for 20 "artifact reaches A" assert_file_content "$A/dist/app" "$(cat "$B/dist/app")"
wait_for 20 "artifact is executable on A" is_exec "$A/dist/app"

# ---------------------------------------------------------------------------
# (d) `tomo restore` of an older executable version restores the bit.
#     data.txt's history now holds: plain(v1, non-exec), +x(v2, exec),
#     -x(v3, non-exec). Restoring the executable version must chmod it back.
# ---------------------------------------------------------------------------
log "(d) tomo restore of an older executable version restores the bit on disk"
# Wait until history has recorded an executable version of data.txt. Predicate
# functions run in the current shell (wait_for calls "$@" directly), so they see
# $TOMO_BIN/$A — do NOT wrap them in a fresh `bash -c`, which would not.
# History only GUARANTEES the final state of a burst (invariant #4): on slow
# runners the (b) +x/-x pair can coalesce inside one adaptive capture window,
# legitimately dropping the intermediate exec version. So make the exec state
# a SETTLED FINAL state before capturing its id: chmod +x again, wait for the
# NEWEST recorded version to be exec (guaranteed), then chmod -x back.
exec_version_id() {
  ( cd "$A" && "$TOMO_BIN" log data.txt --json 2>/dev/null ) \
    | jq -r 'map(select(.present and .exec)) | .[0].id // empty' 2>/dev/null
}
newest_exec_state() {
  ( cd "$A" && "$TOMO_BIN" log data.txt --json 2>/dev/null ) \
    | jq -r 'if length > 0 then (.[0].exec | tostring) else "" end' 2>/dev/null
}
newest_is_exec()    { [[ "$(newest_exec_state)" == "true" ]]; }
newest_is_nonexec() { [[ "$(newest_exec_state)" == "false" ]]; }
chmod +x "$A/data.txt"
wait_for 20 "newest recorded data.txt version is executable (final state)" newest_is_exec
VID="$(exec_version_id)"
[[ -n "$VID" ]] || fail "no executable version of data.txt found in history (exec bit not versioned?)"
chmod -x "$A/data.txt"
wait_for 20 "newest recorded data.txt version is non-exec again" newest_is_nonexec
log "  restoring data.txt to version #$VID (an executable state)"
# Precondition: it is currently non-executable (from the chmod -x above).
not_exec "$A/data.txt" || fail "data.txt should be non-executable before the restore"
( cd "$A" && "$TOMO_BIN" restore data.txt --version "$VID" ) || fail "tomo restore failed"
wait_for 15 "restore made data.txt executable again on A" is_exec "$A/data.txt"
# The live session re-syncs the restored (executable) state back to B.
wait_for 20 "restored executable state propagates A→B" is_exec "$B/data.txt"

# ---------------------------------------------------------------------------
# Convergence — content, index roots (which now encode the exec bit), and the
# cross-cutting invariants. Wait for the roots to settle first.
# ---------------------------------------------------------------------------
wait_for 20 "index roots converge" roots_equal "$A" "$B"
assert_converged "$A" "$B"

# Final direct mode assertions on both sides (content comparison alone would not
# catch a mode-only divergence).
is_exec  "$A/run.sh"   || fail "run.sh lost its exec bit on A"
is_exec  "$B/run.sh"   || fail "run.sh not executable on B"
is_exec  "$A/dist/app" || fail "artifact not executable on A"
is_exec  "$B/dist/app" || fail "artifact not executable on B"
is_exec  "$A/data.txt" || fail "restored data.txt not executable on A"
is_exec  "$B/data.txt" || fail "restored data.txt not executable on B"

pass
