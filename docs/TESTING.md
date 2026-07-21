# Tomo — Testing Strategy & Acceptance Roadmap

TDD is mandatory at two levels. A feature without tests does not exist.

## Level 1 — Unit & property tests (fast, deterministic, pure)

The sync engine (`tomo-engine`) is a pure state machine, so it is tested
exhaustively without touching a real filesystem, network, or clock:

- **Simulated event streams**: feed sequences of canonical change events and
  assert on emitted actions and index state.
- **Property tests (`proptest`)**: generate random event storms, interleavings,
  and partitions. Core properties:
  - *Convergence*: for any pair of event histories, applying both sides'
    actions leads both indices to identical roots.
  - *Echo idempotence*: applying an action and feeding back its own watcher
    echo produces no new actions.
  - *No lost final write*: for any burst under any pressure schedule, the last
    content of every file is versioned.
  - *Deterministic conflict winner*: both replicas independently pick the same
    winner for any concurrent pair.
  - *Monotonic pressure escalation / decay to purity* for the history
    controller, driven by deterministic simulated time (never `sleep`).
- Vector clock algebra gets its own exhaustive small-case tests plus
  properties (partial order laws, merge commutativity/associativity/idempotence).
- Adapter crates get narrow unit tests (e.g. atomic-save pattern
  canonicalization in `tomo-watch` using synthetic raw-event sequences; frame
  round-tripping in `tomo-proto`).

Rules: no sleeps, no real time, no network, no flakiness tolerated. Real
filesystems and processes belong to Level 2 only.

## Level 2 — Acceptance tests → executable scenarios

Every acceptance test below maps 1:1 to a script in `scenarios/` (same
numbering) using the **real CLI**, SSH to localhost, and temp directories —
two sandboxed "machines" on one VM. See `scenarios/README.md` and the
`scenario-testing` skill for the harness. A milestone is complete only when
its scenarios pass under `./scenarios/run-all.sh`.

### Cross-cutting invariants (asserted at every convergence point, every scenario)

- **Quiet network**: after convergence, zero further traffic and zero new
  history entries (no echo loops).
- **Equal roots**: both sides' index/merkle roots identical.
- **`.tomo/` isolation**: `.tomo/` never appears in the peer's synced tree.
- **History DB integrity check passes.**

### Tier 1 — Core loop (MVP gate)

| # | Scenario | Assertion sketch |
|---|---|---|
| 01 | Basic propagation | create/modify/delete on A → mirrored on B within bound; and B→A. |
| 02 | Echo suppression | full cycle, then quiet-network invariant holds over an observation window. |
| 03 | Atomic-save editors | simulate vim/VSCode save patterns (temp+rename, truncate+write) → exactly one coherent version on peer; never a zero-byte or partial intermediate visible. |
| 04 | Bootstrap | fresh remote → correct-arch binary pushed to `.tomo/bin`, SHA-256 verified, handshake OK. Matching binary → no push. Version off by one patch → re-push. Unsupported arch → clean explicit failure. Agent-context (§4.1): `.tomo/README.md` on both sides carries the version marker (serving side embeds the pushed binary's path); `status.json` peer block records the initiator's hostname + loopback client IP on the serving side and the target host on the initiator side; README never leaks to a synced tree root. |

### Tier 2 — History & conflicts

| # | Scenario | Assertion sketch |
|---|---|---|
| 05 | History fidelity | N sequential edits under light load → N versions retrievable via `tomo log`/`restore`, each byte-identical. |
| 06 | Adaptive degradation | synthetic storm (thousands of events/s) → process responsive, history shows coalesced checkpoints, final content of every file versioned. |
| 07 | Conflict convergence | suspend link, edit same file differently both sides, reconnect → identical winner both sides, loser retrievable, conflict visible in `tomo status --json`. |
| 08 | Delete-vs-edit | A deletes while B edits → deterministic outcome; edited content preserved in history regardless of winner. |

### Tier 3 — Robustness (the rsync-killer tier)

| # | Scenario | Assertion sketch |
|---|---|---|
| 09 | kill -9 recovery | murder either side mid-large-transfer → restart → no corruption, no partials at final paths (staging discipline), sync resumes and converges. |
| 10 | Offline queue | changes on both sides while disconnected → reconnect → full convergence. |
| 11 | Large file + churn | 1 GB file syncing while 10k small files spray → both complete; small-file latency stays bounded (no head-of-line blocking). |
| 12 | Ignore semantics | `target/` ignored → simulated build writes GBs → zero bytes on wire, zero history growth. Flip ignored→synced → picked up. |
| 13 | Clock skew immunity | set one side's wall clock years off (or fake via env/libfaketime) → everything still correct (vector clocks working). |

### Tier 4 — Later feature scenarios (14–22)

| # | Scenario | Assertion sketch |
|---|---|---|
| 22 | Adoption divergence | **Phase A (adoption):** build a tree on A, "clone" it to B with *fresh* mtimes (like `git clone`), then edit a subset on B and give A one differing file with a *newer* mtime than B's copy. First-ever link. Assert: identical files → zero conflict notes; every B-edited file → B's bytes win on **both** sides; A's newer-mtime file → A's bytes win on **both** sides; every losing version recoverable via `tomo log`/`tomo restore`; `tomo conflicts --json` lists the adoption conflicts; `assert_converged`. **Phase B (steady-state carve-out):** with the link established, stop it, edit the *same* file on both sides with `touch -d` arranged so the mtime rule and the standard (hash) rule pick *different* winners, restart → assert the **standard hash** winner prevailed on both sides (mtime never leaks past genesis). **Phase C (upgrade safety):** restart the link on the converged pair → quiet (no new conflicts, no reshipping). |

### Latency/lag variants

The harness supports injecting network latency (`tc netem` on loopback, run
under the harness's netem helpers; fallback: a TCP proxy with delay). Tier 1
and Tier 2 scenarios should each also pass with 50 ms and 200 ms RTT applied.

## Definition of Done, per milestone

1. Unit/property tests written first and passing.
2. `cargo fmt --check`, `clippy -D warnings` clean.
3. The milestone's scenarios implemented and green, including lag variants
   where applicable.
4. `./scenarios/run-all.sh` fully green (no regressions).
5. `docs/SPEC.md` updated if any decision changed; dependency table updated.
