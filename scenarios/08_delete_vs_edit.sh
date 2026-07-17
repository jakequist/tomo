#!/usr/bin/env bash
# Scenario 08 — Delete-vs-edit conflict (M4)
# Spec: docs/TESTING.md. Implement when the milestone lands; until then this
# skips so run-all.sh stays green-with-skips from day zero.
#
# PLAN:
#  1. Sync a file; partition (as in 07).
#  2. A deletes the file; B edits it.
#  3. Heal → deterministic converged outcome on both sides.
#  4. Regardless of winner, edited content is retrievable from history.
#  5. assert_converged.

source "$(dirname "$0")/lib/harness.sh"
scenario_init
skip "not yet implemented — lands with M4 (see PLAN above)"
