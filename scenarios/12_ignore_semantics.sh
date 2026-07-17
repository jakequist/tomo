#!/usr/bin/env bash
# Scenario 12 — Ignore rules are load-bearing (M5)
# Spec: docs/TESTING.md. Implement when the milestone lands; until then this
# skips so run-all.sh stays green-with-skips from day zero.
#
# PLAN:
#  1. Config: target/ ignored. init A↔B, watch running.
#  2. Simulated build on B writes ~2GB across many files in target/.
#  3. Assert zero bytes crossed the wire for target/ (status --json counters)
#     and zero history growth.
#  4. Flip target/ from ignored→synced in config → wait_for pickup on A.
#  5. assert_converged (with the new rule applied).

source "$(dirname "$0")/lib/harness.sh"
scenario_init
skip "not yet implemented — lands with M5 (see PLAN above)"
