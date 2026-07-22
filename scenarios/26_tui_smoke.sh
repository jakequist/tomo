#!/usr/bin/env bash
# Scenario 26 — TUI smoke (UX-V2 §3; the interactive surface)
# Spec: docs/SPEC.md §13 (control channel) + UX-V2 §3. The TUI (`tomo dev tui`
# while the attach lifecycle is being wired) is a control-channel client: it
# subscribes to the event stream and issues commands over the same socket
# `tomo events` / `tomo dev ctl` use. Its logic is covered by the reducer/view
# unit tests; this scenario only proves it comes up against a REAL running
# session, drives a live terminal, and tears the terminal down cleanly on quit —
# without disturbing the session.
#
# PLAN:
#  1. Link A<->B and let them converge.
#  2. Run `tomo dev tui` under a pty (util-linux `script`), feeding `q` on
#     stdin. Assert it exits 0 (script -e returns the child's code).
#  3. Assert the typescript shows the alternate-screen enter AND leave
#     sequences — i.e. the TUI initialized ratatui/crossterm and restored the
#     terminal on the way out (no leaked raw mode / alt screen).
#  4. Assert the underlying session is untouched: still connected, socket still
#     present, and both sides still converged (quitting the UI is not a stop).
#
# Deterministic: `q` is delivered on stdin (buffered — no timing race), and the
# assertions are on exit code + fixed escape byte sequences, never on timing.

source "$(dirname "$0")/lib/harness.sh"
scenario_init
require_cli

command -v script >/dev/null 2>&1 || skip "util-linux \`script\` (pty) not available"

A="$(make_machine a)"
B="$(make_machine b)"

if [[ -n "${TOMO_SCENARIO_LAG:-}" ]]; then
  netem_delay "$TOMO_SCENARIO_LAG" || skip "netem unavailable for lag variant"
fi

# --- 1. link and converge ----------------------------------------------------
WATCH="$(link_machines "$A" "$B")"
wait_for 15 "seed converged" converged_and_settled "$A" "$B"
settle_status "$A" "$B"

# --- 2. run the TUI under a pty, quit with `q` -------------------------------
OUT="$WORK/tui.typescript"
# `script -q` quiet, `-e` return the child's exit status, `-c CMD FILE` run CMD
# with FILE as the typescript. Feeding `q` on stdin makes the reducer set quit.
printf 'q' | script -qec "cd '$A' && '$TOMO_BIN' dev tui" "$OUT" >/dev/null 2>&1
rc=$?
[[ "$rc" -eq 0 ]] || fail "tomo dev tui did not exit cleanly on q (exit $rc)"
log "TUI started and exited 0 on q"

# --- 3. the terminal was entered and restored --------------------------------
grep -qaF $'\x1b[?1049h' "$OUT" || fail "TUI never entered the alternate screen"
grep -qaF $'\x1b[?1049l' "$OUT" || fail "TUI did not restore the terminal on exit"
log "alternate screen entered and restored (no leaked terminal state)"

# --- 4. the session is undisturbed -------------------------------------------
status_connected "$A" || fail "session dropped after the TUI quit"
[[ -S "$A/.tomo/state/ctl.sock" ]] || fail "control socket vanished after the TUI quit"
wait_for 15 "still converged after TUI quit" converged_and_settled "$A" "$B"

# The TUI is a read-mostly client; a post-quit edit must still propagate,
# proving the session kept working throughout.
echo "after-tui" > "$B/after.txt"
wait_for 10 "edit propagates after TUI session" \
  assert_file_content "$A/after.txt" "after-tui"

kill -TERM "$WATCH" 2>/dev/null || true
assert_converged "$A" "$B"

# --- 5. foreground `tomo sync` on a tty = detached session + attached TUI ----
# (UX-V2 §1/§3 default wiring.) `d` detaches leaving the session running;
# `q` then `y` stops it. Input is buffered on the pty — no timing races.
C="$(make_machine c)"
D="$(make_machine d)"
( cd "$C" && "$TOMO_BIN" init ) >/dev/null 2>&1 || fail "init C"
( cd "$D" && "$TOMO_BIN" init ) >/dev/null 2>&1 || fail "init D"

FOUT="$WORK/fg.typescript"
rc=0
printf 'd' | script -qec "cd '$C' && '$TOMO_BIN' sync --local-peer '$D'" "$FOUT" >/dev/null 2>&1 || rc=$?
[[ "$rc" -eq 0 ]] || fail "foreground tty sync did not exit cleanly on d (exit $rc)"
grep -qaF $'\x1b[?1049h' "$FOUT" || fail "foreground sync never entered the TUI"
grep -qa "detached — session still running" "$FOUT" \
  || fail "foreground d-detach did not print the detach hint"
wait_for 10 "detached session holds the lock" test -S "$C/.tomo/state/ctl.sock"
echo "fg-live" > "$C/fg.txt"
wait_for 15 "detached session still syncs" assert_file_content "$D/fg.txt" "fg-live"
log "foreground sync entered the TUI; d detached; session kept syncing"

( cd "$C" && "$TOMO_BIN" stop ) >/dev/null 2>&1 || fail "tomo stop after detach failed"

# Fresh foreground start, stopped from inside the TUI: q opens the confirm,
# y stops the session (quit-means-stop only for foreground starts).
QOUT="$WORK/fgq.typescript"
rc=0
printf 'qy' | script -qec "cd '$C' && '$TOMO_BIN' sync --local-peer '$D'" "$QOUT" >/dev/null 2>&1 || rc=$?
[[ "$rc" -eq 0 ]] || fail "foreground tty sync did not exit cleanly on q/y (exit $rc)"
grep -qa "stopped session" "$QOUT" || fail "q/y stop did not print 'stopped session'"
wait_for 10 "q/y stop released the session" \
  bash -c "! test -S '$C/.tomo/state/ctl.sock'"
log "foreground q → confirm → stop shut the session down cleanly"

pass
