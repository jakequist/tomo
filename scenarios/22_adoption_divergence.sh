#!/usr/bin/env bash
# Scenario 22 — Adoption divergence (Tier 4)
# Spec: docs/TESTING.md row 22; docs/SPEC.md §5.3 (genesis adoption tiebreak);
# CLAUDE.md invariant #7 carve-out.
#
# The real user report: Jake had ~/foo on his Mac and a fresh `git clone` of the
# same repo on a VM; the VM's agent edited files. The FIRST-EVER `tomo sync`
# made every file pair concurrent (disjoint genesis clocks {mac:1} vs {vm:1}),
# and the old "higher content hash" tiebreak was a per-file coin flip that let
# the Mac's STALE copies win. The fix: at genesis (disjoint-support heads, where
# vector clocks carry ZERO ordering information) adopt the more recently
# modified copy. The moment the replicas share any history the standard hash
# rule decides again — mtime never leaks past genesis.
#
# PLAN:
#  Phase A (adoption): build a tree on A; "clone" it to B with FRESH mtimes
#    (like git clone); edit a subset on B (newer mtimes); give A one differing
#    file with a mtime NEWER than B's copy. FIRST-EVER link. Assert: identical
#    files → zero conflicts; B-edited files → B's bytes win on BOTH sides;
#    A's newer file → A's bytes win on BOTH sides; every loser recoverable via
#    log/restore; conflicts --json lists the adoptions; assert_converged.
#  Phase B (steady-state carve-out): with the link established, stop it, edit
#    the SAME files on both sides with mtimes arranged so the mtime rule and the
#    hash rule pick DIFFERENT winners, restart → the STANDARD (hash) winner must
#    prevail on both sides (mtime never leaks past genesis).
#  Phase C (upgrade safety): restart the link on the converged pair → quiet
#    (no new conflicts, no reshipping).

source "$(dirname "$0")/lib/harness.sh"
scenario_init
require_cli
ensure_jq
# Phases B/C stop and respawn the local serve child, so — like 07/09/10 — this
# scenario drives the sanctioned LOCAL link only. The genesis semantics under
# test are transport-agnostic (they are pure engine decisions).
[[ "${TOMO_LINK_MODE:-local}" == "ssh" ]] && skip "22 stops/respawns the local serve child; ssh link mode not supported"

A="$(make_machine a)"
B="$(make_machine b)"

if [[ -n "${TOMO_SCENARIO_LAG:-}" ]]; then
  netem_delay "$TOMO_SCENARIO_LAG" || skip "netem unavailable for lag variant"
fi

# Deterministic mtimes (epoch seconds, all safely in the past so the scan's
# recent-write guard is inactive). A_OLD < B_MID < A_NEW, so B's edits beat A's
# baseline and A's one special file beats B's clone of it.
A_OLD=1600000000   # A's pre-existing tree (2020)
B_MID=1700000000   # B's clone + B's edits (2023) — newer than A's baseline
A_NEW=1710000000   # A's one deliberately-newer file (2024) — newer than B_MID
stamp() { touch -d "@$2" "$1"; }  # GNU touch: @N = seconds since epoch

# --- Build A's pre-existing tree (nested dirs), all stamped OLD. --------------
mkdir -p "$A/src" "$A/docs"
echo "shared alpha"        > "$A/common1.txt"
echo "shared bravo"        > "$A/src/common2.txt"
echo "shared charlie"      > "$A/docs/common3.txt"
echo "steady base one"     > "$A/steady1.txt"
echo "steady base two"     > "$A/steady2.txt"
echo "A baseline edit-one" > "$A/editb1.txt"
echo "A baseline edit-two" > "$A/src/editb2.txt"
echo "A ORIGINAL special"  > "$A/special.txt"
while IFS= read -r f; do stamp "$A/$f" "$A_OLD"; done \
  < <(cd "$A" && find . -name .tomo -prune -o -type f -print)

# --- "Clone" A → B WITHOUT preserving mtimes (git-clone style), then diverge. -
# Copy only tracked files (never A's .tomo over B's).
while IFS= read -r f; do
  mkdir -p "$B/$(dirname "$f")"
  cp "$A/$f" "$B/$f"          # no -p: B gets a fresh mtime, like a clone
done < <(cd "$A" && find . -name .tomo -prune -o -type f -print)

# B's agent edits a subset (newer content AND newer mtime than A's baseline).
echo "B EDITED edit-one" > "$B/editb1.txt"
echo "B EDITED edit-two" > "$B/src/editb2.txt"
# Stamp EVERY B file to B_MID: the identical files (mtime irrelevant, no
# conflict) and B's edits (must beat A's OLD baseline). B's clone of special.txt
# stays at B_MID too — older than A's A_NEW below.
while IFS= read -r f; do stamp "$B/$f" "$B_MID"; done \
  < <(cd "$B" && find . -name .tomo -prune -o -type f -print)

# A changes ONE file to different bytes with a mtime NEWER than B's copy — this
# proves the rule follows mtime, not "remote wins".
echo "A NEWER special" > "$A/special.txt"
stamp "$A/special.txt" "$A_NEW"

# --- FIRST-EVER link: each side independently indexes its own tree at genesis
#     clocks ({a:1} vs {b:1}), then reconciliation adopts the newer copy. ------
( cd "$A" && "$TOMO_BIN" init >/dev/null 2>&1 ) || fail "tomo init on A"
( cd "$B" && "$TOMO_BIN" init >/dev/null 2>&1 ) || fail "tomo init on B"
WATCH="$(start_sync "$A" --local-peer "$B")"
wait_for 20 "A reports connected" status_connected "$A"
wait_for 20 "B reports connected" status_connected "$B"

# --- Converge, then assert the adoption outcomes. ----------------------------
wait_for 30 "genesis converged+settled" converged_and_settled "$A" "$B"

# Identical files stay identical on both sides (they never conflicted).
for f in common1.txt src/common2.txt docs/common3.txt steady1.txt steady2.txt; do
  wait_for 15 "identical $f agrees" cmp -s "$A/$f" "$B/$f"
done

# B-edited files: B's bytes win on BOTH sides (newer mtime beat A's baseline).
for pair in "editb1.txt:B EDITED edit-one" "src/editb2.txt:B EDITED edit-two"; do
  f="${pair%%:*}"; want="${pair#*:}"
  wait_for 15 "$f adopts B's bytes on both sides" cmp -s "$A/$f" "$B/$f"
  [[ "$(cat "$A/$f")" == "$want" ]] || fail "$f on A is '$(cat "$A/$f")', expected B's '$want'"
  [[ "$(cat "$B/$f")" == "$want" ]] || fail "$f on B is '$(cat "$B/$f")', expected B's '$want'"
done

# A's newer file: A's bytes win on BOTH sides (mtime followed, not "remote wins").
wait_for 15 "special adopts A's newer bytes on both sides" cmp -s "$A/special.txt" "$B/special.txt"
[[ "$(cat "$A/special.txt")" == "A NEWER special" ]] || fail "special on A is '$(cat "$A/special.txt")'"
[[ "$(cat "$B/special.txt")" == "A NEWER special" ]] || fail "special on B adopted the wrong copy: '$(cat "$B/special.txt")'"
log "adoption: B's edits won on both sides; A's newer 'special' won on both sides"

# --- conflicts --json lists the adoption conflicts; identical files do NOT. ---
settle_status "$A" "$B"
list_a="$( cd "$A" && "$TOMO_BIN" conflicts list --json )"
has_conflict() { jq -e --arg p "$1" 'any(.[]; .path == $p)' <<<"$list_a" >/dev/null; }
for f in editb1.txt src/editb2.txt special.txt; do
  has_conflict "$f" || fail "expected an adoption conflict for $f in conflicts --json"
done
for f in common1.txt steady1.txt steady2.txt src/common2.txt docs/common3.txt; do
  has_conflict "$f" && fail "identical file $f must NOT be a conflict" || true
done
log "conflicts --json lists exactly the 3 divergent files (identical files quiet)"

# --- Every losing version is recoverable via log/restore. --------------------
# editb1's loser is A's baseline; special's loser is B's clone of A's original.
for pair in "editb1.txt:A baseline edit-one" "special.txt:A ORIGINAL special"; do
  f="${pair%%:*}"; loser_want="${pair#*:}"
  row="$( jq -c --arg p "$f" '[.[] | select(.path==$p)][0]' <<<"$list_a" )"
  [[ "$row" != "null" ]] || fail "no conflict row for $f"
  loser_id="$( jq -r '.loser.id' <<<"$row" )"
  got="$( cd "$A" && "$TOMO_BIN" restore "$f" --version "$loser_id" --stdout )"
  [[ "$got" == "$loser_want" ]] || fail "restored loser for $f is '$got', expected '$loser_want'"
  # And the file has ≥2 versions in its log (winner + loser preserved).
  hist_count_ge "$A" "$f" 2 || fail "$f should have ≥2 recorded versions"
done
log "losing versions recoverable byte-exact via restore --stdout"

wait_for 15 "phaseA converged+settled" converged_and_settled "$A" "$B"
assert_converged "$A" "$B"
log "=== Phase A (adoption) passed ==="

# =============================================================================
# Phase B — steady-state carve-out: mtime must NOT leak past genesis.
# =============================================================================
# steady1 and steady2 are identical, converged files (they now share clock
# support on both replicas). Edit them offline so the mtime rule and the hash
# rule DISAGREE, then prove the HASH rule wins.
#
# Both files race the SAME content pair {X, Y}, but with the NEWER mtime placed
# on X for steady1 and on Y for steady2. If mtime leaked, steady1 → X and
# steady2 → Y (they would differ). Under the standard hash rule both pick the
# same content (whichever hashes higher), so their winners MUST agree — a
# side/mtime-independent check that needs no knowledge of the hash order.
PB_OLD=1720000000
PB_NEW=1730000000
X="phase-b-content-XXXX"
Y="phase-b-content-YYYY"

# Stop the link cleanly (kill the watch, wait for the orphaned serve to exit).
SERVE="$(pgrep -P "$WATCH" -x tomo || true)"
kill -9 "$WATCH" 2>/dev/null || true
[[ -n "$SERVE" ]] && wait_for 15 "serve child exits after stopping link" \
  bash -c "! kill -0 $SERVE 2>/dev/null"

# Offline divergent edits with the mtime crossed against the two files.
echo "$X" > "$A/steady1.txt"; stamp "$A/steady1.txt" "$PB_NEW"   # X newer
echo "$Y" > "$B/steady1.txt"; stamp "$B/steady1.txt" "$PB_OLD"   # Y older
echo "$X" > "$A/steady2.txt"; stamp "$A/steady2.txt" "$PB_OLD"   # X older
echo "$Y" > "$B/steady2.txt"; stamp "$B/steady2.txt" "$PB_NEW"   # Y newer

# Restart the link → reconcile the steady-state divergence.
WATCH="$(start_sync "$A" --local-peer "$B")"
wait_for 30 "A reconnected (phase B)" status_connected "$A"
wait_for 30 "B reconnected (phase B)" status_connected "$B"
wait_for 30 "phaseB converged+settled" converged_and_settled "$A" "$B"
for f in steady1.txt steady2.txt; do
  wait_for 15 "sides agree on $f" cmp -s "$A/$f" "$B/$f"
done

s1="$(cat "$A/steady1.txt")"
s2="$(cat "$A/steady2.txt")"
# Each winner must be one of the two concurrent writes (nothing merged/invented).
[[ "$s1" == "$X" || "$s1" == "$Y" ]] || fail "steady1 winner '$s1' is neither write"
[[ "$s2" == "$X" || "$s2" == "$Y" ]] || fail "steady2 winner '$s2' is neither write"
# The decisive check: the STANDARD (hash) rule is mtime-blind, so both files —
# which crossed the mtime the opposite way — must pick the SAME content. Had
# mtime leaked past genesis, steady1 would be X and steady2 would be Y.
[[ "$s1" == "$s2" ]] \
  || fail "mtime LEAKED past genesis: steady1 → '$s1' but reversed-mtime steady2 → '$s2' (standard hash rule must pick the same content for both)"
log "steady state ignores mtime: both files picked '$s1' by the hash rule (mtime crossed, winner unchanged)"

assert_converged "$A" "$B"
log "=== Phase B (steady-state carve-out) passed ==="

# =============================================================================
# Phase C — upgrade/restart safety: a restart on a converged pair is quiet.
# =============================================================================
settle_status "$A" "$B"
conflicts_before="$( cd "$A" && "$TOMO_BIN" status --json | jq -r '.conflicts_unresolved' )"

SERVE="$(pgrep -P "$WATCH" -x tomo || true)"
kill -9 "$WATCH" 2>/dev/null || true
[[ -n "$SERVE" ]] && wait_for 15 "serve child exits before phase C restart" \
  bash -c "! kill -0 $SERVE 2>/dev/null"

WATCH="$(start_sync "$A" --local-peer "$B")"
wait_for 30 "A reconnected (phase C)" status_connected "$A"
wait_for 30 "B reconnected (phase C)" status_connected "$B"
wait_for 30 "phaseC converged+settled" converged_and_settled "$A" "$B"

# No NEW conflicts fabricated by the restart, and the network is quiet (no
# reshipping / echo loop) — the converged pair simply re-handshakes and idles.
conflicts_after="$( cd "$A" && "$TOMO_BIN" status --json | jq -r '.conflicts_unresolved' )"
[[ "$conflicts_after" == "$conflicts_before" ]] \
  || fail "restart fabricated conflicts: $conflicts_before → $conflicts_after"
assert_quiet_network "$A" 3
assert_converged "$A" "$B"
log "=== Phase C (restart safety) passed ==="

pass
