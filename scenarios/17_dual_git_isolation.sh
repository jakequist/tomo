#!/usr/bin/env bash
# Scenario 17 — two independent .git trees stay isolated (Tier 2, ux-nits)
# Spec: docs/SPEC.md §7 (built-in .git default ignore; ignore classes enforced on
# RECEIVE as well as send). Models a real first-use failure exactly: a Mac dir
# and a remote dir that EACH already contain their own, DIFFERENT `.git`. On the
# broken build, A pushed its .git up and reconcile pulled B's differing
# `.git/config` back down over A's — cross-contamination of two unrelated repos.
# Expected: the two .git dirs totally ignore each other.
#
# Three phases (all must hold; phase 3 is local-mode only — it restarts the link):
#   1. Symmetric isolation: A and B both default-configured, each with its OWN
#      differing .git. Link, converge the normal files. Neither .git tree is
#      touched, ZERO conflicts, NO .git history, the index counts exclude .git,
#      and the network is quiet.
#   2. Asymmetric ingress refusal: B re-includes `.git/**` (so B SHIPS its .git)
#      while A keeps defaults. A must REFUSE the inbound .git at ingress — nothing
#      .git lands on A, a dim note is logged, and A neither errors nor
#      disconnects (the normal file still round-trips).
#   3. Upgrade-inertness (newly-ignored guard): with .git re-included on both and
#      synced A→B, flipping A back to default (ignore) and restarting must NOT
#      mass-delete B's already-synced .git tree.

source "$(dirname "$0")/lib/harness.sh"
scenario_init
require_cli
ensure_jq

MODE="${TOMO_LINK_MODE:-local}"

# Bring up A⇄B in the configured link mode (local served-peer, or self-SSH).
# Both projects must already be init'd (so we can seat configs first).
bring_up_link() { # A_DIR B_DIR → echoes the driving sync pid
  local a="$1" b="$2" pid
  case "$MODE" in
    local) pid="$(start_sync "$a" --local-peer "$b")" ;;
    ssh)   ensure_self_ssh; pid="$(start_sync "$a" "$(whoami)@localhost:$b")" ;;
    *)     fail "unknown TOMO_LINK_MODE: $MODE (expected 'local' or 'ssh')" ;;
  esac
  wait_for 45 "A ($a) connected" status_connected "$a"
  wait_for 45 "B ($b) connected" status_connected "$b"
  printf '%s\n' "$pid"
}

# Seed a .git tree with a config line, HEAD, and a hook sample.
seed_git() { # DIR CONFIG_BYTES HOOK_BYTES
  mkdir -p "$1/.git/hooks" "$1/.git/objects"
  printf '%s\n' "$2"                 > "$1/.git/config"
  printf 'ref: refs/heads/main\n'    > "$1/.git/HEAD"
  printf '%s\n' "$3"                 > "$1/.git/hooks/pre-commit.sample"
}

status_files() { ( cd "$1" && "$TOMO_BIN" status --json 2>/dev/null ) | jq -r '.files // 0'; }
conflicts_count() {
  ( cd "$1" && "$TOMO_BIN" conflicts list --json 2>/dev/null ) | jq 'length' 2>/dev/null || echo 0
}
# assert a .git tree is byte-identical to a pre-link snapshot copy.
assert_git_identical() { # LIVE_DIR SNAPSHOT_DIR LABEL
  diff -r "$1/.git" "$2" >/dev/null 2>&1 \
    || { diff -r "$1/.git" "$2" | head -20 >&2; fail "$3: .git tree changed (cross-contamination)"; }
}

if [[ -n "${TOMO_SCENARIO_LAG:-}" ]]; then
  netem_delay "$TOMO_SCENARIO_LAG" || skip "netem unavailable for lag variant"
fi

# ===========================================================================
# Phase 1 — symmetric isolation (the user's exact case)
# ===========================================================================
log "PHASE 1: two differing .git trees, both sides default → total isolation"
A="$(make_machine p1a)"
B="$(make_machine p1b)"
( cd "$A" && "$TOMO_BIN" init >/dev/null 2>&1 ) || fail "init A"
( cd "$B" && "$TOMO_BIN" init >/dev/null 2>&1 ) || fail "init B"

# Each side's OWN .git, with DIFFERENT bytes at the same paths.
seed_git "$A" "A-side-config" "A-hook"
seed_git "$B" "B-side-config" "B-hook"
# A normal file on each side so real two-way syncing happens.
printf 'from-A\n' > "$A/normal_a.txt"
printf 'from-B\n' > "$B/normal_b.txt"

# Pre-link snapshots of each .git for an exact after-comparison.
SNAP_A="$WORK/snap_a_git"; cp -r "$A/.git" "$SNAP_A"
SNAP_B="$WORK/snap_b_git"; cp -r "$B/.git" "$SNAP_B"

WATCH="$(bring_up_link "$A" "$B")"

# The normal files converge both ways.
wait_for 30 "A's file reaches B" assert_file_content "$B/normal_a.txt" "from-A"
wait_for 30 "B's file reaches A" assert_file_content "$A/normal_b.txt" "from-B"
wait_for 30 "converged and settled" converged_and_settled "$A" "$B"
settle_status "$A" "$B"

# Neither .git tree moved a byte; no .git file leaked across.
assert_git_identical "$A" "$SNAP_A" "A"
assert_git_identical "$B" "$SNAP_B" "B"
[[ "$(cat "$A/.git/config")" == "A-side-config" ]] || fail "A's .git/config was overwritten"
[[ "$(cat "$B/.git/config")" == "B-side-config" ]] || fail "B's .git/config was overwritten"
# A must not have gained B's hook, nor vice-versa.
grep -q 'B-hook' "$A/.git/hooks/pre-commit.sample" && fail "B's hook contaminated A" || true
grep -q 'A-hook' "$B/.git/hooks/pre-commit.sample" && fail "A's hook contaminated B" || true

# ZERO conflicts on both sides.
[[ "$(conflicts_count "$A")" == "0" ]] || fail "A recorded a conflict over .git"
[[ "$(conflicts_count "$B")" == "0" ]] || fail "B recorded a conflict over .git"

# No .git history on either side.
( cd "$A" && "$TOMO_BIN" log .git/config --json >/dev/null 2>&1 ) \
  && fail "A recorded a .git history version"
( cd "$B" && "$TOMO_BIN" log .git/config --json >/dev/null 2>&1 ) \
  && fail "B recorded a .git history version"

# The index counts exclude .git: exactly the two normal files are tracked.
[[ "$(status_files "$A")" == "2" ]] || fail "A index files != 2 (.git leaked into the index?)"
[[ "$(status_files "$B")" == "2" ]] || fail "B index files != 2 (.git leaked into the index?)"

# Quiet network after convergence (no echo loop over the ignored trees).
assert_quiet_network "$A" 4
# NOTE: the harness's assert_converged compares EVERY non-.tomo file and would
# (correctly) flag the two DIFFERING .git trees — but that divergence is the
# whole point here. So we assert convergence on the synced surface only: equal
# index roots (which exclude .git) and healthy history DBs on both sides.
roots_equal "$A" "$B" || fail "index roots differ after convergence"
db_check_ok "$A" || fail "history db check failed on A"
db_check_ok "$B" || fail "history db check failed on B"
# And confirm the .git trees stayed DIVERGENT (proving no cross-contamination).
[[ "$(cat "$A/.git/config")" != "$(cat "$B/.git/config")" ]] \
  || fail "the two .git/config files converged — cross-contamination happened"
kill "$WATCH" 2>/dev/null || true
wait_for 15 "phase-1 link exits" bash -c "! kill -0 $WATCH 2>/dev/null"
log "  PHASE 1 OK: .git trees isolated, no conflicts, no history, index clean, quiet"

# ===========================================================================
# Phase 2 — asymmetric ingress refusal (B ships .git, A refuses on receive)
# ===========================================================================
log "PHASE 2: B re-includes .git and SHIPS it; A (default) refuses at ingress"
A2="$(make_machine p2a)"
B2="$(make_machine p2b)"
( cd "$A2" && "$TOMO_BIN" init >/dev/null 2>&1 ) || fail "init A2"
( cd "$B2" && "$TOMO_BIN" init >/dev/null 2>&1 ) || fail "init B2"
# B re-includes .git so its SEND side ships it; A keeps the built-in default.
# Re-including the tree needs both the bare `.git` (so the scan descends into the
# directory) and `.git/**` (its contents) — git's own re-include semantics.
reinc_git() { # DIR
  { printf '\n[[rules]]\npattern = ".git"\nclass = "synced+versioned"\n'
    printf '\n[[rules]]\npattern = ".git/**"\nclass = "synced+versioned"\n'
  } >> "$1/.tomo/config.toml"
}
reinc_git "$B2"
seed_git "$B2" "B2-side-config" "B2-hook"
printf 'from-B2\n' > "$B2/normal_b.txt"

WATCH="$(bring_up_link "$A2" "$B2")"
# The healthy link still moves ordinary content B→A…
wait_for 30 "B2's normal file reaches A2" assert_file_content "$A2/normal_b.txt" "from-B2"
# NOTE: the two indices legitimately DIFFER here — B tracks its re-included .git,
# A refuses it — so roots never equalize (that is the correct outcome). Wait only
# for each side's own status to go quiet, not for a shared root.
settle_status "$A2" "$B2"

# …but NONE of B's .git may land on A: A default-ignores it and enforces that on
# ingress (the peer shipping it does not override the receiver's ignore).
assert_absent "$A2/.git" || fail "B's .git landed on A despite A's ignore (ingress not enforced)"
[[ "$(status_files "$A2")" == "1" ]] || fail "A2 index should track only the one normal file"
# A logs a single dim refusal note for the .git prefix, and never errors.
ALOG="$WORK/p2a.watch.log"
grep -qi "not applying incoming .git" "$ALOG" \
  || { cat "$ALOG" >&2; fail "phase 2: no ingress-refusal note for .git on A"; }
grep -qi "error:" "$ALOG" && { cat "$ALOG" >&2; fail "phase 2: A errored while refusing .git"; } || true
status_connected "$A2" || fail "phase 2: A disconnected while refusing .git"
[[ "$(conflicts_count "$A2")" == "0" ]] || fail "phase 2: A recorded a conflict over refused .git"
kill "$WATCH" 2>/dev/null || true
wait_for 15 "phase-2 link exits" bash -c "! kill -0 $WATCH 2>/dev/null"
log "  PHASE 2 OK: A refused inbound .git (noted, no error, stayed connected)"

# ===========================================================================
# Phase 3 — upgrade-inertness (newly-ignored guard). Local mode only: it
# restarts the link to reload a flipped config, exactly as scenario 12 does.
# ===========================================================================
if [[ "$MODE" != "local" ]]; then
  log "  PHASE 3 skipped (needs a watch restart; only run under TOMO_LINK_MODE=local)"
  pass
fi

log "PHASE 3: .git synced A→B under a re-include, then A flips back to ignore →"
log "         B's synced .git must NOT be mass-deleted (newly-ignored guard)"
A3="$(make_machine p3a)"
B3="$(make_machine p3b)"
( cd "$A3" && "$TOMO_BIN" init >/dev/null 2>&1 ) || fail "init A3"
( cd "$B3" && "$TOMO_BIN" init >/dev/null 2>&1 ) || fail "init B3"
PRISTINE_A3="$(cat "$A3/.tomo/config.toml")"
# Both re-include .git so a real synced .git exists (index entries on both).
# Only A carries the .git tree, so there is no A/B .git conflict. Two rules per
# side: `.git` un-ignores the directory (so the scan descends) and `.git/**` its
# contents.
reinc() {
  { printf '\n[[rules]]\npattern = ".git"\nclass = "synced+versioned"\n'
    printf '\n[[rules]]\npattern = ".git/**"\nclass = "synced+versioned"\n'
  } >> "$1/.tomo/config.toml"
}
reinc "$A3"; reinc "$B3"
seed_git "$A3" "A3-config" "A3-hook"
printf 'seed\n' > "$A3/normal.txt"

WATCH="$(bring_up_link "$A3" "$B3")"
wait_for 30 "normal file syncs A3→B3" assert_file_content "$B3/normal.txt" "seed"
wait_for 30 ".git syncs A3→B3 under re-include" assert_file_content "$B3/.git/config" "A3-config"
wait_for 30 "converged and settled (phase 3 seed)" converged_and_settled "$A3" "$B3"
settle_status "$A3" "$B3"

# Now UPGRADE-equivalent: A flips .git back to the default ignore, B too, restart.
log "flipping .git back to ignore on both sides and restarting the link"
kill "$WATCH" 2>/dev/null || true
wait_for 15 "phase-3 link exits before restart" bash -c "! kill -0 $WATCH 2>/dev/null"
printf '%s\n' "$PRISTINE_A3" > "$A3/.tomo/config.toml"   # .git ignored again
printf '%s\n' "$PRISTINE_A3" > "$B3/.tomo/config.toml"

WATCH="$(bring_up_link "$A3" "$B3")"
# The newly-ignored guard (scan_diff): a now-ignored path present in the index is
# never emitted as a deletion, so re-ignoring .git must NOT propagate a removal
# that wipes the already-synced tree. The on-disk .git files on BOTH sides must
# survive the flip+restart untouched. (Index tracking legitimately drops .git —
# it is ignored now — so we assert the DISK invariant, not index convergence.)
settle_status "$A3" "$B3"
# Give a brief settling window, then confirm nothing deleted the .git trees.
wait_for 15 "normal.txt still converged after flip" \
  assert_same_content "$A3/normal.txt" "$B3/normal.txt"
[[ -f "$B3/.git/config" && "$(cat "$B3/.git/config")" == "A3-config" ]] \
  || fail "B's synced .git was mass-deleted/altered after A re-ignored it (guard failed)"
[[ -f "$A3/.git/config" && "$(cat "$A3/.git/config")" == "A3-config" ]] \
  || fail "A's own .git/config vanished after re-ignore"
[[ -f "$B3/.git/HEAD" && -f "$B3/.git/hooks/pre-commit.sample" ]] \
  || fail "part of B's synced .git tree was deleted after the flip"
log "  PHASE 3 OK: re-ignoring .git did not delete the already-synced tree"

pass
