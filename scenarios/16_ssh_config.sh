#!/usr/bin/env bash
# Scenario 16 — ~/.ssh/config semantics (Tier 2, ssh-config-semantics milestone)
# Spec: docs/SPEC.md §2; motivating failure: a Mac whose `~/.ssh/config` rewrites
# an alias to a real HostName, selects a non-default key, relaxes host-key
# checking, and reaches the host through a ProxyJump — all of which `ssh host`
# honours but `tomo` historically ignored.
#
# Exercises the real transport against a temp "remote" over self-SSH to
# localhost, driven entirely by a scenario-written `TOMO_SSH_CONFIG` (so the
# user's real ~/.ssh/config and ~/.ssh/known_hosts are never touched or
# destroyed). Three labeled sub-checks; the scenario passes only if all hold:
#
#   a. Alias → HostName=127.0.0.1 + a custom-named IdentityFile + IdentitiesOnly
#      + StrictHostKeyChecking no + UserKnownHostsFile /dev/null → sync converges
#      with NO known_hosts entry (host key accepted unverified), and the connect
#      log names the resolved endpoint.
#   b. ProxyJump: `Host tomo-viajump` (HostName 127.0.0.1) ProxyJump=tomo-jump
#      (also 127.0.0.1) → the direct-tcpip jump chain is proven end-to-end
#      against real sshd; the connect log shows "via tomo-jump".
#   c. accept-new: an unknown key is recorded into a scenario-local
#      UserKnownHostsFile on the first connect, and a second connect reuses it
#      silently (no re-record, no duplicate line).

source "$(dirname "$0")/lib/harness.sh"
scenario_init
require_cli
ensure_jq
ensure_self_ssh

ME="$(whoami)"

# A custom-named identity at a NON-default path: a copy of the self-SSH key (so
# it is already authorized) placed where only the ssh_config's IdentityFile
# points. Proves the config-declared key path is honoured.
CUSTOM_ID="$WORK/custom_id"
cp "$HOME/.ssh/id_ed25519" "$CUSTOM_ID"
chmod 600 "$CUSTOM_ID"

# Scenario-local known_hosts for the accept-new check (never the real one).
ACCEPT_KH="$WORK/accept_known_hosts"

# Hermetic ssh_config, selected via TOMO_SSH_CONFIG so the transport reads it
# instead of ~/.ssh/config. Every host pins UserKnownHostsFile away from the
# real file.
SSH_CFG="$WORK/ssh_config"
cat > "$SSH_CFG" <<EOF
Host tomo-direct
  HostName 127.0.0.1
  User $ME
  IdentityFile $CUSTOM_ID
  IdentitiesOnly yes
  StrictHostKeyChecking no
  UserKnownHostsFile /dev/null

Host tomo-jump
  HostName 127.0.0.1
  User $ME
  StrictHostKeyChecking no
  UserKnownHostsFile /dev/null

Host tomo-viajump
  HostName 127.0.0.1
  User $ME
  ProxyJump tomo-jump
  StrictHostKeyChecking no
  UserKnownHostsFile /dev/null

Host tomo-acceptnew
  HostName 127.0.0.1
  User $ME
  StrictHostKeyChecking accept-new
  UserKnownHostsFile $ACCEPT_KH
EOF
export TOMO_SSH_CONFIG="$SSH_CFG"

# Guard: the real known_hosts must be byte-identical before and after the run.
REAL_KH="$HOME/.ssh/known_hosts"
REAL_KH_SUM_BEFORE="$( [[ -f "$REAL_KH" ]] && sha256sum "$REAL_KH" | awk '{print $1}' || echo none )"

# sync_and_converge ALIAS LABEL — start `tomo sync ALIAS <B>` from a fresh A,
# wait until both connect, round-trip a file A→B, assert convergence, stop.
# Echoes the driving A machine dir so the caller can read its watch log.
sync_and_converge() {
  local alias="$1" label="$2"
  local a b pid
  a="$(make_machine "a_$label")"
  b="$(make_machine "b_$label")"
  ( cd "$a" && "$TOMO_BIN" init >/dev/null 2>&1 ) || fail "$label: init A"
  ( cd "$b" && "$TOMO_BIN" init >/dev/null 2>&1 ) || fail "$label: init B"
  pid="$(start_sync "$a" "$alias" "$b")"
  wait_for 45 "$label: A reports connected" status_connected "$a"
  wait_for 45 "$label: B reports connected" status_connected "$b"
  printf 'hello-%s\n' "$label" > "$a/file_$label.txt"
  wait_for 30 "$label: file reaches B" \
    assert_file_content "$b/file_$label.txt" "hello-$label"
  wait_for 30 "$label: index roots equal" roots_equal "$a" "$b"
  assert_converged "$a" "$b"
  kill "$pid" 2>/dev/null || true
  wait "$pid" 2>/dev/null || true
  pkill -9 -f "$a" 2>/dev/null || true
}

# ===========================================================================
# (a) Alias rewrite + custom identity + no known_hosts (StrictHostKeyChecking no)
# ===========================================================================
log "CHECK a: alias→HostName, custom identity, StrictHostKeyChecking no, /dev/null"
sync_and_converge tomo-direct direct
ALOG="$WORK/a_direct.watch.log"
grep -q "connecting to tomo-direct (127.0.0.1) over SSH" "$ALOG" \
  || { cat "$ALOG" >&2; fail "a: connect log does not name the resolved endpoint"; }
grep -qi "accepting unverified host key for 127.0.0.1" "$ALOG" \
  || { cat "$ALOG" >&2; fail "a: no note that the host key was accepted unverified"; }
log "  a OK: converged with no known_hosts entry; endpoint logged as 127.0.0.1"

# ===========================================================================
# (b) ProxyJump through localhost — proves the direct-tcpip chain end-to-end
# ===========================================================================
log "CHECK b: ProxyJump tomo-viajump → via tomo-jump → real sshd"
sync_and_converge tomo-viajump viajump
BLOG="$WORK/a_viajump.watch.log"
grep -q "connecting to tomo-viajump (127.0.0.1 via tomo-jump) over SSH" "$BLOG" \
  || { cat "$BLOG" >&2; fail "b: connect log does not show the jump chain"; }
log "  b OK: synced through the direct-tcpip jump chain"

# ===========================================================================
# (c) accept-new records the key once, then reuses it silently
# ===========================================================================
log "CHECK c: accept-new records once, second connect reuses silently"
[[ ! -f "$ACCEPT_KH" ]] || fail "c: scenario-local known_hosts unexpectedly pre-exists"

sync_and_converge tomo-acceptnew acceptnew1
C1LOG="$WORK/a_acceptnew1.watch.log"
[[ -f "$ACCEPT_KH" ]] || fail "c: accept-new did not create the known_hosts file"
KH_LINES="$(grep -c '127.0.0.1' "$ACCEPT_KH" 2>/dev/null || echo 0)"
[[ "$KH_LINES" == "1" ]] \
  || { cat "$ACCEPT_KH" >&2; fail "c: expected exactly one recorded key, got $KH_LINES"; }
grep -q "recorded new host key for 127.0.0.1" "$C1LOG" \
  || { cat "$C1LOG" >&2; fail "c: first connect did not report recording the key"; }

# Second connect against the now-populated known_hosts: silent reuse.
sync_and_converge tomo-acceptnew acceptnew2
C2LOG="$WORK/a_acceptnew2.watch.log"
KH_LINES2="$(grep -c '127.0.0.1' "$ACCEPT_KH" 2>/dev/null || echo 0)"
[[ "$KH_LINES2" == "1" ]] \
  || { cat "$ACCEPT_KH" >&2; fail "c: second connect changed the known_hosts count ($KH_LINES2)"; }
if grep -qi "recorded new host key\|accepting unverified host key" "$C2LOG"; then
  cat "$C2LOG" >&2
  fail "c: second connect was not a silent reuse (re-recorded or re-accepted)"
fi
log "  c OK: recorded once, reused silently (one key, no re-record)"

# The user's real known_hosts must be untouched.
REAL_KH_SUM_AFTER="$( [[ -f "$REAL_KH" ]] && sha256sum "$REAL_KH" | awk '{print $1}' || echo none )"
[[ "$REAL_KH_SUM_BEFORE" == "$REAL_KH_SUM_AFTER" ]] \
  || fail "the real ~/.ssh/known_hosts was modified (before=$REAL_KH_SUM_BEFORE after=$REAL_KH_SUM_AFTER)"

log "all three ssh-config sub-checks held"
pass
