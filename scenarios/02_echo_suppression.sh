#!/usr/bin/env bash
# Scenario 02 — Echo suppression / quiet network (M1)
# Spec: docs/TESTING.md. Implement when the milestone lands; until then this
# skips so run-all.sh stays green-with-skips from day zero.
#
# PLAN:
#  1. init A↔B, start watch, sync one file, wait_for convergence.
#  2. Snapshot `tomo status --json` net counters + history version counts.
#  3. Observe for a fixed window (poll, not sleep-assert) — counters and
#     history counts must not change. Any delta = echo loop = FAIL.
#  4. assert_converged.

source "$(dirname "$0")/lib/harness.sh"
scenario_init
skip "not yet implemented — lands with M1 (see PLAN above)"
