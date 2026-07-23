#!/usr/bin/env bash
# Scenario 31 — kill -9 mid-seed, both sides (H2)
# Spec: docs/SEED-PERF.md §2 item H2; CLAUDE.md invariant #8 (crash safety via
# staging + atomic rename — kill -9 at ANY moment must not corrupt the tree or
# the history DB) applied to the BULK SEED shape. Extends scenario 09's kill/
# restart idioms to a many-file first-ever seed.
#
# The HARD crash-safety guarantees (invariant #8), asserted every part: the tree
# fully converges (every file byte-identical, index roots equal), `tomo db check`
# is GREEN on both sides (the DB is never corrupt), and no staging debris leaks.
#
# History INTEGRITY at the finer grain of "exactly one version per file per side"
# (the H1 ideal / H2's crash-retry-idempotency goal) is now a HARD assertion by
# default (TOMO_SEED_STRICT_HISTORY=1): SEED-PERF Phase 2 fixed BOTH crash-history
# findings this net originally surfaced as loud WARNINGs:
#   * receiver crash mid-seed  → was a PERMANENT receiver-side history GAP (files
#     landed + indexed on B but carried ZERO versions). FIXED (B1): the startup
#     reconcile detects index-present-but-history-absent paths and re-captures
#     them (bounded via the pressure controller), so the settled count is exactly
#     N — invariant #4 holds across the crash.
#   * sender crash + restart   → was DUPLICATE versions (the restart re-versioned
#     already-versioned files). FIXED (B2): the history store's v3
#     `versions_identity` UNIQUE index makes ingest idempotent, so a crash-retry
#     double-record is a no-op — the settled count is exactly N, never > N.
# db check stays green and the trees converge, as before. Set
# TOMO_SEED_STRICT_HISTORY=0 to demote the exactly-one check back to a WARNING
# (e.g. to reproduce the pre-fix baseline against an old binary).
#
# PLAN (three parts, each a fresh pair seeded from a shared deterministic tree):
#  A. Kill the RECEIVER (the served peer / applier child) once B has ~30% of the
#     files. Per M5 the driver survives, surfaces disconnected, auto-respawns the
#     serve child and resumes. Assert convergence + no duplicate versions + db green.
#  B. Kill the SENDER (the driving sync) at ~30%. The orphaned serve child EOFs
#     and exits; restart the link (scenario-09 part-A idiom). Same postconditions.
#  C. Repeated-kill loop: kill the receiver every ~5 s until the seed converges
#     ANYWAY. Same postconditions after.
#
# Local link only — it kills/respawns the local serve child (like 07/09/22).

source "$(dirname "$0")/lib/harness.sh"
scenario_init
require_cli
ensure_jq
[[ "${TOMO_LINK_MODE:-local}" == "ssh" ]] && skip "31 kills/respawns the local serve child; ssh link mode not supported"

SEED_FILES="${TOMO_SEED_FILES:-1200}"
# Generous per-part convergence budget: each respawn re-scans B's growing tree in
# the unoptimized debug build before resuming, so recovery is legitimately slow.
CONV_TIMEOUT="${TOMO_SEED_CONV_TIMEOUT:-240}"
# History completeness (exactly-one-per-side) is HARD by default: SEED-PERF
# Phase 2 fixed BOTH crash-history findings — B1 (receiver-crash gap: the startup
# reconcile re-captures index-present-but-history-absent files) and B2 (sender
# crash-retry duplicates: the store's v3 `versions_identity` index makes ingest
# idempotent). Set TOMO_SEED_STRICT_HISTORY=0 to demote back to a WARNING (e.g. to
# reproduce the pre-fix baseline against an old binary).
STRICT_HISTORY="${TOMO_SEED_STRICT_HISTORY:-1}"

# ---------------------------------------------------------------------------
# Deterministic seed tree (self-contained; see scenario 30 for the rationale).
# ---------------------------------------------------------------------------
seed_relpath() { printf 'd%d/s%d/f%d.dat' "$(( $1 / 100 ))" "$(( ($1 / 10) % 10 ))" "$1"; }
seed_size() {
  local i="$1"
  if (( i % 200 == 0 )); then echo $(( 1000000 + (i * 7) % 1000000 ))
  elif (( i % 20 == 0 )); then echo $(( 20000 + (i * 11) % 40000 ))
  else echo $(( 200 + (i * 37) % 3000 )); fi
}
gen_seed_tree() {
  local root="$1" count="$2" i rel dir last_dir=""
  for (( i = 0; i < count; i++ )); do
    rel="$(seed_relpath "$i")"; dir="$root/${rel%/*}"
    [[ "$dir" != "$last_dir" ]] && { mkdir -p "$dir"; last_dir="$dir"; }
    head -c "$(seed_size "$i")" /dev/zero \
      | openssl enc -aes-256-ctr -nosalt -pass "pass:tomoseed-$i" -out "$root/$rel" 2>/dev/null
  done
}

versions_total()    { ( cd "$1" && "$TOMO_BIN" db check --json 2>/dev/null ) | jq -r '.versions_checked // 0'; }
dst_count()         { find "$1" -name .tomo -prune -o -type f -print 2>/dev/null | wc -l | tr -d ' '; }
dst_count_ge()      { (( "$(dst_count "$1")" >= "$2" )); }
serve_child()       { pgrep -P "$1" -x tomo || true; }
staging_clean()     { [[ -z "$(find "$1/.tomo/staging" -type f 2>/dev/null)" ]]; }
# Poll for a clean staging dir: a live session holds transient persist temps at
# any instant (as assert_converged notes for B), so a one-shot check races them.
assert_staging_clean() {
  local dir="$1" deadline=$(( $(now_ms) + 5000 ))
  while ! staging_clean "$dir"; do
    (( $(now_ms) < deadline )) || { ls -l "$dir/.tomo/staging" >&2; fail "staging debris on $dir"; }
    sleep 0.2
  done
}

# settle_versions DIR → the STABILIZED repo-wide version count. Receiver history
# ingest trails root-convergence (and, after a crash, plateaus below N), so poll
# until the count is unchanged across a 3 s gap (bounded 90 s) and echo it. The
# fixed gap is a stability window, not a convergence wait (same idiom as the
# harness's settle_status / assert_quiet_network).
settle_versions() { # DIR
  local dir="$1" deadline=$(( $(now_ms) + 90000 )) prev cur
  prev="$(versions_total "$dir")"
  while :; do
    sleep 3
    cur="$(versions_total "$dir")"
    { [[ "$cur" == "$prev" ]] || (( $(now_ms) >= deadline )); } && { printf '%s\n' "$cur"; return 0; }
    prev="$cur"
  done
}

# check_completeness LABEL DIR COUNT — the H1 exactly-one-per-side ideal. EITHER
# deviation is a finding: count > N = duplicate crash-retry versions; count < N =
# post-crash history gap. Both are loud WARNINGS by default (suite stays green),
# promoted to hard failures under TOMO_SEED_STRICT_HISTORY=1.
check_completeness() { # LABEL DIR COUNT
  local label="$1" count="$3" msg
  if (( count == SEED_FILES )); then
    log "$label history complete: exactly $SEED_FILES versions"
    return 0
  elif (( count > SEED_FILES )); then
    msg="$label settled at $count/$SEED_FILES versions — $(( count - SEED_FILES )) DUPLICATE versions from the non-idempotent crash-retry (H2)"
  else
    msg="$label settled at $count/$SEED_FILES versions — $(( SEED_FILES - count )) files landed but were NOT versioned after the crash (invariant #4 gap)"
  fi
  if [[ "$STRICT_HISTORY" == "1" ]]; then
    fail "$msg (strict mode)"
  fi
  log "WARNING (FINDING): $msg. Suite stays green; set TOMO_SEED_STRICT_HISTORY=1 to hard-fail. See scenario report."
}

# assert_seed_recovered A B — the shared post-crash postconditions.
assert_seed_recovered() { # A B
  local a="$1" b="$2"
  wait_for "$CONV_TIMEOUT" "converged+settled after recovery" converged_and_settled "$a" "$b"
  # HARD: byte-identical tree, equal index roots, db check green, .tomo isolation.
  assert_converged "$a" "$b"
  assert_staging_clean "$a"
  assert_staging_clean "$b"
  # History integrity: HARD no-duplicates on both sides; completeness per policy.
  local va vb; va="$(settle_versions "$a")"; vb="$(settle_versions "$b")"
  check_completeness "A ($a)" "$a" "$va"
  check_completeness "B ($b)" "$b" "$vb"
}

# Threshold: kill once the receiver has ~30% of the files (transfer clearly in
# flight, with the bulk of it still to come).
KILL_AT=$(( SEED_FILES * 30 / 100 ))
(( KILL_AT < 1 )) && KILL_AT=1

# --- Build the shared template tree ONCE; each part copies it (cheaper than
#     regenerating openssl streams three times; content is what matters). ------
TEMPLATE="$WORK/template"
log "generating deterministic seed template: $SEED_FILES files"
gen_seed_tree "$TEMPLATE" "$SEED_FILES"

# fresh_pair NAME → makes machines NAME_a/NAME_b, copies the template into A,
# inits both. Echoes nothing; sets globals PA / PB to the dirs.
PA=""; PB=""
fresh_pair() { # NAME
  PA="$(make_machine "$1_a")"; PB="$(make_machine "$1_b")"
  cp -r "$TEMPLATE/." "$PA/"
  ( cd "$PA" && "$TOMO_BIN" init >/dev/null 2>&1 ) || fail "init $PA"
  ( cd "$PB" && "$TOMO_BIN" init >/dev/null 2>&1 ) || fail "init $PB"
}

# ===========================================================================
# Part A — kill the RECEIVER (serve child) mid-seed; driver auto-respawns it.
# ===========================================================================
log "part A: kill -9 the RECEIVER at ~${KILL_AT}/${SEED_FILES} files"
fresh_pair a
A="$PA"; B="$PB"
WATCH="$(start_sync "$A" --local-peer "$B")"
wait_for 30 "A connected (part A)" status_connected "$A"
SERVE="$(serve_child "$WATCH")"
[[ -n "$SERVE" ]] || fail "could not find serve child of $WATCH"
wait_for "$CONV_TIMEOUT" "B crosses ~30% ($KILL_AT files)" dst_count_ge "$B" "$KILL_AT"
log "part A: B has $(dst_count "$B") files — killing receiver (serve $SERVE)"
kill -9 "$SERVE"
# The driver must survive the peer death (surface queueing) then auto-respawn.
wait_for 30 "A reports disconnected after receiver kill" \
  bash -c "[[ \"\$( ( cd '$A' && '$TOMO_BIN' status --json 2>/dev/null ) | jq -r '.connected // false')\" == false ]]"
wait_for "$CONV_TIMEOUT" "A auto-respawns receiver + reconnects" status_connected "$A"
assert_seed_recovered "$A" "$B"
log "=== part A passed: receiver kill recovered — converged, db green, no corruption ==="

# ===========================================================================
# Part B — kill the SENDER (driving sync) mid-seed; restart the link.
# ===========================================================================
log "part B: kill -9 the SENDER at ~${KILL_AT}/${SEED_FILES} files"
fresh_pair b
A="$PA"; B="$PB"
WATCH="$(start_sync "$A" --local-peer "$B")"
wait_for 30 "A connected (part B)" status_connected "$A"
SERVE="$(serve_child "$WATCH")"
[[ -n "$SERVE" ]] || fail "could not find serve child of $WATCH"
wait_for "$CONV_TIMEOUT" "B crosses ~30% ($KILL_AT files)" dst_count_ge "$B" "$KILL_AT"
log "part B: B has $(dst_count "$B") files — killing sender (watch $WATCH)"
kill -9 "$WATCH"
# The orphaned serve child loses its stdin and exits on its own; wait for it so
# the restart brings up a single clean served peer (scenario-09 part-A idiom).
wait_for 30 "orphaned serve child exits after sender kill" \
  bash -c "! kill -0 $SERVE 2>/dev/null"
WATCH="$(start_sync "$A" --local-peer "$B")"
wait_for "$CONV_TIMEOUT" "A reconnected after sender restart" status_connected "$A"
wait_for "$CONV_TIMEOUT" "B reconnected after sender restart" status_connected "$B"
assert_seed_recovered "$A" "$B"
log "=== part B passed: sender kill recovered — converged, db green, no corruption ==="

# ===========================================================================
# Part C — repeated receiver kills every ~5 s until the seed converges anyway.
# ===========================================================================
log "part C: repeated receiver kills every ~5s until the seed converges"
fresh_pair c
A="$PA"; B="$PB"
WATCH="$(start_sync "$A" --local-peer "$B")"
wait_for 30 "A connected (part C)" status_connected "$A"

kills=0
deadline=$(( $(now_ms) + CONV_TIMEOUT * 1000 ))
while ! converged_and_settled "$A" "$B"; do
  (( $(now_ms) < deadline )) || fail "part C: seed never converged after $kills repeated kills"
  SERVE="$(serve_child "$WATCH")"
  if [[ -n "$SERVE" ]]; then
    kill -9 "$SERVE"; kills=$((kills+1))
    log "part C: kill #$kills (B had $(dst_count "$B") files)"
    wait_for 60 "A reconnects after kill #$kills" status_connected "$A"
  fi
  # Deliberate kill cadence (test stimulus, not a convergence wait — analogous to
  # scenario 14's fixed storm window): let the resumed seed make ~5 s of forward
  # progress before the next crash. Convergence is polled by the loop condition.
  sleep 5
done
log "part C: converged after $kills repeated receiver kills"
assert_seed_recovered "$A" "$B"
log "=== part C passed: seed survived $kills repeated kills — converged, db green, no corruption ==="

pass
