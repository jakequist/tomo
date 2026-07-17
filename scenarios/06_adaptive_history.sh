#!/usr/bin/env bash
# Scenario 06 — Adaptive degradation under storm (M3)
# Spec: docs/TESTING.md. Implement when the milestone lands; until then this
# skips so run-all.sh stays green-with-skips from day zero.
#
# PLAN:
#  1. init A↔B, watch running.
#  2. Storm: rapid writes to many files (thousands of events; generate with a
#     tight loop or small helper binary) while polling that `tomo status`
#     stays responsive (bounded response time).
#  3. After quiescence: history contains coalesced checkpoints (version count
#     << event count) AND the final content of EVERY file is versioned.
#  4. assert_converged.

source "$(dirname "$0")/lib/harness.sh"
scenario_init
skip "not yet implemented — lands with M3 (see PLAN above)"
