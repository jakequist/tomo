#!/usr/bin/env bash
# Scenario 27 — History control-channel commands (UX-V2 §3 TUI v2; SPEC §13.2)
# Spec: docs/SPEC.md §13.2 (command channel). The additive `history_paths`,
# `history_log`, `version_diff`, `restore`, and `conflict_unresolve` commands
# back the TUI history browser and its real undo. The TUI's logic is covered by
# the reducer/view unit tests; this scenario drives the NEW commands end-to-end
# with `tomo dev ctl` against a REAL running session — the same socket the TUI
# uses — and asserts the underlying store/apply effects.
#
# PLAN:
#  1. link A↔B (local mode). Build ≥2 versions of one path with DIFFERENT
#     origins: A writes it (local on A), then B writes it (remote on A).
#  2. history_paths lists the edited path with a version count ≥ 2.
#  3. history_log shows the timeline newest-first with the right origins
#     (newest = remote, an older one = local).
#  4. version_diff between the two version ids renders a real unified diff.
#  5. restore the OLD version via ctl → the bytes land on A AND sync to B
#     (a restore is an ordinary local edit the session ships).
#  6. conflict flow: create a concurrent conflict (07 partition idiom), resolve
#     it `keep` via ctl, then `conflict_unresolve` it → assert it is listed
#     unresolved again.
#  All assertions are on ctl replies + on-disk/history effects, never on timing.

source "$(dirname "$0")/lib/harness.sh"
scenario_init
require_cli
ensure_jq
[[ "${TOMO_LINK_MODE:-local}" == "ssh" ]] && skip "27 partitions the local serve child; ssh link mode not supported"

A="$(make_machine a)"
B="$(make_machine b)"

if [[ -n "${TOMO_SCENARIO_LAG:-}" ]]; then
  netem_delay "$TOMO_SCENARIO_LAG" || skip "netem unavailable for lag variant"
fi

# Run one control-channel command against a machine's session, printing the reply.
ctl() { # DIR JSON
  ( cd "$1" && "$TOMO_BIN" dev ctl "$2" )
}

# --- 1. link and build a two-origin history for notes.txt --------------------
WATCH="$(link_machines "$A" "$B")"

echo "v1-from-A" > "$A/notes.txt"                 # A authors v1 (local on A)
wait_for 10 "v1 propagates A→B" assert_file_content "$B/notes.txt" "v1-from-A"
wait_for 15 "v1 converged" converged_and_settled "$A" "$B"
settle_status "$A" "$B"

echo "v2-from-B" > "$B/notes.txt"                 # B authors v2 (remote on A)
wait_for 10 "v2 propagates B→A" assert_file_content "$A/notes.txt" "v2-from-B"
wait_for 15 "v2 converged" converged_and_settled "$A" "$B"
settle_status "$A" "$B"

# --- 2. history_paths lists notes.txt with a version count -------------------
paths_reply="$(ctl "$A" '{"type":"history_paths","limit":50}')"
log "history_paths reply: $paths_reply"
jq -e '.ok==true and (.paths | any(.path=="notes.txt" and .versions>=2))' <<<"$paths_reply" >/dev/null \
  || fail "history_paths did not list notes.txt with ≥2 versions: $paths_reply"

# --- 3. history_log shows the timeline newest-first with right origins -------
log_reply="$(ctl "$A" '{"type":"history_log","path":"notes.txt"}')"
log "history_log reply: $log_reply"
jq -e '.ok==true and (.versions|length>=2)' <<<"$log_reply" >/dev/null \
  || fail "history_log did not return ≥2 versions: $log_reply"
# Newest first: the most recent version on A came from B (remote); an older one
# is A's own local authorship.
jq -e '.versions[0].origin=="remote"' <<<"$log_reply" >/dev/null \
  || fail "newest notes.txt version is not remote-origin: $log_reply"
jq -e 'any(.versions[]; .origin=="local")' <<<"$log_reply" >/dev/null \
  || fail "no local-origin version of notes.txt: $log_reply"

new_id="$(jq -r '.versions[0].id' <<<"$log_reply")"
old_id="$(jq -r '.versions | last | .id' <<<"$log_reply")"
[[ -n "$new_id" && -n "$old_id" && "$new_id" != "$old_id" ]] \
  || fail "could not read two distinct version ids ($old_id, $new_id)"
log "notes.txt versions: old=#$old_id new=#$new_id"

# --- 4. version_diff renders a real diff between the two ids ------------------
diff_reply="$(ctl "$A" "{\"type\":\"version_diff\",\"path\":\"notes.txt\",\"from\":$old_id,\"to\":$new_id}")"
log "version_diff reply: $diff_reply"
jq -e '.ok==true and .diffable==true' <<<"$diff_reply" >/dev/null \
  || fail "version_diff was not diffable: $diff_reply"
# from(old, v1) → to(new, v2): v1 removed, v2 added.
jq -e '.diff | any(test("v1-from-A"))' <<<"$diff_reply" >/dev/null \
  || fail "version_diff missing the removed old line: $diff_reply"
jq -e '.diff | any(test("v2-from-B"))' <<<"$diff_reply" >/dev/null \
  || fail "version_diff missing the added new line: $diff_reply"

# --- 5. restore the OLD version → lands on A AND syncs to B -------------------
restore_reply="$(ctl "$A" "{\"type\":\"restore\",\"path\":\"notes.txt\",\"version\":$old_id}")"
log "restore reply: $restore_reply"
jq -e ".ok==true and .version==$old_id and .size>0 and .deleted==false" <<<"$restore_reply" >/dev/null \
  || fail "restore did not report writing the old version: $restore_reply"
# The restored bytes are v1, replacing the on-disk v2 — an observable change —
# and the live session ships them to the peer like any local edit.
wait_for 10 "restored bytes land on A" assert_file_content "$A/notes.txt" "v1-from-A"
wait_for 15 "restored bytes sync A→B"  assert_file_content "$B/notes.txt" "v1-from-A"
wait_for 15 "converged after restore"  converged_and_settled "$A" "$B"
settle_status "$A" "$B"
log "restore via ctl landed on A and synced to B"

# --- 6. conflict flow: keep via ctl, then conflict_unresolve -----------------
# Partition the local serve child (idiom from scenario 07) to force a concurrent
# conflict, then exercise the resolve/unresolve round-trip over the ctl socket.
SERVE="$(pgrep -P "$WATCH" -x tomo || true)"
[[ -n "$SERVE" ]] || fail "could not find serve child of watch pid $WATCH"
cleanup_serve() { [[ -n "${SERVE:-}" ]] && { kill -CONT "$SERVE" 2>/dev/null; kill -KILL "$SERVE" 2>/dev/null; } || true; }
register_cleanup_fn cleanup_serve

echo "clash-base" > "$A/clash.txt"
wait_for 10 "clash seed A→B" assert_file_content "$B/clash.txt" "clash-base"
wait_for 15 "clash seed converged" converged_and_settled "$A" "$B"
settle_status "$A" "$B"

kill -STOP "$SERVE"                          # partition
echo "clash-from-B" > "$B/clash.txt"         # B first (its inotify drains ahead)
echo "clash-from-A" > "$A/clash.txt"         # then A
kill -CONT "$SERVE"                          # heal

wait_for 30 "A records the clash conflict" bash -c \
  "[[ \"\$( cd '$A' && '$TOMO_BIN' status --json | jq -r .conflicts_unresolved )\" -ge 1 ]]"

# Pull the clash.txt conflict id over the COMMAND channel (conflicts_list).
list_reply="$(ctl "$A" '{"type":"conflicts_list"}')"
cid="$(jq -r '[.conflicts[] | select(.path=="clash.txt")][0].id' <<<"$list_reply")"
[[ -n "$cid" && "$cid" != "null" ]] || fail "no clash.txt conflict id in ctl conflicts_list: $list_reply"
log "clash.txt conflict id = $cid"

# Resolve keep via ctl → it leaves the unresolved set.
keep_reply="$(ctl "$A" "{\"type\":\"conflicts_resolve\",\"id\":$cid,\"action\":\"keep\"}")"
log "ctl keep reply: $keep_reply"
jq -e '.ok==true' <<<"$keep_reply" >/dev/null || fail "ctl keep did not report ok: $keep_reply"
wait_for 10 "conflict $cid leaves the unresolved set" bash -c \
  "! ( cd '$A' && '$TOMO_BIN' dev ctl '{\"type\":\"conflicts_list\"}' | jq -e --argjson id $cid 'any(.conflicts[]; .id==\$id)' >/dev/null )"

# Now UNRESOLVE it via ctl → it reappears in the unresolved list.
unres_reply="$(ctl "$A" "{\"type\":\"conflict_unresolve\",\"id\":$cid}")"
log "ctl conflict_unresolve reply: $unres_reply"
jq -e ".ok==true and .unresolved==$cid and .newly_unresolved==true" <<<"$unres_reply" >/dev/null \
  || fail "conflict_unresolve did not flip the conflict back: $unres_reply"
wait_for 10 "conflict $cid is listed unresolved again" bash -c \
  "( cd '$A' && '$TOMO_BIN' dev ctl '{\"type\":\"conflicts_list\"}' | jq -e --argjson id $cid 'any(.conflicts[]; .id==\$id)' >/dev/null )"
# And the human status badge count reflects it.
wait_for 10 "unresolved count reflects the reopened conflict" bash -c \
  "[[ \"\$( cd '$A' && '$TOMO_BIN' status --json | jq -r .conflicts_unresolved )\" -ge 1 ]]"
log "conflict $cid resolved (keep) then unresolved via ctl — reappears unresolved"

# --- final convergence -------------------------------------------------------
kill -CONT "$SERVE" 2>/dev/null || true
wait_for 15 "converged at end" converged_and_settled "$A" "$B"
pass
