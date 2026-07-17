#!/usr/bin/env bash
# Scenario 10 — Offline changes on both sides (M5)
# Spec: docs/TESTING.md. Implement when the milestone lands; until then this
# skips so run-all.sh stays green-with-skips from day zero.
#
# PLAN:
#  1. init A↔B, converge, then stop the link entirely.
#  2. Make disjoint changes on both sides (creates, edits, deletes).
#  3. Reconnect → wait_for full convergence including both sides changes.
#  4. History contains offline-made versions; assert_converged.

source "$(dirname "$0")/lib/harness.sh"
scenario_init
skip "not yet implemented — lands with M5 (see PLAN above)"
