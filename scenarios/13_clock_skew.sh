#!/usr/bin/env bash
# Scenario 13 — Wall-clock skew immunity (M5)
# Spec: docs/TESTING.md. Implement when the milestone lands; until then this
# skips so run-all.sh stays green-with-skips from day zero.
#
# PLAN:
#  1. Run B-side tomo under libfaketime (apt install faketime) offset -3 years
#     (avoids needing to touch the real system clock, works in one VM).
#  2. Full basic-propagation pass (as scenario 01) plus a conflict (as 07).
#  3. Everything converges correctly; history ordering is by vector clock,
#     not wall time. Display timestamps may look weird — decisions may not.

source "$(dirname "$0")/lib/harness.sh"
scenario_init
skip "not yet implemented — lands with M5 (see PLAN above)"
