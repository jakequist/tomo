#!/usr/bin/env bash
# Tomo e2e scenario harness.
#
# Model: one Linux VM plays both machines. "Machine A" is a temp dir driven by
# the locally built `tomo` binary; "Machine B" is another temp dir reached over
# real SSH to localhost (so the bootstrap, transport, and remote-spawn paths
# are genuinely exercised).
#
# Every scenario sources this file. Scenarios must be:
#   - self-contained (create their own tmpdirs, clean up on exit)
#   - deterministic (poll with timeouts via wait_for; NEVER bare sleeps)
#   - loud on failure (fail() dumps state)
#
# Exit codes: 0 pass, 1 fail, 77 skip (missing prerequisite — runner reports
# skips distinctly; use for e.g. netem unavailable or binary not yet built).

set -euo pipefail

# ---------------------------------------------------------------------------
# Globals
# ---------------------------------------------------------------------------
HARNESS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HARNESS_DIR/../.." && pwd)"
TOMO_BIN="${TOMO_BIN:-$REPO_ROOT/target/debug/tomo}"
SCENARIO_NAME="$(basename "${0:-interactive}" .sh)"
WORK="" # set by scenario_init
declare -a CLEANUP_PIDS=()
declare -a CLEANUP_FNS=()

log()  { printf '[%s] %s\n' "$SCENARIO_NAME" "$*" >&2; }
skip() { log "SKIP: $*"; exit 77; }

# ---------------------------------------------------------------------------
# Portable userland shims (GNU coreutils vs BSD/macOS).
#
# This harness was authored on Linux against GNU coreutils; macOS ships BSD
# `date` (no `%N`) and BSD `stat` (`-f`, not `-c`). Detect the available tools
# ONCE at load time and expose uniform helpers. Scenarios and the harness MUST
# use these instead of raw `date +%s%N` / `stat -c` so a single source stays
# green on both platforms. Precedence: native GNU → coreutils `g*` (Homebrew on
# macOS) → BSD fallback.
# ---------------------------------------------------------------------------

# now_ms: integer milliseconds since the epoch (the polling-loop clock).
# now_ns: integer nanoseconds since the epoch (fine-grained latency math).
if date +%s%N 2>/dev/null | grep -qE '^[0-9]+$'; then
  now_ns() { date +%s%N; }
elif command -v gdate >/dev/null 2>&1; then
  now_ns() { gdate +%s%N; }
else
  now_ns() { perl -MTime::HiRes=time -e 'printf "%.0f\n", time()*1e9'; }
fi
now_ms() { echo $(( $(now_ns) / 1000000 )); }

# stat_size / stat_mtime / stat_mtime_ns / stat_inode: portable file metadata.
# GNU (native or gstat) gives true nanosecond mtime; BSD stat only whole
# seconds, so its _ns synthesizes seconds×1e9 (callers needing genuine
# sub-second precision must run where coreutils is present).
if stat -c%s . >/dev/null 2>&1; then
  stat_size()     { stat -c%s "$1"; }
  stat_mtime()    { stat -c%Y "$1"; }
  stat_mtime_ns() { stat -c'%.9Y' "$1" | tr -d .; }
  stat_inode()    { stat -c%i "$1"; }
elif command -v gstat >/dev/null 2>&1; then
  stat_size()     { gstat -c%s "$1"; }
  stat_mtime()    { gstat -c%Y "$1"; }
  stat_mtime_ns() { gstat -c'%.9Y' "$1" | tr -d .; }
  stat_inode()    { gstat -c%i "$1"; }
else
  stat_size()     { stat -f%z "$1"; }
  stat_mtime()    { stat -f%m "$1"; }
  stat_mtime_ns() { echo $(( $(stat -f%m "$1") * 1000000000 )); }
  stat_inode()    { stat -f%i "$1"; }
fi

fail() {
  log "FAIL: $*"
  log "--- state dump ---"
  [[ -n "$WORK" ]] && find "$WORK" -maxdepth 4 -not -path '*/.tomo/db/*' | head -50 >&2 || true
  exit 1
}

# ---------------------------------------------------------------------------
# Lifecycle
# ---------------------------------------------------------------------------
scenario_init() {
  [[ -x "$TOMO_BIN" ]] || skip "tomo binary not built at $TOMO_BIN (run: cargo build)"
  WORK="$(mktemp -d "/tmp/tomo-scenario-${SCENARIO_NAME}.XXXXXX")"
  trap scenario_teardown EXIT
  log "workdir: $WORK"
}

# Teardown must never change the scenario's outcome: a PASS whose cleanup
# hiccups is still a PASS. Hence every step is failure-tolerant, and we WAIT
# for killed processes to actually exit before rm -rf — a dying process
# writing into .tomo/state mid-removal once turned a green scenario red on CI
# ("Directory not empty").
scenario_teardown() {
  local pid
  for pid in "${CLEANUP_PIDS[@]:-}"; do
    # CONT first: a SIGSTOPped child (partition scenarios) cannot process TERM.
    [[ -n "$pid" ]] && { kill -CONT "$pid" 2>/dev/null; kill "$pid" 2>/dev/null; } || true
  done
  # Wait (bounded) for registered pids to exit; escalate to KILL.
  local deadline=$(( SECONDS + 6 ))
  for pid in "${CLEANUP_PIDS[@]:-}"; do
    [[ -n "$pid" ]] || continue
    while kill -0 "$pid" 2>/dev/null && (( SECONDS < deadline )); do sleep 0.2; done
    kill -9 "$pid" 2>/dev/null || true
  done
  # Safety sweep: `start_watch` is always invoked as WATCH="$(start_watch …)",
  # so its `register_pid "$!"` runs in the command-substitution SUBSHELL and
  # never reaches the parent's CLEANUP_PIDS — watch pids are therefore not in
  # the loop above and would reparent to init on exit, accumulating orphaned
  # watches that skew later timing-sensitive runs. Kill anything still holding
  # THIS scenario's workdir in its argv (unique tmpdir → scenario-isolated).
  # Closing the watch also EOFs its local-peer serve child's stdin, so the
  # serve process exits with it. Runs whether the scenario passed or failed.
  if [[ -n "$WORK" ]]; then
    pkill -9 -f "$WORK" 2>/dev/null || true
  fi
  local fn
  for fn in "${CLEANUP_FNS[@]:-}"; do
    [[ -n "$fn" ]] && "$fn" || true
  done
  if [[ -n "$WORK" && -d "$WORK" && -z "${TOMO_KEEP:-}" ]]; then
    rm -rf "$WORK" 2>/dev/null || { sleep 1; rm -rf "$WORK" 2>/dev/null; } || true
  fi
  return 0
}

register_pid()        { CLEANUP_PIDS+=("$1"); }
register_cleanup_fn() { CLEANUP_FNS+=("$1"); }

pass() { log "PASS"; exit 0; }

# Skip (not fail) while the CLI is still the scaffold stub. Once `tomo
# --version` works (M1 CLI skeleton), scenarios run for real — and failing
# is then the correct TDD signal.
require_cli() {
  "$TOMO_BIN" --version >/dev/null 2>&1 \
    || skip "CLI not implemented yet — this scenario becomes active at its milestone"
}

# jq is required for every --json assertion. Sandbox VM: installing is fine.
# Skip (not fail) if it truly cannot be obtained, so run-all stays diagnostic.
ensure_jq() {
  command -v jq >/dev/null 2>&1 && return 0
  log "installing jq (sandbox VM; safe)"
  sudo apt-get install -y -qq jq >/dev/null 2>&1 || true
  command -v jq >/dev/null 2>&1 || skip "jq required for --json assertions but unavailable"
}

# ---------------------------------------------------------------------------
# Self-SSH: ensure we can `ssh localhost` non-interactively.
# Sandboxed VM: it is fine (and expected) to install/configure sshd and keys.
# ---------------------------------------------------------------------------
# Refresh the localhost host key idempotently. A STALE entry is a normal state
# on a reused machine: an OS/sshd reinstall rotates the host key and the leftover
# known_hosts line makes ssh refuse with "REMOTE HOST IDENTIFICATION HAS CHANGED"
# BEFORE auth is attempted — so clearing then re-scanning is part of the happy
# path, not just error recovery.
refresh_self_known_hosts() {
  mkdir -p "$HOME/.ssh" && chmod 700 "$HOME/.ssh"
  local h
  for h in localhost 127.0.0.1 ::1; do ssh-keygen -R "$h" >/dev/null 2>&1 || true; done
  ssh-keyscan -H localhost >> "$HOME/.ssh/known_hosts" 2>/dev/null || true
}

ensure_self_ssh() {
  ssh -o BatchMode=yes -o ConnectTimeout=3 localhost true 2>/dev/null && return 0
  # A stale host key is the most common reason the preflight fails on a reused
  # box; refresh and retry before doing anything heavier.
  refresh_self_known_hosts
  ssh -o BatchMode=yes -o ConnectTimeout=3 localhost true 2>/dev/null && return 0

  log "configuring self-SSH"
  if [[ ! -f "$HOME/.ssh/id_ed25519" ]]; then
    ssh-keygen -q -t ed25519 -N '' -f "$HOME/.ssh/id_ed25519"
  fi
  cat "$HOME/.ssh/id_ed25519.pub" >> "$HOME/.ssh/authorized_keys"
  sort -u -o "$HOME/.ssh/authorized_keys" "$HOME/.ssh/authorized_keys"
  chmod 600 "$HOME/.ssh/authorized_keys"

  # Ensure an sshd is actually listening. On Linux (the sandbox VM) we may
  # install and start it. On macOS, sshd is "Remote Login", managed by launchd
  # and not enable-able non-interactively without admin rights — if it is off,
  # skip with the exact toggle rather than spawning a rogue /usr/sbin/sshd.
  if [[ "$(uname -s)" == "Darwin" ]]; then
    ssh -o BatchMode=yes -o ConnectTimeout=3 localhost true 2>/dev/null || skip \
      "self-SSH unavailable: enable Remote Login (System Settings → General → Sharing → Remote Login, or: sudo systemsetup -setremotelogin on)"
  else
    command -v sshd >/dev/null || {
      sudo apt-get update -qq && sudo apt-get install -y -qq openssh-server
    }
    sudo service ssh start 2>/dev/null || sudo /usr/sbin/sshd || true
  fi

  refresh_self_known_hosts
  ssh -o BatchMode=yes -o ConnectTimeout=3 localhost true \
    || skip "could not establish self-SSH"
}

# ensure_alt_sshd PORT — start a SECOND sshd listening on PORT (for
# non-standard-port host-key tests). Reuses the system host keys and config, so
# the key served on PORT is the same as on 22 — which is exactly what lets a
# plain (port-less) known_hosts entry authenticate via OpenSSH's fallback.
# Registers a pidfile-based cleanup. Skips (via return 1 → caller decides) if
# sudo or sshd is unavailable. Requires ensure_self_ssh to have run first.
ALT_SSHD_PIDFILE=""
ensure_alt_sshd() { # PORT
  local port="$1" sshd_bin=""
  for c in /usr/sbin/sshd /sbin/sshd "$(command -v sshd 2>/dev/null)"; do
    [[ -n "$c" && -x "$c" ]] && { sshd_bin="$c"; break; }
  done
  [[ -n "$sshd_bin" ]] || return 1
  local pidfile="$WORK/altsshd.pid"
  # sshd re-execs itself, so it insists on an absolute path (which $sshd_bin is).
  sudo "$sshd_bin" -p "$port" -o PidFile="$pidfile" 2>/dev/null || return 1
  ALT_SSHD_PIDFILE="$pidfile"
  register_cleanup_fn _kill_alt_sshd
  # Poll until it accepts a TCP connection (bash /dev/tcp; no external tools).
  local deadline=$(( $(now_ms) + 5000 ))
  while (( $(now_ms) < deadline )); do
    if (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then
      exec 3>&- 3<&-
      return 0
    fi
    sleep 0.1
  done
  return 1
}

_kill_alt_sshd() {
  [[ -n "$ALT_SSHD_PIDFILE" && -f "$ALT_SSHD_PIDFILE" ]] \
    && sudo kill "$(cat "$ALT_SSHD_PIDFILE" 2>/dev/null)" 2>/dev/null || true
}

# ---------------------------------------------------------------------------
# Machines. make_machine NAME → dir with a fresh "project root".
# start_sync MACHINE_DIR [extra args] → runs `tomo sync` in background,
# logging to $WORK/<name>.watch.log, and registers the pid for cleanup.
# `tomo sync` is the primary command (it subsumes the old connect-then-watch);
# extra args are passed straight through (e.g. `--local-peer B`, or an
# `user@host /path` SSH target).
# ---------------------------------------------------------------------------
make_machine() {
  local name="$1" dir="$WORK/$1"
  mkdir -p "$dir"
  printf '%s\n' "$dir"
}

start_sync() {
  local dir="$1"; shift || true
  ( cd "$dir" && exec "$TOMO_BIN" sync "$@" ) \
    >"$WORK/$(basename "$dir").watch.log" 2>&1 &
  register_pid "$!"
  printf '%s\n' "$!"
}

# Back-compat shim: `start_watch` is the old name for `start_sync`. Kept so
# scenarios (and any external harness callers) keep working during the
# `watch` → `sync` transition; new scenarios should call `start_sync`.
start_watch() { start_sync "$@"; }

# link_machines A_DIR B_DIR → inits both (idempotent), brings up the sync link
# per TOMO_LINK_MODE (default "local"), waits until BOTH sides report connected,
# and echoes the driving sync PID (same contract as start_sync). start_sync
# remains available for scenarios that want to drive the link by hand.
#
#   TOMO_LINK_MODE=local  → the sanctioned local link: A `tomo sync --local-peer B`
#                           spawns a served peer rooted at B over stdio pipes.
#   TOMO_LINK_MODE=ssh    → the SSH transport (self-SSH to localhost).
link_machines() {
  local a="$1" b="$2"
  ensure_jq
  ( cd "$a" && "$TOMO_BIN" init >/dev/null 2>&1 ) || fail "tomo init on $a"
  ( cd "$b" && "$TOMO_BIN" init >/dev/null 2>&1 ) || fail "tomo init on $b"

  local mode="${TOMO_LINK_MODE:-local}" pid
  case "$mode" in
    local)
      pid="$(start_sync "$a" --local-peer "$b")"
      ;;
    ssh)
      # `tomo sync user@localhost B` records the peer AND bootstraps B (pushes
      # the remote binary, exchanges Hello) AND starts syncing — one command,
      # which IS the new UX (no separate `connect` step). Self-SSH to localhost
      # is the stand-in for the real Mac↔Linux pair.
      ensure_self_ssh
      pid="$(start_sync "$a" "$(whoami)@localhost" "$b")"
      ;;
    *)
      fail "unknown TOMO_LINK_MODE: $mode (expected 'local' or 'ssh')"
      ;;
  esac

  wait_for 15 "A ($a) reports connected" status_connected "$a"
  wait_for 15 "B ($b) reports connected" status_connected "$b"
  printf '%s\n' "$pid"
}

# ---------------------------------------------------------------------------
# Polling assertions — the ONLY sanctioned way to wait for convergence.
# wait_for TIMEOUT_SECS DESCRIPTION CMD... : polls every 100ms.
# ---------------------------------------------------------------------------
wait_for() {
  local timeout="$1" desc="$2"; shift 2
  local deadline=$(( $(now_ms) + timeout*1000 ))
  while (( $(now_ms) < deadline )); do
    if "$@" >/dev/null 2>&1; then return 0; fi
    sleep 0.1
  done
  fail "timeout (${timeout}s) waiting for: $desc"
}

assert_file_content() { # PATH EXPECTED_CONTENT
  [[ -f "$1" ]] && [[ "$(cat "$1")" == "$2" ]]
}

assert_same_content() { cmp -s "$1" "$2"; }
assert_absent()       { [[ ! -e "$1" ]]; }

# --- status --json readers (real CLI only; the canonical convergence oracle) --
status_root() { # DIR → index root hash (empty if unavailable)
  ( cd "$1" && "$TOMO_BIN" status --json 2>/dev/null ) | jq -r '.root // empty'
}

# DIR → total protocol frames (sent+recv); empty when net is null (unconnected).
net_frames() {
  ( cd "$1" && "$TOMO_BIN" status --json 2>/dev/null ) \
    | jq -r 'if .net == null then empty else (.net.frames_sent + .net.frames_recv) end'
}

# Predicate (wait_for-friendly): DIR currently reports connected.
status_connected() {
  [[ "$( ( cd "$1" && "$TOMO_BIN" status --json 2>/dev/null ) | jq -r '.connected // false')" == "true" ]]
}

# Predicate (wait_for-friendly): A and B report identical, non-empty index roots.
# Scenarios wait_for this BEFORE assert_converged so the hard check never races.
roots_equal() { # DIR_A DIR_B
  local ra rb
  ra="$(status_root "$1")"; rb="$(status_root "$2")"
  [[ -n "$ra" && "$ra" == "$rb" ]]
}

# Predicate: DIR has no deferred reconciling rescan pending. True convergence
# (and any quiet-network observation) requires this on BOTH sides — a pending
# rescan may legally ship late reconciliation traffic.
status_settled() {
  [[ "$( ( cd "$1" && "$TOMO_BIN" status --json 2>/dev/null ) | jq -r '.reconciling // false')" == "false" ]]
}

# Predicate: both sides converged AND settled (roots equal, nothing pending).
converged_and_settled() { # DIR_A DIR_B
  roots_equal "$1" "$2" && status_settled "$1" && status_settled "$2"
}

# settle_status DIR_A DIR_B — block until BOTH sides' full status (minus the
# timestamp) is identical across two reads 2.5s apart. The live status file is
# write-throttled and can lag reality by up to ~2s; any scenario that
# snapshots counters/counts for a quiet-window comparison MUST settle first
# or it will race the file catching up. Fails after 30s of movement.
settle_status() { # DIR_A DIR_B
  local deadline=$(( $(now_ms) + 30000 ))
  local snap
  snap() { # DIR → status json minus volatile timestamp ('' on any failure —
           # a transient unreadable/mid-rename status must not trip set -e)
    ( cd "$1" && "$TOMO_BIN" status --json 2>/dev/null ) \
      | jq -S 'del(.updated_unix_ms)' 2>/dev/null || true
  }
  while :; do
    local a1 b1 a2 b2
    a1="$(snap "$1")"; b1="$(snap "$2")"
    sleep 2.5
    a2="$(snap "$1")"; b2="$(snap "$2")"
    [[ -n "$a1" && "$a1" == "$a2" && -n "$b1" && "$b1" == "$b2" ]] && return 0
    (( $(now_ms) < deadline )) || fail "status never settled on $1/$2"
  done
}

# assert_quiet_network DIR OBSERVATION_SECS — sanctioned bounded observation of
# the quiet-network invariant (docs/TESTING.md): sample the total frame counter,
# hold for the window, fail if it EVER moves. A plain sleep is correct here — we
# are asserting nothing-happens-over-a-window, not waiting-for-something — so
# this is not a `wait_for` case. Call only after convergence.
assert_quiet_network() {
  local dir="$1" secs="$2" before after
  # SETTLE FIRST: the live status file is write-throttled, so its counters can
  # lag reality by up to ~2s. Snapshotting a stale value would let just-
  # finished traffic "appear" during the window as the file catches up (a
  # false echo-loop positive). Require two identical reads 2.5s apart before
  # the observation window begins.
  local settle_deadline=$(( $(now_ms) + 20000 )) s1 s2
  while :; do
    s1="$(net_frames "$dir")"; sleep 2.5; s2="$(net_frames "$dir")"
    [[ -n "$s1" && "$s1" == "$s2" ]] && break
    (( $(now_ms) < settle_deadline )) \
      || fail "net counters never settled on $dir (still moving after 20s)"
  done
  before="$s2"
  [[ -n "$before" ]] || fail "no net counters on $dir (not connected?) — cannot assert quiet network"
  local deadline=$(( $(now_ms) + secs*1000 ))
  while (( $(now_ms) < deadline )); do
    after="$(net_frames "$dir")"
    [[ "$after" == "$before" ]] \
      || fail "network not quiet: frame count moved $before → $after during ${secs}s observation (echo loop?)"
    sleep 0.2
  done
}

# Convergence invariants (see docs/TESTING.md). These call the real CLI's
# --json output; they are the canonical post-condition of every scenario.
assert_converged() { # DIR_A DIR_B
  local a="$1" b="$2"
  # Compare synced FILES (and their contents). Empty-directory existence is
  # deliberately not synchronized in v0 (index tracks files only; dirs
  # materialize on demand and are pruned when sync empties them — SPEC §5.4),
  # so bare `diff -r` would false-positive on empty-dir asymmetry.
  local list_a list_b
  list_a="$(cd "$a" && find . -name .tomo -prune -o -type f -print | sort)"
  list_b="$(cd "$b" && find . -name .tomo -prune -o -type f -print | sort)"
  [[ "$list_a" == "$list_b" ]] \
    || { diff <(printf '%s\n' "$list_a") <(printf '%s\n' "$list_b") | head -20 >&2; \
         fail "file sets differ after convergence"; }
  local f
  while IFS= read -r f; do
    [[ -z "$f" ]] && continue
    cmp -s "$a/$f" "$b/$f" || fail "content differs after convergence: $f"
  done <<< "$list_a"
  # Staging must be empty once quiescent — but a LIVE session legitimately has
  # in-flight temps at any moment (the throttled status/index persists stage
  # through `.tomo/staging/` too, session.rs reset_staging docs), so a one-shot
  # check races them on slow runners. Poll briefly: a transient persist temp
  # vanishes in milliseconds; a genuinely leaked temp persists and still fails.
  local staging_deadline=$(( $(now_ms) + 3000 ))
  while [[ -e "$b/.tomo/staging" ]] && [[ -n "$(ls -A "$b/.tomo/staging" 2>/dev/null)" ]]; do
    if (( $(now_ms) >= staging_deadline )); then
      ls -l "$b/.tomo/staging" >&2
      fail "staging not empty on B"
    fi
    sleep 0.1
  done
  # Equal index roots (M1) — hard final check. Scenarios should wait_for
  # `roots_equal A B` first; this catch never fires on a converged pair.
  local root_a root_b
  root_a="$(status_root "$a")"; root_b="$(status_root "$b")"
  [[ -n "$root_a" && "$root_a" == "$root_b" ]] \
    || fail "index roots differ after convergence (A=$root_a B=$root_b)"
  # History DB integrity passes on both sides (cross-cutting invariant,
  # docs/TESTING.md). Runs only where a store exists; cheap (single query pass).
  db_check_ok "$a" || fail "history db check failed on A ($a)"
  db_check_ok "$b" || fail "history db check failed on B ($b)"
  ! find "$b" -path "$b/.tomo" -prune -o -name '.tomo' -print | grep -q . \
    || fail ".tomo leaked into peer tree"
}

# db_check_ok DIR — `tomo db check` passes (exit 0), or the store does not exist
# yet (a pre-M3 tree carries no history to verify). Kept cheap: a single check
# pass over the store. Used by assert_converged and directly by history scenarios.
db_check_ok() {
  local dir="$1"
  [[ -d "$dir/.tomo/db" ]] || return 0
  ( cd "$dir" && "$TOMO_BIN" db check >/dev/null 2>&1 )
}

# --- history readers (real CLI only; used by scenarios 05/06) ----------------
# hist_json DIR RELPATH → the `tomo log --json` array (empty array on no history).
hist_json() {
  ( cd "$1" && "$TOMO_BIN" log "$2" --json 2>/dev/null ) || printf '[]\n'
}

# hist_count DIR RELPATH → number of recorded versions (0 when none/unreadable).
hist_count() {
  local n
  n="$(hist_json "$1" "$2" | jq 'length' 2>/dev/null)"
  printf '%s\n' "${n:-0}"
}

# Predicate (wait_for-friendly): DIR records exactly N versions of RELPATH.
hist_count_eq() { [[ "$(hist_count "$1" "$2")" == "$3" ]]; }

# Predicate (wait_for-friendly): DIR records at least N versions of RELPATH.
hist_count_ge() { (( "$(hist_count "$1" "$2")" >= "$3" )); }

# ---------------------------------------------------------------------------
# Network lag injection. Applies netem delay on loopback. Requires root (fine
# in the sandbox). Pair every add with remove via register_cleanup_fn.
# Usage:   netem_delay 50ms   ...   netem_clear
# If tc/netem is unavailable, callers should `skip`, or run the no-lag variant.
# ---------------------------------------------------------------------------
# Resolve the `tc` binary. It ships in /sbin or /usr/sbin, which are off a
# normal user's PATH — so a bare `command -v tc` wrongly reports it missing even
# though `sudo tc` works. Check those locations explicitly.
_tc_bin() {
  command -v tc 2>/dev/null && return 0
  local p
  for p in /usr/sbin/tc /sbin/tc; do
    [[ -x "$p" ]] && { printf '%s\n' "$p"; return 0; }
  done
  return 1
}

netem_delay() {
  local delay="$1" tc
  tc="$(_tc_bin)" || { sudo apt-get install -y -qq iproute2 >/dev/null 2>&1 || true; tc="$(_tc_bin)"; }
  [[ -n "$tc" ]] || return 1
  sudo "$tc" qdisc add dev lo root netem delay "$delay" 2>/dev/null \
    || sudo "$tc" qdisc change dev lo root netem delay "$delay" \
    || return 1
  register_cleanup_fn netem_clear
  # Loopback delay applies both directions, so ${delay} setting ≈ 2×${delay} RTT.
  log "netem: ${delay} delay on loopback (≈ 2× that in RTT)"
}

netem_clear() {
  local tc; tc="$(_tc_bin)" || return 0
  sudo "$tc" qdisc del dev lo root 2>/dev/null || true
}

# Optional fallback when netem is unavailable: route the SSH connection through
# a delaying TCP proxy (e.g. `toxiproxy`, or socat + pv rate limiting) and
# point `tomo connect` at the proxy port. Implement in a scenario when needed.

# ---------------------------------------------------------------------------
# Editor-save simulators (for scenario 03)
# ---------------------------------------------------------------------------
save_like_vim() { # FILE CONTENT — write tempfile then rename over target
  local f="$1" c="$2" tmp
  tmp="$(dirname "$f")/.$(basename "$f").swp.$$"
  printf '%s' "$c" > "$tmp"
  mv "$tmp" "$f"
}

save_like_truncate() { # FILE CONTENT — truncate then write (some editors)
  : > "$1"
  printf '%s' "$2" >> "$1"
}
