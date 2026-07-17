#!/usr/bin/env bash
# Scenario 01 — Basic propagation (Tier 1, milestone M1/M2)
#
# create/modify/delete a file on A → appears/updates/disappears on B within
# bounds; then the mirror image B→A. Full spec: docs/TESTING.md tier 1.
#
# This scenario is the exemplar for the harness pattern: init, link, actions,
# wait_for-based assertions, converged invariant, pass. The link is brought up
# by link_machines per TOMO_LINK_MODE (default "local", the sanctioned M1 link;
# "ssh" activates at M2).

source "$(dirname "$0")/lib/harness.sh"
scenario_init
require_cli

# SSH is only needed for the ssh link mode; the local M1 link uses stdio pipes.
[[ "${TOMO_LINK_MODE:-local}" == "ssh" ]] && ensure_self_ssh

A="$(make_machine a)"
B="$(make_machine b)"

# Optional lag variant (run-all --lag 50ms)
if [[ -n "${TOMO_SCENARIO_LAG:-}" ]]; then
  netem_delay "$TOMO_SCENARIO_LAG" || skip "netem unavailable for lag variant"
fi

# Init both, bring up the link, wait until both sides are connected.
link_machines "$A" "$B" >/dev/null

# --- create A→B ---
echo "hello from a" > "$A/src.txt"
wait_for 10 "create propagates A→B" assert_file_content "$B/src.txt" "hello from a"

# --- modify A→B ---
echo "edited on a" > "$A/src.txt"
wait_for 10 "modify propagates A→B" assert_file_content "$B/src.txt" "edited on a"

# --- create B→A (the two-way requirement) ---
echo "born on b" > "$B/artifact.bin"
wait_for 10 "create propagates B→A" assert_file_content "$A/artifact.bin" "born on b"

# --- delete A→B ---
rm "$A/src.txt"
wait_for 10 "delete propagates A→B" assert_absent "$B/src.txt"

# --- nested dirs ---
mkdir -p "$A/deep/nested/dir"
echo "deep" > "$A/deep/nested/dir/file.txt"
wait_for 10 "nested create propagates" \
  assert_file_content "$B/deep/nested/dir/file.txt" "deep"

# Converge: equal index roots first (wait_for-friendly), then hard invariants.
wait_for 10 "index roots converge" roots_equal "$A" "$B"
assert_converged "$A" "$B"
pass
