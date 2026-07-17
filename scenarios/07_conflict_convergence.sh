#!/usr/bin/env bash
# Scenario 07 — Concurrent edit conflict (M4)
# Spec: docs/TESTING.md. Implement when the milestone lands; until then this
# skips so run-all.sh stays green-with-skips from day zero.
#
# PLAN:
#  1. init A↔B, sync a file, wait_for convergence.
#  2. Partition: SIGSTOP the transport (or tomo pause once it exists).
#  3. Edit same file differently on A and B.
#  4. Heal partition → wait_for identical content both sides (deterministic
#     winner — run scenario twice to confirm same winner both times).
#  5. Loser version retrievable via tomo conflicts list --json + restore.
#  6. Conflict visible in tomo status --json; assert_converged.

source "$(dirname "$0")/lib/harness.sh"
scenario_init
skip "not yet implemented — lands with M4 (see PLAN above)"
