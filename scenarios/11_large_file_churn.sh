#!/usr/bin/env bash
# Scenario 11 — Large file + small-file churn (M5)
# Spec: docs/TESTING.md. Implement when the milestone lands; until then this
# skips so run-all.sh stays green-with-skips from day zero.
#
# PLAN:
#  1. init A↔B, watch running; optionally netem_delay 50ms for realism.
#  2. Start syncing a 1GB file; concurrently spray 10k small files.
#  3. Measure small-file propagation latency DURING the large transfer —
#     must stay under bound (no head-of-line blocking).
#  4. Both complete; byte-compare 1GB file; assert_converged.

source "$(dirname "$0")/lib/harness.sh"
scenario_init
skip "not yet implemented — lands with M5 (see PLAN above)"
