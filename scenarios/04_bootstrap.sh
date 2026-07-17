#!/usr/bin/env bash
# Scenario 04 — Remote bootstrap matrix (M2)
# Spec: docs/TESTING.md. Implement when the milestone lands; until then this
# skips so run-all.sh stays green-with-skips from day zero.
#
# PLAN:
#  1. Fresh remote dir, no binary → connect → assert .tomo/bin/tomo-<ver>-<triple>
#     exists, is executable, sha256 matches local embedded copy, handshake OK.
#  2. Reconnect with matching binary present → assert NO new push (mtime/inode
#     unchanged).
#  3. Plant a binary with patch-version bumped name/handshake → assert re-push.
#  4. Fake unsupported arch (wrap uname via PATH shim in remote command, or a
#     test-only env override) → assert clean, explicit failure message.

source "$(dirname "$0")/lib/harness.sh"
scenario_init
skip "not yet implemented — lands with M2 (see PLAN above)"
