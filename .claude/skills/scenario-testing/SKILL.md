---
name: scenario-testing
description: How to run, write, and debug Tomo's end-to-end scenarios — the self-SSH harness, temp-dir "machines", polling assertions, network lag injection with tc netem, clock skew via libfaketime, and environment setup on the sandboxed VM. Consult this skill before running anything in scenarios/, before implementing a stubbed scenario, when a scenario fails or flakes, when setting up SSH/sshd on this machine, or whenever acceptance testing or e2e regression testing comes up.
---

# Scenario Testing for Tomo

## Mental model

One sandboxed Linux VM plays both machines. Machine A = temp dir + local
`tomo`; Machine B = temp dir reached over **real SSH to localhost**, so
bootstrap/transport/remote-spawn are genuinely exercised. Scenarios map 1:1
(by number) to the acceptance tests in `docs/TESTING.md`. The harness is
`scenarios/lib/harness.sh`; the exemplar is `01_basic_propagation.sh`; stubs
02–13 carry step-by-step PLAN comments — implement at their milestone by
following the plan and deleting the `skip` line.

## Environment setup (do it, don't ask)

This VM is sandboxed — install and configure freely:

- `ensure_self_ssh` in the harness installs/starts sshd, generates an ed25519
  key, and authorizes it. If it skips, run its steps manually and inspect
  (`sudo service ssh status`, `ssh -v localhost true`).
- Useful packages: `openssh-server iproute2 jq faketime` (apt), and build
  essentials for cargo.
- Root via sudo is fine (needed for `tc netem`).

## Iron rules

1. **Poll, never sleep.** `wait_for <timeout> <desc> <cmd...>` is the only
   sanctioned wait. If a scenario passes only with a bare sleep, you found a
   real product bug — fix the product.
2. **Exit codes**: 0 pass / 1 fail / 77 skip. Skips must state the missing
   prerequisite so `run-all.sh` output stays diagnostic.
3. Every scenario ends with `assert_converged A B`. As `--json` surfaces land
   (status/log/conflicts), graduate the TODOs inside `assert_converged` into
   real assertions (index roots, quiet-network counters, db integrity) — this
   strengthens *every* scenario at once.
4. Self-contained: own tmpdirs, cleanup via the harness traps, no shared
   state between scenarios, safe to run in any order.
5. Scenarios use the **real CLI only** — no reaching into internals. If an
   assertion is impossible via the CLI, that's a missing `--json` field;
   add it to the product first.

## Running

```bash
cargo build                        # harness uses target/debug/tomo
./scenarios/run-all.sh --quick     # Tier 1 — after every major change
./scenarios/run-all.sh             # full suite — before milestone completion
./scenarios/run-all.sh 07          # one scenario while iterating
./scenarios/run-all.sh --lag 50ms  # latency variants (Tier 1–2 must pass at 50ms and 200ms)
```

## Network lag

`netem_delay 50ms` applies delay on loopback via `tc qdisc … netem`
(cleanup auto-registered; manual reset: `sudo tc qdisc del dev lo root`).
Note loopback delay applies **both directions** — 50ms setting ≈ 100ms RTT;
keep assertions time-bound-aware. If netem is unavailable, `skip` the lag
variant, or (better, when needed) build the documented fallback: a delaying
TCP proxy the SSH connection routes through.

## Special techniques

- **Clock skew (13)**: run the B side under `faketime -f "-3y"` — never touch
  the real system clock; libfaketime intercepts time syscalls per-process.
- **Partitions (07/08/10)**: SIGSTOP/SIGCONT the transport process, or use a
  `tomo pause` command once it exists; prefer the CLI once available.
- **Crash tests (09)**: `kill -9` only; poll `status --json` to time the kill
  mid-transfer; assert no partial at final paths at ANY sampled instant.
- **Storms (06)**: generate events with a tight shell loop or tiny helper;
  measure responsiveness by timing `tomo status` calls during the storm.

## Debugging a failing scenario

Watch logs land in the scenario workdir (`/tmp/tomo-scenario-*/*.watch.log`);
`fail()` dumps the tree. Re-run the single scenario, keep the workdir by
commenting the teardown trap temporarily, and inspect `.tomo/logs/` on both
machines. Reproduce shrink-style: comment out later steps until the minimal
failing sequence remains, then port that sequence into a Level-1 engine test
so the regression is caught in milliseconds forever after.
