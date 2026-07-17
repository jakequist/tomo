#!/usr/bin/env bash
# Scenario 03 — Atomic-save editor patterns (M1)
# Spec: docs/TESTING.md. Implement when the milestone lands; until then this
# skips so run-all.sh stays green-with-skips from day zero.
#
# PLAN:
#  1. init A↔B with watch running.
#  2. save_like_vim (temp+rename) and save_like_truncate on the same path,
#     multiple rapid rounds.
#  3. wait_for final content on B; assert peer NEVER observed a zero-byte or
#     temp-named file (poll B during the storm recording any bad observation).
#  4. tomo log shows coherent versions only; assert_converged.

source "$(dirname "$0")/lib/harness.sh"
scenario_init
skip "not yet implemented — lands with M1 (see PLAN above)"
