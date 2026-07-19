#!/usr/bin/env bash
# Scenario 20 — macOS↔Linux filename semantics (Tier-1 edge case 3)
# Spec: docs/NOTES.md ledger item 3; docs/HANDOFF-MACOS.md "Filename semantics".
#
# The flagship pairing is macOS (APFS: case-insensitive by default, and a
# *normalizing* filesystem that returns NFD names from readdir) ↔ Linux
# (case-sensitive, byte-preserving). Two hazards, and what this scenario proves
# on Linux (this VM has no APFS — the real-APFS legs are validated in the Mac
# session, see docs/HANDOFF-MACOS.md):
#
#   (a) NFC/NFD non-normalization: names that are the NFC and NFD encodings of
#       the SAME visual string are DISTINCT byte sequences. On a byte-preserving
#       FS (Linux) Tomo must NOT over-normalize — both must sync as two files.
#   (b) Case pair Foo/foo: on case-sensitive Linux these are two files and sync
#       fine both ways with no guard, no conflict.
#   (c) Case-collision guard: with the local FS FORCED case-insensitive on B
#       (debug hook TOMO_TEST_FORCE_FS, cfg(debug_assertions) only), A shipping
#       `Foo.txt` then `foo.txt` (different bytes) must have B keep the first,
#       REFUSE the second, preserve the refused bytes in history, note it, count
#       it as a conflict, and stay connected — with A entirely unaffected.
#
# Run 3× for stability; passes under both TOMO_LINK_MODE=local (default) and ssh.

source "$(dirname "$0")/lib/harness.sh"
scenario_init
require_cli
ensure_jq

MODE="${TOMO_LINK_MODE:-local}"

# Distinct byte sequences for the same visual "café" — precomposed (NFC, one
# scalar U+00E9) vs decomposed (NFD, 'e' + combining acute U+0301).
NFC_NAME="$(printf 'caf\xc3\xa9.txt')"
NFD_NAME="$(printf 'cafe\xcc\x81.txt')"

bring_up_link() { # A_DIR B_DIR → echoes the driving sync pid
  local a="$1" b="$2" pid
  case "$MODE" in
    local) pid="$(start_sync "$a" --local-peer "$b")" ;;
    ssh)   ensure_self_ssh; pid="$(start_sync "$a" "$(whoami)@localhost" "$b")" ;;
    *)     fail "unknown TOMO_LINK_MODE: $MODE (expected 'local' or 'ssh')" ;;
  esac
  wait_for 45 "A ($a) connected" status_connected "$a"
  wait_for 45 "B ($b) connected" status_connected "$b"
  printf '%s\n' "$pid"
}

conflicts_count() { # DIR → session conflict count from status.json
  ( cd "$1" && "$TOMO_BIN" status --json 2>/dev/null ) | jq -r '.conflicts // 0'
}
status_files() { ( cd "$1" && "$TOMO_BIN" status --json 2>/dev/null ) | jq -r '.files // 0'; }

if [[ -n "${TOMO_SCENARIO_LAG:-}" ]]; then
  netem_delay "$TOMO_SCENARIO_LAG" || skip "netem unavailable for lag variant"
fi

# ===========================================================================
# Phase A+B — a byte-preserving, case-sensitive FS (plain Linux): NFC/NFD and
# Foo/foo pairs are all DISTINCT and all sync both ways. No normalization, no
# collapse, no conflict.
# ===========================================================================
log "PHASE A/B: NFC≠NFD and Foo≠foo are distinct files on Linux; all sync"
A="$(make_machine ab_a)"
B="$(make_machine ab_b)"
( cd "$A" && "$TOMO_BIN" init >/dev/null 2>&1 ) || fail "init A"
( cd "$B" && "$TOMO_BIN" init >/dev/null 2>&1 ) || fail "init B"

# (a) NFC/NFD pair with DIFFERENT content, so a wrongful collapse would lose one.
printf 'i-am-nfc\n' > "$A/$NFC_NAME"
printf 'i-am-nfd\n' > "$A/$NFD_NAME"
# (b) case pair with DIFFERENT content.
printf 'upper\n' > "$A/Case.txt"
printf 'lower\n' > "$A/case.txt"

WATCH="$(bring_up_link "$A" "$B")"

# All four names arrive on B, byte-faithful and DISTINCT.
wait_for 30 "NFC name reaches B"  assert_file_content "$B/$NFC_NAME" "i-am-nfc"
wait_for 30 "NFD name reaches B"  assert_file_content "$B/$NFD_NAME" "i-am-nfd"
wait_for 30 "Case.txt reaches B"  assert_file_content "$B/Case.txt"  "upper"
wait_for 30 "case.txt reaches B"  assert_file_content "$B/case.txt"  "lower"
wait_for 30 "converged and settled" converged_and_settled "$A" "$B"
settle_status "$A" "$B"

# Four distinct tracked files on each side — nothing collapsed.
[[ "$(status_files "$A")" == "4" ]] || fail "A should track 4 distinct files (over-normalized?)"
[[ "$(status_files "$B")" == "4" ]] || fail "B should track 4 distinct files (over-normalized?)"
# Zero conflicts anywhere: distinct names are not a collision on a case-sensitive FS.
[[ "$(conflicts_count "$A")" == "0" ]] || fail "A recorded a spurious conflict"
[[ "$(conflicts_count "$B")" == "0" ]] || fail "B recorded a spurious conflict"
# The NFC and NFD files must remain physically distinct (no collapse) on B.
[[ "$(cat "$B/$NFC_NAME")" != "$(cat "$B/$NFD_NAME")" ]] \
  || fail "NFC and NFD files collapsed on B — Tomo over-normalized a byte-preserving FS"
# status.json records the probed FS semantics (both false on Linux ext4/tmpfs).
fs_ci="$( ( cd "$B" && "$TOMO_BIN" status --json ) | jq -r '.fs.case_insensitive')"
fs_nu="$( ( cd "$B" && "$TOMO_BIN" status --json ) | jq -r '.fs.normalizes_unicode')"
[[ "$fs_ci" == "false" && "$fs_nu" == "false" ]] \
  || fail "B's probed fs semantics should be case-sensitive+byte-preserving on Linux (got ci=$fs_ci nu=$fs_nu)"

assert_converged "$A" "$B"
db_check_ok "$A" || fail "history db check failed on A"
db_check_ok "$B" || fail "history db check failed on B"
kill "$WATCH" 2>/dev/null || true
wait_for 15 "phase A/B link exits" bash -c "! kill -0 $WATCH 2>/dev/null"
log "  PHASE A/B OK: NFC/NFD and Foo/foo stayed distinct; converged; no conflicts"

# ===========================================================================
# Phase C — case-collision guard on a FORCED case-insensitive B.
# ===========================================================================
log "PHASE C: B forced case-insensitive → B keeps Foo.txt, refuses colliding foo.txt"
C_A="$(make_machine c_a)"
C_B="$(make_machine c_b)"
( cd "$C_A" && "$TOMO_BIN" init >/dev/null 2>&1 ) || fail "init C_A"
( cd "$C_B" && "$TOMO_BIN" init >/dev/null 2>&1 ) || fail "init C_B"

# Force case-insensitive FS semantics for this link. The debug hook is inherited
# by BOTH the sync (A) and the served-peer (B) processes, but the guard is an
# INBOUND-only check: A only SENDS these files, so forcing A is inert — we assert
# A is unaffected below. (A's real Linux FS is still case-sensitive, so Foo.txt
# and foo.txt are genuinely two files on A's disk.)
export TOMO_TEST_FORCE_FS="case-insensitive"

# A file that will be the keeper (arrives first), plus a normal control file so
# we can watch the healthy link independently of the collision.
printf 'FOO-first\n' > "$C_A/Foo.txt"
printf 'control\n'   > "$C_A/control.txt"

WATCH="$(bring_up_link "$C_A" "$C_B")"
wait_for 30 "control reaches B"        assert_file_content "$C_B/control.txt" "control"
wait_for 30 "Foo.txt (keeper) reaches B" assert_file_content "$C_B/Foo.txt"   "FOO-first"

# Now the colliding name with DIFFERENT bytes. On B (case-insensitive) foo.txt
# case-folds onto the already-present Foo.txt: it must be refused, not applied.
printf 'foo-SECOND-different\n' > "$C_A/foo.txt"

# B logs the collision to serve.log (serve's stdout is the wire, so notes go to
# the log). Wait for the exact refusal note.
BLOG="$C_B/.tomo/logs/serve.log"
wait_for 30 "B logs the case-collision refusal" \
  bash -c "grep -qi 'case collision' '$BLOG'"
grep -qi "kept 'Foo.txt', incoming preserved in history" "$BLOG" \
  || { tail -20 "$BLOG" >&2; fail "phase C: collision note missing the keep/preserve detail"; }

# The keeper is untouched; the refused name was NEVER written to B's disk.
assert_file_content "$C_B/Foo.txt" "FOO-first" || fail "phase C: keeper Foo.txt was clobbered"
[[ ! -f "$C_B/foo.txt" ]] \
  || fail "phase C: colliding foo.txt was written on B (guard did not refuse)"

# The refused bytes are preserved in history (recoverable via `tomo log`).
wait_for 15 "B preserved the refused foo.txt in history" \
  bash -c "( cd '$C_B' && '$TOMO_BIN' log foo.txt --json >/dev/null 2>&1 )"

# B counts it as a conflict and STAYS CONNECTED (never blocks sync, invariant #5).
wait_for 15 "B conflict count reflects the collision" \
  bash -c "[[ \"\$(cd '$C_B' && '$TOMO_BIN' status --json | jq -r '.conflicts // 0')\" -ge 1 ]]"
status_connected "$C_B" || fail "phase C: B disconnected over the collision"
grep -qi "error:" "$BLOG" && { tail -20 "$BLOG" >&2; fail "phase C: B errored on the collision"; } || true

# A is entirely unaffected: it holds BOTH real files with their distinct bytes,
# records NO conflict, and stays connected (the sender side never guards).
assert_file_content "$C_A/Foo.txt" "FOO-first"           || fail "phase C: A's Foo.txt changed"
assert_file_content "$C_A/foo.txt" "foo-SECOND-different" || fail "phase C: A's foo.txt changed"
[[ "$(conflicts_count "$C_A")" == "0" ]] || fail "phase C: A recorded a conflict (should be unaffected)"
status_connected "$C_A" || fail "phase C: A disconnected over the collision"

# The control file still round-trips after the collision (link is healthy).
printf 'control-v2\n' > "$C_A/control.txt"
wait_for 30 "control update still syncs after collision" \
  assert_file_content "$C_B/control.txt" "control-v2"

db_check_ok "$C_A" || fail "phase C: history db check failed on A"
db_check_ok "$C_B" || fail "phase C: history db check failed on B"
unset TOMO_TEST_FORCE_FS
kill "$WATCH" 2>/dev/null || true
wait_for 15 "phase C link exits" bash -c "! kill -0 $WATCH 2>/dev/null"
log "  PHASE C OK: B kept Foo.txt, refused+preserved foo.txt, stayed connected; A unaffected"

pass
