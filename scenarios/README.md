# Tomo e2e Scenarios

Executable acceptance tests using the **real CLI**, mirroring
`docs/TESTING.md` 1:1 by number. One VM plays both machines: "A" is a temp
dir driven locally; "B" is a temp dir reached over **real SSH to localhost**,
so bootstrap, transport, and remote-spawn are genuinely exercised.

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
- `01_basic_propagation.sh` is the exemplar; copy its shape. Stubs 02–13
  contain step-by-step PLAN comments — implement them at their milestone and
  delete the `skip` line.
- Lag injection: `netem_delay 50ms` (root is fine in the sandbox); cleanup is
  automatic. If netem is unavailable, `skip` the lag variant.
- Clock skew (13) uses `libfaketime` rather than touching the system clock.
