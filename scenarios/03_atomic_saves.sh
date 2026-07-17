#!/usr/bin/env bash
# Scenario 03 — Atomic-save editor patterns (Tier 1, M1)
# Spec: docs/TESTING.md. Editors save via temp+rename (vim/VSCode) or
# truncate+write. The peer must land exactly the final coherent content and
# must NEVER expose a zero-byte / partial file at the TARGET path — staging +
# atomic rename discipline (invariant #8).
#
# PLAN:
#  1. link A↔B.
#  2. save_like_vim (temp+rename) and save_like_truncate on A, many rounds,
#     plus a tight rapid burst of vim-style saves.
#  3. A background sampler watches the TARGET paths on B for the whole storm;
#     any zero-byte-at-target observation = FAIL.
#  4. B ends with exactly the final content of each file; assert_converged.
#
# KNOWN-CHURN: with the current design an editor's transient temp file
# (e.g. `.doc.txt.swp.NNN`) MAY briefly propagate to B as its own path. That is
# logged churn to eliminate later (temp-file sync suppression); this scenario
# asserts only about the TARGET path's coherence, never about temp paths.

source "$(dirname "$0")/lib/harness.sh"
scenario_init
require_cli
[[ "${TOMO_LINK_MODE:-local}" == "ssh" ]] && ensure_self_ssh

A="$(make_machine a)"
B="$(make_machine b)"

if [[ -n "${TOMO_SCENARIO_LAG:-}" ]]; then
  netem_delay "$TOMO_SCENARIO_LAG" || skip "netem unavailable for lag variant"
fi

link_machines "$A" "$B" >/dev/null

# Target paths whose coherence we guard on B.
VIM_T="$B/doc_vim.txt"
TRUNC_T="$B/doc_trunc.txt"
BURST_T="$B/doc_burst.txt"

# Background sampler: flag any moment a TARGET exists but is zero bytes.
STOP="$WORK/sampler.stop"
BAD="$WORK/sampler.bad"
rm -f "$STOP" "$BAD"
(
  while [[ ! -f "$STOP" ]]; do
    for t in "$VIM_T" "$TRUNC_T" "$BURST_T"; do
      if [[ -f "$t" && ! -s "$t" ]]; then
        printf 'zero-byte at %s\n' "$t" >> "$BAD"
      fi
    done
  done
) &
SAMPLER=$!
register_pid "$SAMPLER"

# --- storm: interleaved vim + truncate saves, many rounds ---
ROUNDS=10
for n in $(seq 1 "$ROUNDS"); do
  save_like_vim      "$A/doc_vim.txt"   "vim v$n"
  save_like_truncate "$A/doc_trunc.txt" "trunc v$n"
done

# --- tight rapid burst of vim-style saves (10 in a loop, no pauses) ---
for n in $(seq 1 10); do
  save_like_vim "$A/doc_burst.txt" "burst v$n"
done

# --- B lands exactly the final content of each file ---
wait_for 15 "vim final content on B"   assert_file_content "$VIM_T"   "vim v$ROUNDS"
wait_for 15 "trunc final content on B" assert_file_content "$TRUNC_T" "trunc v$ROUNDS"
wait_for 15 "burst final content on B" assert_file_content "$BURST_T" "burst v10"

# Converge, then stop the sampler and adjudicate the zero-byte observations.
wait_for 10 "index roots converge" roots_equal "$A" "$B"
touch "$STOP"
kill "$SAMPLER" 2>/dev/null || true
wait "$SAMPLER" 2>/dev/null || true

[[ -s "$BAD" ]] && fail "zero-byte target observed on B during storm:
$(cat "$BAD" | sort -u | head)"

assert_converged "$A" "$B"
pass
