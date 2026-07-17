#!/usr/bin/env bash
# Scenario 05 — History fidelity under light load (M3)
# Spec: docs/TESTING.md. Implement when the milestone lands; until then this
# skips so run-all.sh stays green-with-skips from day zero.
#
# PLAN:
#  1. init A↔B, watch running, history.mode default (adaptive).
#  2. Make N=20 sequential edits with wait_for convergence between each
#     (light load ⇒ purity ⇒ every edit becomes a version).
#  3. tomo log --json shows exactly N versions; tomo restore each version to a
#     scratch path and byte-compare against what was written.
#  4. assert_converged.

source "$(dirname "$0")/lib/harness.sh"
scenario_init
skip "not yet implemented — lands with M3 (see PLAN above)"
