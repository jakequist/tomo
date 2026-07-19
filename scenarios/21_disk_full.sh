#!/usr/bin/env bash
# Scenario 21 — Disk-full degradation on the receiver (Tier 2)
# Spec: docs/NOTES.md tier-2 "disk-full degradation scenario"; invariants #5
# (conflicts/errors never block sync) and #8 (crash safety: never a partial file
# at a final path). Machine B's project lives on a tiny loopback tmpfs. We fill
# that filesystem, then push a file from A that cannot fit → B's apply hits
# ENOSPC. The receiver must:
#   - SURVIVE (no crash, no corruption), staying connected, with a loud note;
#   - leave NO partial file at the target's final path;
#   - pass `db check` on BOTH sides.
# Then we free the space (delete the filler) and the sync must self-heal:
#   - B re-requests the missing file automatically and converges.
#
# tmpfs needs root (sudo); if sudo/mount are unavailable the scenario SKIPs
# cleanly. On the sandbox VM sudo is present, so here it RUNS.
#
# Local link only: B's project root is the tmpfs mount, so we drive the link by
# hand (A `tomo sync --local-peer B`), exactly like scenario 12.

source "$(dirname "$0")/lib/harness.sh"
scenario_init
require_cli
ensure_jq
[[ "${TOMO_LINK_MODE:-local}" == "ssh" ]] && skip "21 roots B on a local tmpfs; ssh link mode not applicable"

if [[ -n "${TOMO_SCENARIO_LAG:-}" ]]; then
  netem_delay "$TOMO_SCENARIO_LAG" || skip "netem unavailable for lag variant"
fi

# --- tmpfs for machine B (needs sudo) ---
sudo -n true 2>/dev/null || skip "sudo unavailable — cannot mount a tmpfs for the disk-full test"
MOUNT="$WORK/bmount"
mkdir -p "$MOUNT"
# 24 MiB: room for B's .tomo overhead, the 8 MiB payload, AND its transient
# staging copy (chunks + atomic-write temp) once space is freed.
sudo mount -t tmpfs -o size=24m tmpfs "$MOUNT" 2>/dev/null || skip "tmpfs mount failed (no privilege?)"
mountpoint -q "$MOUNT" || skip "tmpfs did not actually mount at $MOUNT"
_umount_bfs() { sudo umount -l "$MOUNT" 2>/dev/null || true; }
register_cleanup_fn _umount_bfs

A="$(make_machine a)"
B="$MOUNT/proj"
mkdir -p "$B"

# --- init both, bring up the local link ---
( cd "$A" && "$TOMO_BIN" init >/dev/null 2>&1 ) || fail "init A"
( cd "$B" && "$TOMO_BIN" init >/dev/null 2>&1 ) || fail "init B (on tmpfs)"
WATCH="$(start_sync "$A" --local-peer "$B")"
wait_for 15 "A connected" status_connected "$A"
wait_for 15 "B connected" status_connected "$B"

# --- sanity: a small file syncs, both converge, db green ---
echo "hello" > "$A/small.txt"
wait_for 10 "small file syncs A→B" assert_file_content "$B/small.txt" "hello"
wait_for 15 "sanity converged" converged_and_settled "$A" "$B"
db_check_ok "$A" || fail "db check failed on A (baseline)"
db_check_ok "$B" || fail "db check failed on B (baseline)"
settle_status "$A" "$B"

# --- fill B's tmpfs, leaving less than the payload free ---
# Filler lives OUTSIDE B's project (same tmpfs, so it consumes the space B needs
# for an apply, but it is never synced). Leave ~2 MiB free.
avail_kb="$(df -k --output=avail "$MOUNT" | tail -1 | tr -d ' ')"
[[ -n "$avail_kb" && "$avail_kb" -gt 3072 ]] || fail "unexpected tmpfs free space: ${avail_kb}KB"
filler_kb=$(( avail_kb - 2048 ))
dd if=/dev/zero of="$MOUNT/filler.dat" bs=1024 count="$filler_kb" status=none 2>/dev/null || true
# Confirm the payload genuinely cannot fit — otherwise the test proves nothing.
free_kb="$(df -k --output=avail "$MOUNT" | tail -1 | tr -d ' ')"
(( free_kb < 8192 )) || fail "setup: expected <8 MiB free after filler, got ${free_kb}KB (filler failed?)"
log "filled B's tmpfs: ${free_kb}KB free (< 8 MiB payload)"

SERVE_LOG="$B/.tomo/logs/serve.log"

# --- push an 8 MiB file from A that cannot fit on B (forces a chunked transfer
#     whose chunk staging hits ENOSPC on B). Build it OUTSIDE the synced tree and
#     `mv` it in, so it appears at its full 8 MiB size ATOMICALLY — otherwise
#     dd's create-then-grow would let A ship a 0-byte intermediate first (which
#     fits on B and is a test artifact, not the payload we mean to stall). ---
dd if=/dev/urandom of="$WORK/big.staging" bs=1M count=8 status=none
mv "$WORK/big.staging" "$A/big.bin"
log "wrote 8 MiB big.bin on A atomically (exceeds B's free space)"

# The receiver notes the disk-full stall (served peer logs to serve.log).
wait_for 30 "B logs a disk-full stall" \
  bash -c "grep -qi 'disk full' '$SERVE_LOG'"
log "B reported the disk-full stall"

# B survives and A stays connected (the session did NOT die).
status_connected "$B" || fail "B's session died on disk-full (must survive — invariant #5)"
status_connected "$A" || fail "A dropped its peer on B's disk-full (should stay connected)"

# No partial file at big.bin's FINAL path on B (invariant #8).
assert_absent "$B/big.bin" || fail "a partial big.bin is visible at its final path on B"

# History DB integrity holds on BOTH sides despite the failed apply.
db_check_ok "$A" || fail "db check failed on A during the stall"
db_check_ok "$B" || fail "db check failed on B during the stall"
log "during stall: B alive, A connected, no partial file, db green on both"

# --- free the space; the sync must self-heal and converge ---
rm -f "$MOUNT/filler.dat"
log "freed B's tmpfs; expecting automatic re-request + convergence"

wait_for 90 "big.bin lands on B after space is freed" \
  assert_same_content "$A/big.bin" "$B/big.bin"
wait_for 60 "converged and settled after recovery" converged_and_settled "$A" "$B"
assert_converged "$A" "$B"
log "recovered: big.bin converged byte-for-byte after freeing space"
pass
