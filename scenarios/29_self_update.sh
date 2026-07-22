#!/usr/bin/env bash
# Scenario 29 — `tomo update` self-update (content-addressed, mirrors installer)
# Spec: docs/SPEC.md §11 (Dependencies: ureq) and the self-update design
# (decided 2026-07-22). docs/TESTING.md row 29.
#
# No real network. We build a FAKE release directory — the freshly built tomo
# binary copied under this platform's stable asset name, plus a `SHA256SUMS`
# generated with `sha256sum` (exactly the installer's contract) — and serve it
# from localhost with `python3 -m http.server`. A pristine copy of the binary is
# placed at an "installed" location and driven with TOMO_UPDATE_BASE pointed at
# the local server (the documented test hook), exercising every path:
#
#   A. SHA256SUMS matches the installed binary  → "already up to date",
#      inode unchanged (no needless replace).
#   B. asset differs from the installed binary  → `--check` reports
#      "update available" WITHOUT touching it, then `update` REPLACES it (hash
#      now equals the served asset, exec bit set).
#   C. corrupt SHA256SUMS (a hash that matches nothing) → hard error, the
#      installed binary is untouched and NO staging debris is left beside it.
#   D. unreachable base → clean error, again no partial files beside the binary.
#
# Case B's differing "asset" is the same tomo binary with one trailing NUL byte
# appended: the ELF loader ignores trailing bytes, so it still runs `--version`
# (which the update's version report shells out to) while hashing differently —
# verified at setup; the code tolerates a non-running asset regardless.

source "$(dirname "$0")/lib/harness.sh"
scenario_init
require_cli

command -v python3 >/dev/null 2>&1 || skip "python3 required to serve the fake release"

# --- platform asset name (the same os/arch tags install.sh uses) -----------
case "$(uname -m)" in
  x86_64|amd64)  arch=x86_64 ;;
  aarch64|arm64) arch=arm64 ;;
  *) skip "unsupported architecture $(uname -m) for self-update" ;;
esac
case "$(uname -s)" in
  Linux)  os=linux ;;
  Darwin) os=macos ;;
  *) skip "unsupported OS $(uname -s) for self-update" ;;
esac
ASSET="tomo-${os}-${arch}"
log "platform asset: $ASSET"

# --- portable helpers -------------------------------------------------------
hash_of() { sha256sum "$1" | cut -d' ' -f1; }
count_partials() { find "$1" -maxdepth 1 -name '.tomo-update-*' | wc -l | tr -d ' '; }

REL="$WORK/release"     # the fake release dir the server exposes
INST="$WORK/installed"  # where the "installed" binary lives
mkdir -p "$REL" "$INST"

# Pristine build → the installed binary.
cp "$TOMO_BIN" "$INST/tomo"
chmod +x "$INST/tomo"

# --- serve the release dir over localhost -----------------------------------
PORT="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')"
python3 -m http.server "$PORT" --bind 127.0.0.1 --directory "$REL" \
  >"$WORK/http.log" 2>&1 &
SRV=$!
register_pid "$SRV"
BASE="http://127.0.0.1:$PORT"
server_up() { (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null; }
wait_for 10 "http server on :$PORT is up" server_up

run_update() { ( cd "$INST" && TOMO_UPDATE_BASE="$BASE" "$INST/tomo" "$@" ); }

# ===========================================================================
# Case A — SHA256SUMS matches the installed binary → already up to date.
# ===========================================================================
log "(A) matching SHA256SUMS → already up to date, inode unchanged"
cp "$TOMO_BIN" "$REL/$ASSET"
( cd "$REL" && sha256sum "$ASSET" > SHA256SUMS )
INODE_A="$(stat_inode "$INST/tomo")"
HASH_A="$(hash_of "$INST/tomo")"
OUT_A="$(run_update update 2>&1)" || fail "update (up-to-date) should exit 0: $OUT_A"
grep -q "already up to date" <<<"$OUT_A" || fail "expected 'already up to date', got: $OUT_A"
[[ "$(stat_inode "$INST/tomo")" == "$INODE_A" ]] || fail "up-to-date replaced the binary (inode changed)"
[[ "$(hash_of "$INST/tomo")" == "$HASH_A" ]] || fail "up-to-date altered the binary bytes"

# ===========================================================================
# Case B — a genuinely different asset → --check reports, update replaces.
# ===========================================================================
log "(B) differing asset → --check reports available (no touch), then update replaces"
cp "$TOMO_BIN" "$REL/$ASSET"
printf '\0' >> "$REL/$ASSET"                 # one trailing byte: still an ELF, new hash
chmod +x "$REL/$ASSET"
( cd "$REL" && sha256sum "$ASSET" > SHA256SUMS )
ASSET_HASH="$(hash_of "$REL/$ASSET")"
# Setup sanity: the appended-byte binary still runs (`update` shells out to it
# for the new-version line). If a future toolchain breaks this, the code still
# handles it — so only note it, do not fail.
"$REL/$ASSET" --version >/dev/null 2>&1 || log "note: appended-byte asset does not run --version (code tolerates it)"

INODE_B="$(stat_inode "$INST/tomo")"
HASH_B="$(hash_of "$INST/tomo")"
# --check must report availability and change NOTHING.
OUT_CHK="$(run_update update --check 2>&1)" || fail "update --check should exit 0: $OUT_CHK"
grep -q "update available" <<<"$OUT_CHK" || fail "expected 'update available' from --check, got: $OUT_CHK"
[[ "$(stat_inode "$INST/tomo")" == "$INODE_B" ]] || fail "--check replaced the binary (inode changed)"
[[ "$(hash_of "$INST/tomo")" == "$HASH_B" ]] || fail "--check altered the binary bytes"

# Full update must replace with the served asset.
OUT_UPD="$(run_update update 2>&1)" || fail "update should exit 0: $OUT_UPD"
grep -q "updated tomo" <<<"$OUT_UPD" || fail "expected 'updated tomo …', got: $OUT_UPD"
[[ "$(hash_of "$INST/tomo")" == "$ASSET_HASH" ]] || fail "binary not replaced with the served asset"
[[ -x "$INST/tomo" ]] || fail "replaced binary lost its exec bit"
[[ "$(count_partials "$INST")" == "0" ]] || fail "staging debris left after a successful update"

# A second run is now idempotent (up to date against the same asset).
OUT_B2="$(run_update update 2>&1)" || fail "second update should exit 0: $OUT_B2"
grep -q "already up to date" <<<"$OUT_B2" || fail "expected 'already up to date' after replace, got: $OUT_B2"

# ===========================================================================
# Case C — corrupt SHA256SUMS (hash matches nothing) → hard error, untouched.
# ===========================================================================
log "(C) corrupt SHA256SUMS → hard error, binary untouched, no debris"
cp "$TOMO_BIN" "$INST/tomo"                  # fresh pristine install
chmod +x "$INST/tomo"
cp "$TOMO_BIN" "$REL/$ASSET"; printf '\0' >> "$REL/$ASSET"   # asset differs → download proceeds
printf '%064d  %s\n' 0 "$ASSET" > "$REL/SHA256SUMS"          # a hash that matches nothing
INODE_C="$(stat_inode "$INST/tomo")"
HASH_C="$(hash_of "$INST/tomo")"
if run_update update >"$WORK/c.out" 2>&1; then
  fail "corrupt SHA256SUMS should fail: $(cat "$WORK/c.out")"
fi
grep -qi "checksum mismatch" "$WORK/c.out" || fail "expected a checksum-mismatch error, got: $(cat "$WORK/c.out")"
[[ "$(stat_inode "$INST/tomo")" == "$INODE_C" ]] || fail "checksum-mismatch replaced the binary"
[[ "$(hash_of "$INST/tomo")" == "$HASH_C" ]] || fail "checksum-mismatch altered the binary"
[[ "$(count_partials "$INST")" == "0" ]] || fail "staging debris left after a checksum mismatch"

# ===========================================================================
# Case D — unreachable base → clean error, no partial files beside the binary.
# ===========================================================================
log "(D) unreachable base → clean error, no debris"
DEAD_PORT="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); p=s.getsockname()[1]; s.close(); print(p)')"
INODE_D="$(stat_inode "$INST/tomo")"
if ( cd "$INST" && TOMO_UPDATE_BASE="http://127.0.0.1:$DEAD_PORT" "$INST/tomo" update ) \
     >"$WORK/d.out" 2>&1; then
  fail "unreachable base should fail: $(cat "$WORK/d.out")"
fi
grep -qi "could not fetch" "$WORK/d.out" || fail "expected a fetch error, got: $(cat "$WORK/d.out")"
[[ "$(stat_inode "$INST/tomo")" == "$INODE_D" ]] || fail "unreachable base altered the binary"
[[ "$(count_partials "$INST")" == "0" ]] || fail "staging debris left after an unreachable base"

pass
