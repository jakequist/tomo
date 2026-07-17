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

scenario_teardown() {
  local pid
  for pid in "${CLEANUP_PIDS[@]:-}"; do
    [[ -n "$pid" ]] && kill "$pid" 2>/dev/null || true
  done
  local fn
  for fn in "${CLEANUP_FNS[@]:-}"; do
    [[ -n "$fn" ]] && "$fn" || true
  done
  [[ -n "$WORK" && -d "$WORK" ]] && rm -rf "$WORK"
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

# ---------------------------------------------------------------------------
# Self-SSH: ensure we can `ssh localhost` non-interactively.
# Sandboxed VM: it is fine (and expected) to install/configure sshd and keys.
# ---------------------------------------------------------------------------
ensure_self_ssh() {
  if ssh -o BatchMode=yes -o ConnectTimeout=3 localhost true 2>/dev/null; then
    return 0
  fi
  log "configuring self-SSH (sandbox VM; safe to modify)"
  command -v sshd >/dev/null || {
    sudo apt-get update -qq && sudo apt-get install -y -qq openssh-server
  }
  sudo service ssh start 2>/dev/null || sudo /usr/sbin/sshd || true
  if [[ ! -f "$HOME/.ssh/id_ed25519" ]]; then
    mkdir -p "$HOME/.ssh" && chmod 700 "$HOME/.ssh"
    ssh-keygen -q -t ed25519 -N '' -f "$HOME/.ssh/id_ed25519"
  fi
  cat "$HOME/.ssh/id_ed25519.pub" >> "$HOME/.ssh/authorized_keys"
  sort -u -o "$HOME/.ssh/authorized_keys" "$HOME/.ssh/authorized_keys"
  chmod 600 "$HOME/.ssh/authorized_keys"
  ssh-keyscan -H localhost >> "$HOME/.ssh/known_hosts" 2>/dev/null || true
  ssh -o BatchMode=yes -o ConnectTimeout=3 localhost true \
    || skip "could not establish self-SSH"
}

# ---------------------------------------------------------------------------
# Machines. make_machine NAME → dir with a fresh "project root".
# start_watch MACHINE_DIR [extra args] → runs `tomo watch` in background,
# logging to $WORK/<name>.watch.log, and registers the pid for cleanup.
# ---------------------------------------------------------------------------
make_machine() {
  local name="$1" dir="$WORK/$1"
  mkdir -p "$dir"
  printf '%s\n' "$dir"
}

start_watch() {
  local dir="$1"; shift || true
  ( cd "$dir" && exec "$TOMO_BIN" watch "$@" ) \
    >"$WORK/$(basename "$dir").watch.log" 2>&1 &
  register_pid "$!"
  printf '%s\n' "$!"
}

# ---------------------------------------------------------------------------
# Polling assertions — the ONLY sanctioned way to wait for convergence.
# wait_for TIMEOUT_SECS DESCRIPTION CMD... : polls every 100ms.
# ---------------------------------------------------------------------------
wait_for() {
  local timeout="$1" desc="$2"; shift 2
  local deadline=$(( $(date +%s%N)/1000000 + timeout*1000 ))
  while (( $(date +%s%N)/1000000 < deadline )); do
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

# Convergence invariants (see docs/TESTING.md). These call the real CLI's
# --json output; they are the canonical post-condition of every scenario.
assert_converged() { # DIR_A DIR_B
  local a="$1" b="$2"
  diff -r --exclude='.tomo' "$a" "$b" >/dev/null \
    || fail "trees differ after convergence"
  [[ -e "$b/.tomo/staging" ]] && [[ -n "$(ls -A "$b/.tomo/staging" 2>/dev/null)" ]] \
    && fail "staging not empty on B" || true
  # TODO(M1): assert equal index roots via `tomo status --json | jq .root`
  # TODO(M1): quiet-network — capture counters before/after an observation
  #           window via `tomo status --json | jq .net` and assert no delta.
  # TODO(M3): `tomo db check` integrity passes on both sides.
  ! find "$b" -path "$b/.tomo" -prune -o -name '.tomo' -print | grep -q . \
    || fail ".tomo leaked into peer tree"
}

# ---------------------------------------------------------------------------
# Network lag injection. Applies netem delay on loopback. Requires root (fine
# in the sandbox). Pair every add with remove via register_cleanup_fn.
# Usage:   netem_delay 50ms   ...   netem_clear
# If tc/netem is unavailable, callers should `skip`, or run the no-lag variant.
# ---------------------------------------------------------------------------
netem_delay() {
  local delay="$1"
  command -v tc >/dev/null || { sudo apt-get install -y -qq iproute2 || true; }
  command -v tc >/dev/null || return 1
  sudo tc qdisc add dev lo root netem delay "$delay" 2>/dev/null \
    || sudo tc qdisc change dev lo root netem delay "$delay" \
    || return 1
  register_cleanup_fn netem_clear
  log "netem: ${delay} delay on loopback"
}

netem_clear() { sudo tc qdisc del dev lo root 2>/dev/null || true; }

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
