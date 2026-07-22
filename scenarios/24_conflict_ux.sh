#!/usr/bin/env bash
# Scenario 24 — Conflict UX (Tier 2, UX-V2 §4)
# Spec: docs/TESTING.md row 24; docs/UX-V2.md §4 (conflict UX, TUI-independent).
# Convergence + preserved-loser mechanics are scenario 07's job; THIS scenario
# asserts the command-level conflict UX that lands on top:
#   §4.1 actionable conflict line (live log carries a real id + resolve command);
#   §4.3 `tomo conflicts show <path>` renders the §3b on-disk/in-history framing;
#   §4.2 path-based `tomo conflicts resolve <path>`;
#   §4.4 `--both` writes a `<path>.theirs` sidecar that SYNCS to the peer and the
#        conflict is acknowledged;
#   §4.5 `--interactive` from a non-tty errors cleanly.
#
# The tty interactive loop is covered by unit tests (its pure loop logic with
# injected I/O); here we only assert the non-tty guard, per the plan.
#
# PLAN mirrors 07 for producing a conflict: SIGSTOP the local serve child to
# part the link, write DIFFERENT content to the same path on both sides (B
# first so B's queued inotify is recorded before A's incoming frame applies),
# SIGCONT to heal, then converge.

source "$(dirname "$0")/lib/harness.sh"
scenario_init
require_cli
[[ "${TOMO_LINK_MODE:-local}" == "ssh" ]] && skip "24 partitions the local serve child; ssh link mode not supported"

A="$(make_machine a)"
B="$(make_machine b)"
WLOG="$WORK/$(basename "$A").watch.log"   # A is foreground `tomo sync` (Human reporter)

if [[ -n "${TOMO_SCENARIO_LAG:-}" ]]; then
  netem_delay "$TOMO_SCENARIO_LAG" || skip "netem unavailable for lag variant"
fi

WATCH="$(link_machines "$A" "$B")"

# --- partition control: SIGSTOP/SIGCONT the served peer (child of the watch) ---
SERVE="$(pgrep -P "$WATCH" -x tomo || true)"
[[ -n "$SERVE" ]] || fail "could not find serve child of watch pid $WATCH"
cleanup_serve() { [[ -n "${SERVE:-}" ]] && { kill -CONT "$SERVE" 2>/dev/null; kill -KILL "$SERVE" 2>/dev/null; } || true; }
register_cleanup_fn cleanup_serve
part() { kill -STOP "$SERVE"; }
heal() { kill -CONT "$SERVE"; }

unresolved_a() { ( cd "$A" && "$TOMO_BIN" status --json 2>/dev/null ) | jq -r '.conflicts_unresolved // 0'; }

# make_conflict PATH CONTENT_B CONTENT_A — part, write both sides (B first), heal,
# and converge to the identical deterministic winner on both sides.
make_conflict() {
  local rel="$1" cb="$2" ca="$3"
  part
  printf '%s\n' "$cb" > "$B/$rel"   # B side first (its inotify recorded ahead of A's frame)
  printf '%s\n' "$ca" > "$A/$rel"   # A side second
  heal
  wait_for 30 "reconverged after $rel conflict" converged_and_settled "$A" "$B"
  wait_for 15 "sides agree on winner for $rel" cmp -s "$A/$rel" "$B/$rel"
}

# --- 1. seed two files both sides know, converge + settle ---
echo "base-original" > "$A/base.txt"
echo "keep-original" > "$A/both.txt"
wait_for 10 "base seed A→B" assert_file_content "$B/base.txt" "base-original"
wait_for 10 "both seed A→B" assert_file_content "$B/both.txt" "keep-original"
wait_for 15 "seed converged" converged_and_settled "$A" "$B"
settle_status "$A" "$B"

# =====================================================================
# §4.1 — the live watch log carries an ACTIONABLE conflict line with a real id.
# =====================================================================
make_conflict base.txt "bravo-from-B" "alpha-from-A"

wait_for 15 "A records the base.txt conflict" \
  bash -c "[[ \"\$(unresolved_a)\" -ge 1 ]] || [[ \"\$( cd '$A' && '$TOMO_BIN' conflicts list --json | jq '[.[]|select(.path==\"base.txt\")]|length')\" -ge 1 ]]"

# The recorded conflict id for base.txt (as A's DB sees it).
base_id="$( cd "$A" && "$TOMO_BIN" conflicts list --json | jq -r '[.[]|select(.path=="base.txt")][0].id' )"
[[ "$base_id" =~ ^[0-9]+$ ]] || fail "no numeric conflict id for base.txt (got '$base_id')"

# The foreground session's log must carry the ready-to-paste command WITH that id.
wait_for 15 "A watch log carries the actionable conflict line for base.txt" \
  grep -Eq "conflict base\.txt .*tomo conflicts resolve ${base_id} --take-loser" "$WLOG"
# And it must name whose copy survived (peer/your), not the old bare 'conflict <path>'.
grep -Eq "conflict base\.txt — kept (your|[^ ]+'s) copy · (yours|peer's): tomo conflicts resolve ${base_id} --take-loser" "$WLOG" \
  || { log "watch log tail:"; grep -E "conflict base\.txt" "$WLOG" >&2 || true; fail "actionable conflict line missing its 'kept …' framing"; }
log "§4.1 actionable line present with real id #$base_id"

# =====================================================================
# §4.3 — `tomo conflicts show <path>` renders the §3b on-disk/in-history framing.
# Read-only, works against the live session.
# =====================================================================
show_out="$( cd "$A" && "$TOMO_BIN" conflicts show base.txt )"
grep -q "on disk now —" <<<"$show_out" || { printf '%s\n' "$show_out" >&2; fail "show: missing 'on disk now —' header"; }
grep -q "in history —"  <<<"$show_out" || { printf '%s\n' "$show_out" >&2; fail "show: missing 'in history —' header"; }
# The inline diff frames loser→winner.
grep -q "in history → on disk" <<<"$show_out" || { printf '%s\n' "$show_out" >&2; fail "show: missing loser→winner diff header"; }
# show <id> resolves to the same conflict.
( cd "$A" && "$TOMO_BIN" conflicts show "$base_id" ) | grep -q "conflict #$base_id on base.txt" \
  || fail "show <id> did not render conflict #$base_id"
log "§4.3 show renders the §3b framing for path and id"

# =====================================================================
# §4.2 — path-based resolve targets that path's newest unresolved conflict.
# =====================================================================
u_before="$(unresolved_a)"
[[ "$u_before" -ge 1 ]] || fail "expected ≥1 unresolved on A before path resolve"
( cd "$A" && "$TOMO_BIN" conflicts resolve base.txt --keep-current ) | grep -q "acknowledged conflict #$base_id" \
  || fail "path-based resolve did not acknowledge base.txt's conflict #$base_id"
wait_for 10 "unresolved drops after path resolve" \
  bash -c "[[ \"\$(unresolved_a)\" -lt $u_before ]]"
# A clean error when a path has no unresolved conflict, naming that path.
# (Capture first — the command exits non-zero and would trip `pipefail`.)
clean_err="$( ( cd "$A" && "$TOMO_BIN" conflicts resolve nope.txt --keep-current ) 2>&1 || true )"
grep -q "no unresolved conflict on nope.txt" <<<"$clean_err" \
  || { printf '%s\n' "$clean_err" >&2; fail "resolving a path with no unresolved conflict should error naming the path"; }
log "§4.2 path-based resolve works and errors cleanly on a conflict-free path"

# =====================================================================
# §4.4 — `--both` writes a `<path>.theirs` sidecar that SYNCS to the peer and
# the conflict is acknowledged.
# =====================================================================
make_conflict both.txt "bravo-both-B" "alpha-both-A"
both_id="$( cd "$A" && "$TOMO_BIN" conflicts list --json | jq -r '[.[]|select(.path=="both.txt")][0].id' )"
[[ "$both_id" =~ ^[0-9]+$ ]] || fail "no numeric conflict id for both.txt (got '$both_id')"

# The preserved loser on A is the write that did NOT win — capture it to verify
# the sidecar carries exactly those bytes.
both_winner="$(cat "$A/both.txt")"
loser_id="$( cd "$A" && "$TOMO_BIN" conflicts list --json | jq -r '[.[]|select(.path=="both.txt")][0].loser.id' )"
loser_bytes="$( cd "$A" && "$TOMO_BIN" restore both.txt --version "$loser_id" --stdout )"

u_before="$(unresolved_a)"
( cd "$A" && "$TOMO_BIN" conflicts resolve both.txt --both ) | grep -q "both.txt.theirs" \
  || fail "--both did not report writing the .theirs sidecar"

# Sidecar exists on A, holds the loser bytes, and the winner file is untouched.
[[ -f "$A/both.txt.theirs" ]] || fail "sidecar both.txt.theirs not created on A"
[[ "$(cat "$A/both.txt.theirs")" == "$loser_bytes" ]] || fail "sidecar does not hold the preserved loser bytes"
[[ "$(cat "$A/both.txt")" == "$both_winner" ]] || fail "--both disturbed the winner file"
# The conflict is acknowledged (unresolved count drops).
wait_for 10 "unresolved drops after --both" bash -c "[[ \"\$(unresolved_a)\" -lt $u_before ]]"
# The sidecar SYNCS to the peer like any ordinary file.
wait_for 15 "sidecar syncs A→B" assert_file_content "$B/both.txt.theirs" "$loser_bytes"
log "§4.4 --both wrote a synced .theirs sidecar and acknowledged the conflict"

# =====================================================================
# §4.5 — `--interactive` from a non-tty errors cleanly (no prompt, no hang).
# =====================================================================
if ( cd "$A" && "$TOMO_BIN" conflicts resolve --interactive </dev/null ) >/dev/null 2>"$WORK/inter.err"; then
  fail "--interactive from a non-tty should fail, but it succeeded"
fi
grep -q "needs an interactive terminal on stdin" "$WORK/inter.err" \
  || { cat "$WORK/inter.err" >&2; fail "--interactive non-tty error message is not the tty guard"; }
log "§4.5 --interactive refuses a non-tty stdin cleanly"

# --- final convergence ---
wait_for 15 "converged and settled (final)" converged_and_settled "$A" "$B"
assert_converged "$A" "$B"
pass
