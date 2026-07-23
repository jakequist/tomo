#!/usr/bin/env bash
# Scenario 30 — Seed correctness + throughput floor (the master net)
# Spec: docs/SEED-PERF.md §2 items H1 (seed-correctness, the master net) and
# H12 (CI perf floor). This is the regression net that every seed-performance
# phase (Phase 1 de-cadencing, Phase 2 receiver batching, Phase 3 bulk mode)
# must keep GREEN. It pins the correctness of a first-ever bulk seed and locks
# in a generous, env-tunable throughput floor so later gains cannot silently
# regress on CI's 2-core runners.
#
# PLAN:
#  1. Generate a deterministic seed tree on A: TOMO_SEED_FILES files (default
#     2000) across nested dirs, mixed sizes (tiny/inline, medium, and a handful
#     above the chunked-transfer threshold), content seeded by file index so
#     the tree — and therefore the timing — is reproducible run to run.
#  2. FIRST-EVER link (ssh where available, local otherwise, via TOMO_LINK_MODE).
#     Time the seed with now_ms from link start to converged+settled (H12).
#  3. FULL postconditions (H1):
#       - every file byte-identical (assert_converged does a full per-file cmp);
#       - exactly ONE history version per file per side — waited-for on a
#         repo-wide total count (drains receiver history lag), asserted == N
#         (catches the crash-retry / duplicate-version failure mode), plus a
#         per-file spot check on a sample of TOMO_SEED_SAMPLE paths both sides;
#       - index roots equal (assert_converged);
#       - zero staging/chunk debris on BOTH sides;
#       - `tomo db check` green on both sides, versions_checked == N both sides.
#  4. H12: assert the measured seed duration <= TOMO_SEED_BOUND_MS.
#
# The seed is link-mode agnostic (no kill/partition here), so it runs under both
# TOMO_LINK_MODE=local (default) and =ssh — a genuine first-ever bootstrap seed.

source "$(dirname "$0")/lib/harness.sh"
scenario_init
require_cli
ensure_jq

# ---------------------------------------------------------------------------
# Tunables. Defaults sized for a 2-core CI runner in the debug profile.
# ---------------------------------------------------------------------------
SEED_FILES="${TOMO_SEED_FILES:-2000}"          # set 20000 for the full manual run
SEED_SAMPLE="${TOMO_SEED_SAMPLE:-12}"          # per-file history spot checks

# H12 throughput floor. Derivation (re-measured after SEED-PERF Phase 1): a
# 2000-file debug local seed converges in ~10.0 s on this dev VM (two runs:
# 10.2 s / 9.8 s) — ~5 ms/file, dominated by the per-file crash-safety fsync
# (invariant #8; this VM's ext4 fsync is ~3.8 ms), NOT pipeline cadence. Phase 1
# de-cadenced the outbound stream (batched frames, bytes-in-flight window,
# priority lane) but on an fsync-bound receiver like this one that holds the rate
# rather than dropping it; the cadence win lands on cadence-bound SSH links (see
# the Phase 1 report / docs/NOTES). So the budget stays 15 ms/file = ~3x the
# measured ~5 ms/file — headroom for a loaded 2-core CI runner and history-ingest
# catch-up — with a 30 s floor so the tiny-count case is never brittle. Phase 2
# (batched fsync barriers) is what will actually lower this rate and let the
# floor ratchet further DOWN. For the 20k manual run this yields 300 s (measured
# ~91-96 s release), comfortably above the baseline.
default_bound=$(( SEED_FILES * 15 ))
(( default_bound < 30000 )) && default_bound=30000
SEED_BOUND_MS="${TOMO_SEED_BOUND_MS:-$default_bound}"

# Generous wall clock for the convergence wait itself — never tighter than the
# H12 bound (otherwise wait_for would fail before the H12 assertion can speak).
CONV_TIMEOUT=$(( SEED_BOUND_MS / 1000 + 120 ))
# History ingest trails root-convergence on the receiver; give it room to drain.
HIST_TIMEOUT=$(( SEED_FILES / 100 + 120 ))

# ---------------------------------------------------------------------------
# Deterministic seed tree.
#
# seed_relpath INDEX  → the repo-relative path for file INDEX (shared by the
# generator and the per-file history sampler so they agree without bookkeeping).
# Files fan out across nested dirs (~100 per leaf, 10 leaves per parent).
# ---------------------------------------------------------------------------
seed_relpath() { printf 'd%d/s%d/f%d.dat' "$(( $1 / 100 ))" "$(( ($1 / 10) % 10 ))" "$1"; }

# seed_size INDEX → deterministic mixed size. Most files are tiny (inline path);
# every 20th is medium; every 200th is > 1 MiB (forces the chunked manifest/pull
# path). This exercises BOTH transfer paths in one seed.
seed_size() {
  local i="$1"
  if (( i % 200 == 0 )); then echo $(( 1000000 + (i * 7) % 1000000 ))
  elif (( i % 20 == 0 )); then echo $(( 20000 + (i * 11) % 40000 ))
  else echo $(( 200 + (i * 37) % 3000 )); fi
}

# gen_seed_tree ROOT COUNT — write COUNT deterministic files under ROOT. Content
# is an AES-CTR keystream keyed by the file index: incompressible (a fair
# throughput floor — no dedup shortcut), unique per file, and reproducible.
gen_seed_tree() {
  local root="$1" count="$2" i rel dir
  local last_dir=""
  for (( i = 0; i < count; i++ )); do
    rel="$(seed_relpath "$i")"
    dir="$root/${rel%/*}"
    [[ "$dir" != "$last_dir" ]] && { mkdir -p "$dir"; last_dir="$dir"; }
    # Feed exactly N zero bytes INTO openssl (AES-CTR is a stream cipher, so the
    # ciphertext length equals the input length). openssl reads to EOF and exits
    # 0 — no SIGPIPE, so this survives the harness's `set -o pipefail`.
    head -c "$(seed_size "$i")" /dev/zero \
      | openssl enc -aes-256-ctr -nosalt -pass "pass:tomoseed-$i" -out "$root/$rel" 2>/dev/null
  done
}

# ---------------------------------------------------------------------------
# Total-version count oracle (repo-wide). `tomo db check --json` reports
# versions_checked over the whole store in a single pass — the cheapest
# repo-wide count. `tomo log --json` (no path) is the CLI-surface cross-check.
# ---------------------------------------------------------------------------
versions_total() { ( cd "$1" && "$TOMO_BIN" db check --json 2>/dev/null ) | jq -r '.versions_checked // 0'; }
log_total()      { ( cd "$1" && "$TOMO_BIN" log --json --limit $(( SEED_FILES * 2 + 100 )) 2>/dev/null ) | jq 'length' 2>/dev/null || echo 0; }

versions_total_ge() { (( "$(versions_total "$1")" >= "$2" )); }

# staging_clean DIR → no leftover staging/chunk temp files (debris).
staging_clean() { [[ -z "$(find "$1/.tomo/staging" -type f 2>/dev/null)" ]]; }
# assert_staging_clean DIR — a LIVE session legitimately holds transient persist
# temps under .tomo/staging at any instant (the index/status persist stages
# through it), so a one-shot check races them (as assert_converged notes for B).
# Poll briefly: a transient temp vanishes in ms; genuine debris persists and fails.
assert_staging_clean() {
  local dir="$1" deadline=$(( $(now_ms) + 5000 ))
  while ! staging_clean "$dir"; do
    (( $(now_ms) < deadline )) || { ls -l "$dir/.tomo/staging" >&2; fail "staging debris on $dir"; }
    sleep 0.2
  done
}

# ---------------------------------------------------------------------------
# Link (mode-aware). Mirrors link_machines' TOMO_LINK_MODE handling but with a
# generous, count-derived connect timeout so the manual 20k run's longer initial
# scan does not trip a fixed 15 s ceiling. Echoes the driving sync pid.
# ---------------------------------------------------------------------------
seed_link() { # A_DIR B_DIR
  local a="$1" b="$2" mode="${TOMO_LINK_MODE:-local}" pid
  ( cd "$a" && "$TOMO_BIN" init >/dev/null 2>&1 ) || fail "tomo init on $a"
  ( cd "$b" && "$TOMO_BIN" init >/dev/null 2>&1 ) || fail "tomo init on $b"
  case "$mode" in
    local) pid="$(start_sync "$a" --local-peer "$b")" ;;
    ssh)   ensure_self_ssh; pid="$(start_sync "$a" "$(whoami)@localhost:$b")" ;;
    *)     fail "unknown TOMO_LINK_MODE: $mode" ;;
  esac
  wait_for "$CONV_TIMEOUT" "A ($a) reports connected" status_connected "$a"
  wait_for "$CONV_TIMEOUT" "B ($b) reports connected" status_connected "$b"
  printf '%s\n' "$pid"
}

# ===========================================================================
A="$(make_machine a)"
B="$(make_machine b)"

log "generating deterministic seed tree: $SEED_FILES files on A"
gen_seed_tree "$A" "$SEED_FILES"
NFILES="$(find "$A" -name .tomo -prune -o -type f -print | wc -l | tr -d ' ')"
[[ "$NFILES" == "$SEED_FILES" ]] || fail "generator produced $NFILES files, expected $SEED_FILES"
log "seed tree ready: $NFILES files, $(du -sh "$A" 2>/dev/null | cut -f1) on disk"

# --- 2. FIRST-EVER link; time the seed to convergence (H12). ---------------
log "first-ever link (mode=${TOMO_LINK_MODE:-local}); timing seed to convergence"
seed_start_ms="$(now_ms)"
WATCH="$(seed_link "$A" "$B")"
wait_for "$CONV_TIMEOUT" "seed converged+settled" converged_and_settled "$A" "$B"
seed_ms=$(( $(now_ms) - seed_start_ms ))
log "seed converged in ${seed_ms} ms (H12 bound ${SEED_BOUND_MS} ms)"

# --- 3. FULL postconditions (H1). ------------------------------------------
# 3a. Byte-identical + index roots equal + staging clean on B + db green both
#     sides + .tomo isolation — the full assert_converged contract. It does a
#     per-file cmp of EVERY file (not a sample), so it covers content in full.
assert_converged "$A" "$B"
log "assert_converged: all $NFILES files byte-identical, roots equal, db green"

# 3b. Exactly ONE history version per file per side. Receiver history ingest
#     trails root-convergence, so first WAIT the repo-wide total up to N (drains
#     the lag), THEN assert it is EXACTLY N — a total above N is the duplicate-
#     version / crash-retry failure mode this net exists to catch.
wait_for "$HIST_TIMEOUT" "A history reaches $NFILES versions" versions_total_ge "$A" "$NFILES"
wait_for "$HIST_TIMEOUT" "B history reaches $NFILES versions" versions_total_ge "$B" "$NFILES"
for side in "$A" "$B"; do
  vt="$(versions_total "$side")"; lt="$(log_total "$side")"
  [[ "$vt" == "$NFILES" ]] || fail "db check on $side: $vt versions, expected exactly $NFILES (duplicate/missing history)"
  [[ "$lt" == "$NFILES" ]] || fail "repo-wide log on $side: $lt versions, expected exactly $NFILES"
done
log "exactly one history version per file: $NFILES on A and B (db check + repo-wide log agree)"

# 3c. Per-file spot check on an evenly-spaced sample: exactly 1 version each side.
step=$(( NFILES / SEED_SAMPLE )); (( step < 1 )) && step=1
for (( i = 0; i < NFILES; i += step )); do
  rel="$(seed_relpath "$i")"
  hist_count_eq "$A" "$rel" 1 || fail "A: $rel has $(hist_count "$A" "$rel") versions, expected exactly 1"
  hist_count_eq "$B" "$rel" 1 || fail "B: $rel has $(hist_count "$B" "$rel") versions, expected exactly 1"
done
log "per-file spot check ($SEED_SAMPLE sampled paths): exactly one version each side"

# 3d. Zero staging/chunk debris on BOTH sides (assert_converged only checks B).
assert_staging_clean "$A"
assert_staging_clean "$B"
log "no staging/chunk debris on either side"

# --- 4. H12 throughput floor. ----------------------------------------------
(( seed_ms <= SEED_BOUND_MS )) \
  || fail "H12: seed took ${seed_ms} ms, exceeds floor ${SEED_BOUND_MS} ms (throughput regression)"
log "H12 ok: ${seed_ms} ms <= ${SEED_BOUND_MS} ms"

pass
