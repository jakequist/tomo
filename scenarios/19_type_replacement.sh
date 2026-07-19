#!/usr/bin/env bash
# Scenario 19 — File↔dir type replacement (Tier 1, edge-case 5)
# Spec: docs/SPEC.md §5.4 (File↔dir type replacement); docs/NOTES.md edge-case 5.
#
# A path can flip between being a file and being a directory. The deterministic
# rule is **the directory wins**: a directory is the implicit container of one or
# more present synced descendants (real data), so a file colliding with it is
# preserved to history and its head converges to a tombstone. This is a
# structural property of the (converged) index — no clocks, no replica identity —
# so both replicas reach the identical state without negotiation.
#
# PLAN:
#  (a) file→dir online: A replaces file `alpha` with a directory `alpha/`(+kids)
#      → B converges to the directory; the OLD file bytes stay retrievable from
#        B's history (a present version) and the file head is a tombstone.
#  (b) dir→file online: A replaces directory `beta/`(+kids) with a same-named
#      file `beta` → B converges to the file; the children are tombstoned.
#  (c) concurrent opposite-type while PARTED: A creates file `gamma`, B creates
#      dir `gamma/`(+child) → heal → BOTH sides converge to the SAME state
#        (directory wins) with the losing file preserved in history on BOTH.
#
# Runs under the local link (it partitions the serve child, exactly like 07/08).

source "$(dirname "$0")/lib/harness.sh"
scenario_init
require_cli
[[ "${TOMO_LINK_MODE:-local}" == "ssh" ]] && skip "19 partitions the local serve child; ssh link mode not supported"

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

# has_present DIR RELPATH — history log records a present version of the path.
has_present() { jq -e 'any(.[]; .present == true)'   <(hist_json "$1" "$2") >/dev/null; }
# has_tombstone DIR RELPATH — history log records a tombstone version.
has_tombstone() { jq -e 'any(.[]; .tombstone == true)' <(hist_json "$1" "$2") >/dev/null; }

# =====================================================================
# (a) file → dir  (online)
# =====================================================================
echo "alpha-original" > "$A/alpha"
wait_for 10 "alpha seeds A→B"   assert_file_content "$B/alpha" "alpha-original"
wait_for 15 "alpha seed converged" converged_and_settled "$A" "$B"
settle_status "$A" "$B"

# Replace the file with a directory holding two children.
rm "$A/alpha"
mkdir "$A/alpha"
echo "one" > "$A/alpha/one"
echo "two" > "$A/alpha/two"

wait_for 20 "alpha/one propagates"      assert_file_content "$B/alpha/one" "one"
wait_for 20 "alpha/two propagates"      assert_file_content "$B/alpha/two" "two"
wait_for 20 "B's alpha is now a directory" bash -c "[[ -d '$B/alpha' && ! -f '$B/alpha' ]]"
wait_for 20 "file→dir converged"        converged_and_settled "$A" "$B"
log "(a) file→dir: B converged to the directory (directory wins)"

# The OLD file bytes are retrievable from B's history; the file head is a tombstone.
wait_for 10 "B keeps the old alpha file in history" has_present    "$B" "alpha"
wait_for 10 "B tombstones the alpha file head"      has_tombstone  "$B" "alpha"
log "(a) old file bytes retrievable from B history; file head tombstoned"

# =====================================================================
# (b) dir → file  (online)
# =====================================================================
mkdir "$A/beta"
echo "beta-one" > "$A/beta/one"
echo "beta-two" > "$A/beta/two"
wait_for 15 "beta/one seeds A→B" assert_file_content "$B/beta/one" "beta-one"
wait_for 15 "beta/two seeds A→B" assert_file_content "$B/beta/two" "beta-two"
wait_for 15 "beta dir converged" converged_and_settled "$A" "$B"
settle_status "$A" "$B"

# Replace the directory with a same-named file.
rm -r "$A/beta"
echo "beta-is-now-a-file" > "$A/beta"

wait_for 20 "beta becomes a file on B" bash -c "[[ -f '$B/beta' ]]"
wait_for 20 "beta content propagates"  assert_file_content "$B/beta" "beta-is-now-a-file"
wait_for 20 "beta children gone on B"  bash -c "[[ ! -e '$B/beta/one' && ! -e '$B/beta/two' ]]"
wait_for 20 "dir→file converged"       converged_and_settled "$A" "$B"
log "(b) dir→file: B converged to the file"

# The children are tombstoned in B's history; the new file is a present version.
wait_for 10 "B tombstones beta/one"       has_tombstone "$B" "beta/one"
wait_for 10 "B tombstones beta/two"       has_tombstone "$B" "beta/two"
wait_for 10 "B records the beta file"     has_present   "$B" "beta"
log "(b) children tombstoned in B history; new file recorded"

# =====================================================================
# (c) concurrent opposite-type creation while PARTED (directory wins)
# =====================================================================
settle_status "$A" "$B"
part
# A creates a FILE `gamma`; B creates a DIRECTORY `gamma/` with a child.
echo "gamma-file-on-A" > "$A/gamma"
mkdir "$B/gamma"
echo "child-on-B" > "$B/gamma/x"
heal

# Deterministic convergence: the directory wins on BOTH sides.
wait_for 40 "reconverged after heal" converged_and_settled "$A" "$B"
wait_for 15 "A converges to the directory" bash -c "[[ -d '$A/gamma' && ! -f '$A/gamma' ]]"
wait_for 15 "B keeps the directory"        bash -c "[[ -d '$B/gamma' && ! -f '$B/gamma' ]]"
wait_for 15 "gamma/x present on A" assert_file_content "$A/gamma/x" "child-on-B"
wait_for 15 "gamma/x present on B" assert_file_content "$B/gamma/x" "child-on-B"
log "(c) concurrent file-vs-dir: directory won deterministically on BOTH sides"

# The losing file is preserved in history on BOTH replicas, and its head is a
# tombstone (converged to the directory).
wait_for 15 "A preserves the losing gamma file"   has_present   "$A" "gamma"
wait_for 15 "B preserves the losing gamma file"   has_present   "$B" "gamma"
wait_for 15 "A tombstones the gamma file head"    has_tombstone "$A" "gamma"
wait_for 15 "B tombstones the gamma file head"    has_tombstone "$B" "gamma"
log "(c) losing file preserved in history on both sides; file head tombstoned"

# =====================================================================
# (d) file → symlink  (online) — docs/SPEC.md §5.4 "File→symlink replacement"
#
# Symlinks are never synced in v0: a tracked regular file replaced by a symlink
# is observed as a DELETION of that file. So B must tombstone the file AND keep
# its last regular-file bytes recoverable in history (invariant #5); the symlink
# itself never crosses the wire (B ends with no `delta` at all).
# =====================================================================
settle_status "$A" "$B"
echo "delta-original-bytes" > "$A/delta"
wait_for 15 "delta seeds A→B"      assert_file_content "$B/delta" "delta-original-bytes"
wait_for 15 "delta seed converged" converged_and_settled "$A" "$B"
settle_status "$A" "$B"

# Replace the file with a symlink (dangling target — never followed, never synced).
rm "$A/delta"
ln -s /nonexistent-symlink-target "$A/delta"
[[ -L "$A/delta" ]] || fail "A/delta should now be a symlink"

# The file's deletion propagates: B removes `delta` (as a file) and the symlink
# never appears on B in any form.
wait_for 20 "B removes the delta file (file→symlink = deletion)" \
  bash -c "[[ ! -e '$B/delta' ]]"
wait_for 20 "file→symlink converged" converged_and_settled "$A" "$B"
log "(d) file→symlink: B removed delta; symlink not synced"

# B tombstones the file head AND the old file bytes remain in history (invariant #5).
wait_for 10 "B keeps the old delta bytes in history" has_present   "$B" "delta"
wait_for 10 "B tombstones the delta file head"       has_tombstone "$B" "delta"
log "(d) old file bytes retrievable from B history; file head tombstoned"

# --- final: quiet network + convergence oracle ---
assert_quiet_network "$A" 3
wait_for 15 "converged and settled (final)" converged_and_settled "$A" "$B"
assert_converged "$A" "$B"
pass
