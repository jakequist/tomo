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

### Tier 4 — Later feature scenarios (14–23)

| # | Scenario | Assertion sketch |
|---|---|---|
| 22 | Adoption divergence | **Phase A (adoption):** build a tree on A, "clone" it to B with *fresh* mtimes (like `git clone`), then edit a subset on B and give A one differing file with a *newer* mtime than B's copy. First-ever link. Assert: identical files → zero conflict notes; every B-edited file → B's bytes win on **both** sides; A's newer-mtime file → A's bytes win on **both** sides; every losing version recoverable via `tomo log`/`tomo restore`; `tomo conflicts --json` lists the adoption conflicts; `assert_converged`. **Phase B (steady-state carve-out):** with the link established, stop it, edit the *same* file on both sides with `touch -d` arranged so the mtime rule and the standard (hash) rule pick *different* winners, restart → assert the **standard hash** winner prevailed on both sides (mtime never leaks past genesis). **Phase C (upgrade safety):** restart the link on the converged pair → quiet (no new conflicts, no reshipping). |
| 23 | Control channel | Link a pair; `tomo events --json` streams A's control socket (SPEC §13). Assert: a file created on B surfaces as a `synced` event with the right path (and converges); a `heartbeat` event carries recent, non-null `last_sync_ms_ago`; a concurrent conflict (partition idiom from 07) emits a `conflict` event whose numeric `id` matches `tomo conflicts list --json`; resolving that id via the **command channel** (`tomo dev ctl '{"type":"conflicts_resolve",…}'`) shows resolved in the CLI while the session stays connected and converged; a second concurrent events subscriber also receives events; the socket file is gone after a clean (SIGTERM) shutdown; and a `kill -9`'d session's stale socket does not break the next session's startup (removed + rebound). |
| 24 | Conflict UX (UX-V2 §4) | Produce a conflict (07's partition technique: part the link, write different bytes to the same path on both sides, heal, converge). Assert the command-level conflict UX layered on top: **§4.1** the live foreground watch log carries the *actionable* line `conflict <path> — kept …'s copy · …: tomo conflicts resolve <id> --take-loser` with the SAME id `tomo conflicts list --json` reports; **§4.3** `tomo conflicts show <path>` (and `<id>`) renders the §3b framing (`on disk now — …` / `in history — …`) plus the inline loser→winner diff, read-only against the live session; **§4.2** `tomo conflicts resolve <path> --keep-current` acknowledges that path's newest unresolved conflict and a conflict-free path errors naming it; **§4.4** `tomo conflicts resolve <path> --both` writes a `<path>.theirs` sidecar holding the preserved loser bytes (winner untouched), acknowledges the conflict, and the sidecar SYNCS to the peer like any file; **§4.5** `--interactive` from a non-tty stdin errors cleanly. The interactive tty loop itself is covered by Level-1 unit tests (pure loop logic with injected I/O). `assert_converged`. |
| 27 | History browser control commands (UX-V2 §3 TUI v2; SPEC §13.2) | Link a pair; drive the additive history ctl commands end-to-end with `tomo dev ctl` against a live session (the same socket the TUI history browser uses; its interaction logic is covered by the reducer/view unit tests). Build ≥2 versions of one path with different origins (A authors it → `local` on A; B authors it → `remote` on A). Assert: `history_paths` lists that path with a version count ≥ 2; `history_log` returns the timeline newest-first with the right origins (newest `remote`, an older one `local`); `version_diff` between the two version ids renders a real unified diff (old line removed, new line added); `restore` of the OLD version lands its bytes on A AND syncs to B (a restore is an ordinary local edit the session ships); and the conflict round-trip — create a concurrent conflict (07 partition idiom), `conflicts_resolve` it `keep` via ctl (it leaves the unresolved set), then `conflict_unresolve` it via ctl (it reappears unresolved, and the status count reflects it). Scenario 26 additionally pty-smokes that `h` opens the history browser and `esc esc q` unwinds it cleanly. `assert_converged`. |
| 28 | Pause / resume (UX-V2 §3; SPEC §13.5) | Link a pair (local mode); converge + settle. `tomo pause` on A → assert `status.json.paused` true on A and a `heartbeat` with `paused:true` on A's event feed, and B surfaces the peer-pause (`status.json.peer_paused` true + a "peer paused" note event). Edit the SAME file on both sides (different bytes) plus a disjoint file each → assert **nothing crosses** while paused (bounded quiet check via `assert_quiet_network` / frame count) yet BOTH sides' histories still capture their own local edits (history version counts climb). `tomo resume` on A → both directions drain and `assert_converged`; the concurrent same-file edit surfaces as an ordinary non-blocking conflict (recorded on both sides, loser preserved). Idempotence: a second `pause` says "already paused", a second `resume` "already syncing"; pausing via `tomo dev ctl '{"type":"pause"}'` matches the CLI. Crash: `kill -9` a paused session, restart → it comes up **unpaused** (`status.json.paused` false) and re-converges. `assert_converged`. |
| 29 | Self-update (`tomo update`; SPEC §9 self-update, §11 `ureq`) | No real network. Build a fake release dir — the freshly built binary copied under this platform's asset name (`tomo-<os>-<arch>`) plus a `sha256sum`-generated `SHA256SUMS` — served by `python3 -m http.server` on localhost; a pristine copy of the binary is the "installed" one, driven with `TOMO_UPDATE_BASE` at the server. Assert the four content-addressed paths: **(A)** `SHA256SUMS` matches the installed binary → `already up to date`, inode unchanged; **(B)** a genuinely different asset (same binary + one trailing NUL, still a runnable ELF) → `--check` reports `update available` without touching the binary, then `update` replaces it (hash now equals the served asset, exec bit set, no staging debris) and a second run is idempotent; **(C)** corrupt `SHA256SUMS` (a hash matching nothing) → hard error, binary untouched, no debris; **(D)** unreachable base → clean error, no partial files beside the binary. |

### Tier 5 — Seed hardening (regression nets for seed-perf work; docs/SEED-PERF.md §2)

These are the correctness/throughput nets that must stay green through the
seed-performance phases (Phase 1 de-cadencing, Phase 2 receiver batching, Phase 3
bulk mode). Seed size is env-tunable (`TOMO_SEED_FILES`, default sized for a
2-core CI runner in the debug profile; set 20000 for the full manual bench shape).

| # | Scenario | Assertion sketch |
|---|---|---|
| 30 | Seed correctness + throughput floor (**H1 + H12**, the master net) | Generate a deterministic seed tree (`TOMO_SEED_FILES`, default 2000 files across nested dirs, mixed sizes — inline, medium, and >1 MiB chunked — content an AES-CTR keystream keyed by index so the tree and its timing are reproducible). First-ever link (mode-agnostic: `TOMO_LINK_MODE=local` default or `ssh`), timing the seed with `now_ms`. FULL postconditions: every file byte-identical (`assert_converged` does a per-file `cmp` of all files); **exactly ONE history version per file per side** — waited-for on the repo-wide total (drains receiver-history lag) then asserted `== N` (catches duplicates), via `tomo db check --json` `versions_checked` AND repo-wide `tomo log --json` length, plus a per-file `tomo log` spot check on a sample; index roots equal; zero staging/chunk debris on **both** sides (polled); `tomo db check` green both sides. **H12:** measured seed duration ≤ `TOMO_SEED_BOUND_MS` (default `max(30000, files×15)` ms — ~3.3× the measured ~4.5 ms/file cadence, generous for CI, ratcheted down as Phase 1/2 land). |
| 31 | kill -9 mid-seed, both sides (**H2**) | Extends scenario 09's kill/restart idioms to the bulk-seed shape. **Part A:** kill -9 the RECEIVER (served peer) once B holds ~30% of the files → the driver auto-respawns it and resumes. **Part B:** kill -9 the SENDER (driving sync) at ~30% → the orphaned serve child EOFs and exits, restart the link. **Part C:** repeated receiver kills every ~5 s until the seed converges anyway. Each part HARD-asserts crash safety (invariant #8): full convergence, every file byte-identical, index roots equal, `tomo db check` green both sides, no staging debris. History integrity at the grain of *exactly one version per file per side* is checked and reported as a loud finding (WARN by default, hard under `TOMO_SEED_STRICT_HISTORY=1`): a settled total **above** N = duplicate crash-retry versions; **below** N = post-crash receiver history gap. *Current-engine findings (see the sp-scenarios report): receiver crash → permanent receiver-side history gap; sender crash+restart → duplicate sender-side versions. Trees still converge and `db check` stays green in both cases.* |
| 32 | Interrupt + live edits + adoption at scale (**H3 + H4 + H11**) | Local link only. **H3 (resume):** SIGSTOP the served peer mid-seed (07/17 partition idiom), hold, CONT → assert the seed RESUMES (receiver file count monotonic across the pause — no restart-from-zero) and COMPLETES within a bound; a pause is not a crash, so history stays complete (exactly N/side). Frame-count ceiling is not asserted (no principled baseline at this layer; the monotonic-count + completion-bound are the resume evidence). **H4 (invariant #3 under bulk):** while a seed streams — (a) edit an already-landed file on the source (HARD: it eventually converges; SOFT finding: it should ship within the normal latency bound while the bulk continues — *current engine queues it behind the whole seed, invariant #3 not upheld during bulk; WARN by default, hard under `TOMO_SEED_STRICT_LIVE=1`*); (b) edit a not-yet-seeded file on the source → its FINAL content lands; (c) concurrent same-path edit on both sides (brief partition to force determinism) → single deterministic winner both sides, a conflict recorded, sync never blocked. **H11 (adoption at scale):** both sides pre-populated with an identical 1k-file tree, then ~100 disjoint divergent files per side plus a 20-file overlap with crossed mtimes; first-ever link → the newer-mtime copy wins on BOTH sides for every overlap file (SPEC §5.3), disjoint edits merge silently (editor wins), losers preserved and retrievable via `tomo log`/`tomo restore`. `assert_converged` throughout. |

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
