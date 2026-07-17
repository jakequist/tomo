# Tomo e2e Scenarios

Executable acceptance tests using the **real CLI**, mirroring
`docs/TESTING.md` 1:1 by number. One VM plays both machines: "A" is a temp
dir driven locally; "B" is a peer temp dir. The link between them is chosen by
`link_machines` (see below) per `TOMO_LINK_MODE`.

## Status

| # | Scenario | State | Notes |
|---|---|---|---|
| 01 | Basic propagation | implemented | create/modify/delete both directions, nested dirs |
| 02 | Echo suppression | implemented | quiet-network + no-resurrection over an observation window |
| 03 | Atomic saves | implemented | vim rename + truncate saves; peer never shows a zero-byte target |
| 07 | Conflict convergence | implemented | partition via SIGSTOP of the serve child; concurrent edits → identical deterministic winner both sides, side-independent hash tiebreak, loser preserved + restorable, badge/resolve flow |
| 08 | Delete-vs-edit | implemented | partition; delete-vs-edit → edit wins both sides (Present beats Tombstone), delete preserved as losing tombstone head |
| 04–06, 09–13 | — | stubs | skip until their milestone (see PLAN comments) |

## Link modes (`TOMO_LINK_MODE`, default `local`)

- `local` — the sanctioned **M1** link: `A` runs `tomo watch --local-peer B`,
  which spawns a served peer rooted at `B` over stdio pipes. No SSH needed.
- `ssh` — the **M2** SSH transport (`tomo connect` + real sshd to localhost).
  Stubbed until the transport crate lands; `link_machines` skips in this mode.

`link_machines A B` inits both roots (idempotent), brings up the link, waits
until both sides report `connected` via `status --json`, and echoes the driving
watch PID. Use `start_watch` directly if a scenario needs to drive the link by
hand.

```
./run-all.sh                # everything (skips report distinctly from fails)
./run-all.sh --quick        # Tier 1 only — run after every major change
./run-all.sh --lag 50ms     # with netem latency on loopback
./run-all.sh 07             # one scenario
```

Rules of the harness (see `lib/harness.sh`):

- **Poll, never sleep-and-hope** — `wait_for` with a timeout is the only way
  to await convergence. A scenario that needs a bare `sleep` to pass has found
  a real bug.
- Exit 0 = pass, 1 = fail (with state dump), 77 = skip (missing prerequisite
  or milestone not yet reached).
- Every scenario ends with `assert_converged` (tree diff + invariants; the
  TODOs inside it graduate to real assertions as `--json` surfaces land).
- `01_basic_propagation.sh` is the exemplar; copy its shape. Scenarios 01–03
  are implemented; stubs 04–13 contain step-by-step PLAN comments — implement
  them at their milestone and delete the `skip` line.
- Convergence is asserted via the real CLI's `status --json`: `roots_equal A B`
  (wait_for-friendly) then the hard `assert_converged` (tree diff + equal index
  roots + staging-empty + `.tomo`-isolation). `assert_quiet_network A SECS`
  guards the quiet-network invariant over a bounded observation window.
- Lag injection: `netem_delay 50ms` (root is fine in the sandbox); cleanup is
  automatic. If netem is unavailable, `skip` the lag variant.
- Clock skew (13) uses `libfaketime` rather than touching the system clock.
