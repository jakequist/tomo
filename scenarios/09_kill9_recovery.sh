#!/usr/bin/env bash
# Scenario 09 — kill -9 mid-transfer recovery (M5)
# Spec: docs/TESTING.md. Implement when the milestone lands; until then this
# skips so run-all.sh stays green-with-skips from day zero.
#
# PLAN:
#  1. init A↔B; create a large (~500MB from /dev/urandom) file on A.
#  2. Poll until transfer is in-flight (staging non-empty / status --json
#     shows transfer), then kill -9 the A-side (repeat variant: B-side).
#  3. Assert no partial file visible at final path on B at ANY point.
#  4. Restart watch → wait_for full convergence; byte-compare the large file.
#  5. tomo db check passes; assert_converged.

source "$(dirname "$0")/lib/harness.sh"
scenario_init
skip "not yet implemented — lands with M5 (see PLAN above)"
