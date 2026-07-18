#!/usr/bin/env bash
# Scenario 04 — Remote bootstrap matrix (Tier 1, milestone M2)
# Spec: docs/SPEC.md §3; acceptance row: docs/TESTING.md scenario 04.
#
# Exercises the real SSH bootstrap against a temp "remote" over self-SSH to
# localhost (so SFTP push, chmod, SHA-256 verify, stale pruning, and the version
# handshake are genuinely run). Four labeled sub-checks; the scenario passes only
# if all four hold:
#
#   a. Fresh remote → connect → the correct-arch binary is pushed to
#      `.tomo/bin/tomo-<version>-<triple>`, executable, SHA-256 == the local
#      pushed source binary (dev-mode substitution makes the pushed file
#      byte-identical to this CLI's own binary), handshake reported OK.
#   b. Matching binary present → a `tomo watch` session reuses it (says
#      up-to-date), and the file's inode+mtime are unchanged (no re-push).
#   c. Local version bumped one patch (TOMO_TEST_FORCE_LOCAL_VERSION=0.0.2) →
#      a NEW tomo-0.0.2-* is pushed and the stale 0.0.1 is pruned (exactly one
#      tomo-* remains, named 0.0.2).
#   d. Unsupported arch (TOMO_TEST_FORCE_REMOTE_TRIPLE=junk) on a fresh remote →
#      connect fails cleanly (non-zero), stderr names the unsupported target,
#      and NOTHING is pushed (no .tomo/bin/tomo-*); no external download.

source "$(dirname "$0")/lib/harness.sh"
scenario_init
require_cli
ensure_jq
ensure_self_ssh

USER_AT="$(whoami)@localhost"
VER="$("$TOMO_BIN" --version | awk '{print $2}')" # e.g. "0.0.1"
[[ -n "$VER" ]] || fail "could not determine local tomo version"

# --- small helpers, CLI-only, wait_for-friendly ---------------------------
remote_bins() { # DIR → newline list of tomo-* files in its .tomo/bin (may be empty)
  ( cd "$1/.tomo/bin" 2>/dev/null && ls -1 2>/dev/null | grep '^tomo-' ) || true
}
count_bins() { remote_bins "$1" | grep -c . ; }
only_bin_is() { # DIR VERSION → true iff exactly one tomo-* present, named tomo-<VERSION>-*
  local files; files="$(remote_bins "$1")"
  [[ "$(printf '%s' "$files" | grep -c .)" == "1" ]] && printf '%s\n' "$files" | grep -q "^tomo-${2}-"
}
kill_watch() { local p="$1"; [[ -n "$p" ]] && kill "$p" 2>/dev/null; wait "$p" 2>/dev/null || true; }

A="$(make_machine a)"
B="$(make_machine b)"
( cd "$A" && "$TOMO_BIN" init >/dev/null ) || fail "init A"
( cd "$B" && "$TOMO_BIN" init >/dev/null ) || fail "init B"

# ===========================================================================
# (a) Fresh remote → connect → correct binary pushed, verified, handshake OK.
# ===========================================================================
log "CHECK a: fresh remote → connect pushes + verifies binary, handshake OK"
CONNECT_OUT="$WORK/connect_a.log"
if ( cd "$A" && "$TOMO_BIN" connect "$USER_AT" "$B" ) >"$CONNECT_OUT" 2>&1; then
  :
else
  cat "$CONNECT_OUT" >&2
  fail "a: connect to fresh remote exited non-zero"
fi

[[ "$(count_bins "$B")" == "1" ]] \
  || { remote_bins "$B" >&2; fail "a: expected exactly one pushed binary on B"; }
BIN_NAME="$(remote_bins "$B")"
[[ "$BIN_NAME" == tomo-"$VER"-* ]] \
  || fail "a: pushed binary '$BIN_NAME' is not named tomo-$VER-<triple>"
BIN_PATH="$B/.tomo/bin/$BIN_NAME"
[[ -x "$BIN_PATH" ]] || fail "a: pushed binary '$BIN_NAME' is not executable"

SHA_LOCAL="$(sha256sum "$TOMO_BIN" | awk '{print $1}')"
SHA_REMOTE="$(sha256sum "$BIN_PATH" | awk '{print $1}')"
[[ "$SHA_LOCAL" == "$SHA_REMOTE" ]] \
  || fail "a: pushed SHA-256 ($SHA_REMOTE) != local source binary ($SHA_LOCAL)"

grep -q 'bootstrap: pushed' "$CONNECT_OUT" \
  || { cat "$CONNECT_OUT" >&2; fail "a: connect did not report a push"; }
grep -q 'connection OK' "$CONNECT_OUT" \
  || { cat "$CONNECT_OUT" >&2; fail "a: handshake not reported OK"; }
log "  a OK: pushed $BIN_NAME (exec, SHA matches local), handshake OK"

# ===========================================================================
# (b) Matching binary present → watch reuses it, no re-push (inode+mtime same).
# ===========================================================================
log "CHECK b: matching binary → reuse, no re-push"
INODE_BEFORE="$(stat_inode "$BIN_PATH")"
MTIME_BEFORE="$(stat_mtime "$BIN_PATH")"
WLOG="$WORK/a.reuse.watch.log"
( cd "$A" && exec "$TOMO_BIN" watch ) >"$WLOG" 2>&1 &
WPID=$!
register_pid "$WPID"
wait_for 20 "b: A reports connected (reused binary)" status_connected "$A"
wait_for 20 "b: B reports connected (reused binary)" status_connected "$B"
wait_for 10 "b: watch log reports the binary is up to date" \
  grep -q 'up to date' "$WLOG"
INODE_AFTER="$(stat_inode "$BIN_PATH")"
MTIME_AFTER="$(stat_mtime "$BIN_PATH")"
[[ "$INODE_BEFORE" == "$INODE_AFTER" ]] \
  || fail "b: binary inode changed ($INODE_BEFORE → $INODE_AFTER) — it was re-pushed"
[[ "$MTIME_BEFORE" == "$MTIME_AFTER" ]] \
  || fail "b: binary mtime changed ($MTIME_BEFORE → $MTIME_AFTER) — it was re-pushed"
[[ "$(count_bins "$B")" == "1" ]] || fail "b: bin dir no longer holds exactly one binary"
kill_watch "$WPID"
log "  b OK: reused (inode $INODE_AFTER, mtime unchanged), no re-push"

# ===========================================================================
# (c) Version off by one patch → NEW binary pushed, stale one pruned.
# ===========================================================================
log "CHECK c: local version 0.0.2 → push new, prune stale 0.0.1"
NEWVER="0.0.2"
[[ "$NEWVER" != "$VER" ]] || fail "c: forced version equals real version; adjust the test"
WLOG2="$WORK/a.bump.watch.log"
( cd "$A" && TOMO_TEST_FORCE_LOCAL_VERSION="$NEWVER" exec "$TOMO_BIN" watch ) >"$WLOG2" 2>&1 &
WPID2=$!
register_pid "$WPID2"
# The forced-version handshake never fully converges (the spawned remote binary
# still reports its real version), so we assert on the bootstrap's disk effect,
# not on connectivity: exactly one binary remains and it is the new version.
wait_for 25 "c: new tomo-$NEWVER pushed and stale $VER pruned" only_bin_is "$B" "$NEWVER"
# The push note is printed just AFTER the on-disk prune completes, so poll for it
# rather than racing the flush right after the disk check.
wait_for 10 "c: watch log reports pushing tomo $NEWVER" \
  grep -q "pushed remote binary tomo $NEWVER" "$WLOG2"
kill_watch "$WPID2"
log "  c OK: only tomo-$NEWVER-<triple> remains ($(remote_bins "$B"))"

# ===========================================================================
# (d) Unsupported arch → clean failure, nothing pushed, no external download.
# ===========================================================================
log "CHECK d: unsupported remote triple → clean failure, nothing pushed"
JUNK_TRIPLE="sparc64-unknown-linux-gnu"
AU="$(make_machine au)"
BU="$(make_machine bu)"
( cd "$AU" && "$TOMO_BIN" init >/dev/null ) || fail "init AU"
( cd "$BU" && "$TOMO_BIN" init >/dev/null ) || fail "init BU"
DOUT="$WORK/connect_d.log"
if ( cd "$AU" && TOMO_TEST_FORCE_REMOTE_TRIPLE="$JUNK_TRIPLE" \
      "$TOMO_BIN" connect "$USER_AT" "$BU" ) >"$DOUT" 2>&1; then
  cat "$DOUT" >&2
  fail "d: connect to an unsupported-arch remote unexpectedly succeeded"
fi
grep -qi 'unsupported' "$DOUT" \
  || { cat "$DOUT" >&2; fail "d: failure message does not say 'unsupported'"; }
grep -q "$JUNK_TRIPLE" "$DOUT" \
  || { cat "$DOUT" >&2; fail "d: failure message does not name the detected target ($JUNK_TRIPLE)"; }
grep -qi 'no external downloads' "$DOUT" \
  || { cat "$DOUT" >&2; fail "d: failure message does not affirm no external downloads"; }
[[ "$(count_bins "$BU")" == "0" ]] \
  || { remote_bins "$BU" >&2; fail "d: something was pushed to the unsupported remote"; }
[[ ! -d "$BU/.tomo/bin" ]] || [[ -z "$(ls -A "$BU/.tomo/bin" 2>/dev/null)" ]] \
  || fail "d: remote .tomo/bin is not empty after a failed unsupported-arch connect"
log "  d OK: clean non-zero failure naming $JUNK_TRIPLE, nothing pushed"

log "all four bootstrap sub-checks held"
pass
