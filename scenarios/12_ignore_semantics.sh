#!/usr/bin/env bash
# Scenario 12 — Ignore rules are load-bearing (Tier 2, M5)
# Spec: docs/TESTING.md row 12; docs/SPEC.md §7 (path classes). An `ignored`
# rule (`target/`, trailing-slash form → `target/**`) means the tree NEVER
# crosses the wire and NEVER grows history — the load-bearing guard against a
# server build spraying `target/` at build speed. Flip the rule off and the same
# tree becomes ordinary synced+versioned content.
#
# Config is loaded at watch STARTUP (there is no live re-read), so the ignore
# rule must exist before the link comes up, and flipping it requires restarting
# the affected side's watch. This scenario documents and exercises exactly that.
#
# PLAN:
#  1. init A & B; write the `target/` ignore rule into BOTH configs BEFORE the
#     link starts; bring the link up by hand (link_machines starts the watch
#     itself, so we drive init+watch directly to seat the config first).
#  2. Sanity: an ordinary file still syncs. Converge + settle; snapshot the wire
#     frame counter and the recorded-history counter.
#  3. Generate ~200 MiB across many files under target/ on A, with a background
#     writer hammering target/ throughout an observation window. Assert the wire
#     stays perfectly quiet (frames never move), target/ never appears on B, and
#     history records ZERO versions for anything under target/.
#  4. Explicitly assert the `.tomo/**` hardcoded ignore: no A-side `.tomo`
#     artifact ever leaks into B's tree.
#  5. Flip the rule off on A (remove it) and restart A's watch. The startup scan
#     picks target/ up; assert it now syncs to B and gains history versions.
#  6. assert_converged.

source "$(dirname "$0")/lib/harness.sh"
scenario_init
require_cli
[[ "${TOMO_LINK_MODE:-local}" == "ssh" ]] && skip "12 restarts A's local-peer watch to reload config; ssh link mode not supported"
ensure_jq

A="$(make_machine a)"
B="$(make_machine b)"

if [[ -n "${TOMO_SCENARIO_LAG:-}" ]]; then
  netem_delay "$TOMO_SCENARIO_LAG" || skip "netem unavailable for lag variant"
fi

# --- 1. init both, seat the ignore rule in BOTH configs before the link ---
( cd "$A" && "$TOMO_BIN" init >/dev/null 2>&1 ) || fail "init A"
( cd "$B" && "$TOMO_BIN" init >/dev/null 2>&1 ) || fail "init B"
# Preserve the pristine config so the later flip is an exact removal of the rule.
PRISTINE_A="$(cat "$A/.tomo/config.toml")"
append_ignore_rule() { # DIR
  printf '\n[[rules]]\npattern = "target/"\nclass = "ignored"\n' >> "$1/.tomo/config.toml"
}
append_ignore_rule "$A"
append_ignore_rule "$B"

WATCH="$(start_watch "$A" --local-peer "$B")"
wait_for 15 "A connected" status_connected "$A"
wait_for 15 "B connected" status_connected "$B"

# --- 2. sanity: an ordinary file still syncs; converge + settle; snapshot ---
echo "ordinary" > "$A/normal.txt"
wait_for 10 "ordinary file syncs A→B" assert_file_content "$B/normal.txt" "ordinary"
wait_for 15 "converged and settled" converged_and_settled "$A" "$B"
settle_status "$A" "$B"

hist_versions() { ( cd "$1" && "$TOMO_BIN" status --json 2>/dev/null ) | jq -r '.history.versions_recorded'; }
FRAMES_BEFORE="$(net_frames "$A")"
VERS_BEFORE="$(hist_versions "$A")"
[[ -n "$FRAMES_BEFORE" ]] || fail "no wire counters on A (not connected?)"
log "baseline: wire frames=$FRAMES_BEFORE, history versions=$VERS_BEFORE"

# --- 3. spray ~200 MiB under target/ and prove the wire + history stay flat ---
mkdir -p "$A/target/deep"
for i in $(seq 1 20); do
  dd if=/dev/urandom of="$A/target/deep/obj_$i.bin" bs=1M count=10 status=none
done
# Adversarial: keep churning target/ during the whole quiet-network observation.
churn_target() { local n=0; while :; do echo "build-$n" > "$A/target/churn.log"; n=$((n+1)); sleep 0.1; done; }
churn_target & CHURN_PID=$!
register_pid "$CHURN_PID"

# The load-bearing assertion: not a single frame crosses the wire while target/
# is being written and churned (assert_quiet_network settles first, then fails if
# the counter EVER moves over the window).
assert_quiet_network "$A" 5
kill "$CHURN_PID" 2>/dev/null || true

# target/ never materialized on B.
assert_absent "$B/target" || fail "target/ leaked onto B despite the ignore rule"
# Zero history growth: no versions recorded for any target/ path, and the global
# recorded-versions counter is unchanged from the pre-spray baseline.
( cd "$A" && "$TOMO_BIN" log target/deep/obj_1.bin --json >/dev/null 2>&1 ) \
  && fail "history recorded a version for an ignored target/ file"
VERS_AFTER="$(hist_versions "$A")"
[[ "$VERS_AFTER" == "$VERS_BEFORE" ]] \
  || fail "history grew for ignored target/ writes: $VERS_BEFORE → $VERS_AFTER"
log "target/ ignored: wire quiet, B has no target/, history flat ($VERS_AFTER versions)"

# --- 4. .tomo/** hardcoded ignore: no A-side .tomo artifact leaks into B ---
[[ -e "$B/a" || -e "$B/.tomo/config.toml.a" ]] && fail "unexpected A artifacts under B"
find "$B" -path "$B/.tomo" -prune -o -name '.tomo' -print | grep -q . \
  && fail ".tomo directory leaked into B's synced tree" || true

# ===========================================================================
# 5. flip the rule OFF on A, restart A's watch → target/ now syncs + versions.
# ===========================================================================
log "flipping target/ ignore OFF on A and restarting its watch (config reloads at startup)"
# Stop A's watch (SIGTERM → graceful shutdown reaps the serve child), wait for it
# to fully exit so the restart brings up a single clean link.
kill "$WATCH" 2>/dev/null || true
wait_for 15 "A's watch exits before restart" bash -c "! kill -0 $WATCH 2>/dev/null"
# Restore the pristine config (rule removed).
printf '%s\n' "$PRISTINE_A" > "$A/.tomo/config.toml"

WATCH="$(start_watch "$A" --local-peer "$B")"
# Generous timeout: with the rule gone, A's startup scan now hashes the whole
# ~200 MiB target/ tree BEFORE it reports connected, and the debug build's
# unoptimized BLAKE3 takes ~18s to do that on slower hosts (macOS measured it;
# the Linux dev VM squeaked under 15s). The scan work is the point of the flip,
# not a hang — give it room rather than racing an -O0 hashing pass.
wait_for 60 "A reconnected after flip" status_connected "$A"
wait_for 60 "B reconnected after flip" status_connected "$B"

# The startup scan now treats target/ as ordinary content: it syncs to B (B still
# carries its own ignore rule, which governs only what B ORIGINATES — incoming
# applies still land) and gains history versions on A.
wait_for 30 "a target/ file now syncs to B" \
  assert_same_content "$A/target/deep/obj_1.bin" "$B/target/deep/obj_1.bin"
wait_for 20 "target/ file gains history on A" hist_count_ge "$A" "target/deep/obj_1.bin" 1
log "after flip: target/ syncs and is versioned"

# --- 6. final convergence ---
wait_for 30 "converged and settled (final)" converged_and_settled "$A" "$B"
assert_converged "$A" "$B"
pass
