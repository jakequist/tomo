# Tomo — Build Notes: Bugs & Improvement Backlog

Running log kept by the autonomous build (2026-07-17 overnight run). Two
sections: **Bugs** found while dogfooding the live CLI, and **Improvements**
beyond the spec worth doing once milestones are complete. Each entry gets a
status when addressed.

## Bugs

- **Storm cluster (M5 priority #1)** — found via unthrottled hot-file storm
  (~4.5k rewrites/3s, `printf > hot.txt` tight loop). Fixed during M3:
  (a) `join_reader` deadlocked shutdown when the reader was blocked in a
  blocking `read(stdin)` — fatal errors became zombie processes (gdb-proven);
  now join-only-if-finished. (b) per-event index/status fsync throttled to
  250ms. (c) rescans deferred until 500ms quiescence (a scan during queued
  applies fabricates "local edits" of stale disk). (d) receiver integrity
  mismatch downgraded from fatal to refuse+warn+rescan.
  (e) ROOT-CAUSE FIX: watcher-thread hashing raced the session's own applies,
  fabricating phantom local heads → spurious conflicts (1,440/storm). Sigs
  are now resolved on the session thread at dequeue time (WatchSignal::
  Pending) — storm now converges with 0 conflicts, 0 refused frames.
  REMAINING for M5: (1) apply executor pairs "this frame's bytes" with "the
  batch's Apply target"; under MVR conflicts the Apply target can be a
  different head — source apply bytes BY SIG (frame bytes if matching, else
  disk, else the history CAS — it has the chunks!) instead of refusing.
  Refusal+rescan is the current safe fallback and no longer triggers in
  storms. (2) Add a permanent unthrottled-storm stress scenario (tight-loop
  `>` rewrites, no pacing) asserting bounded convergence + zero conflicts.
  — ADDRESSED (post-M6): `scenarios/14_storm_stress.sh`. A 4 s unthrottled
  `printf 'v%d' > hot.txt` tight loop (~5k writes/storm, no pacing) asserts:
  status < 2 s during the storm, convergence (roots + byte-identical hot.txt)
  within 60 s of storm end, ZERO conflict rows on both sides, hot.txt coalesced
  to < 50 versions, db check green; phase 2 repeats the storm overlapping a
  20 MiB chunked transfer. Measured 3×+ green: ~4.3k–6k writes/storm, max
  status latency 9–17 ms, hot.txt coalesced to 1–4 versions, 0 conflicts.
  (3) **Apply-clobbers-unscanned-local-edit race** (scenario 07 agent):
  an incoming Apply overwrites a concurrent local edit the watcher hasn't
  delivered yet — silent loss, no conflict row. — ADDRESSED (M5 hardening):
  `Engine::is_expected_echo` + an apply guard in session `do_apply` (and
  chunked assembly completion). Before any Apply we snapshot disk and, via
  the pure `applyguard::decide`, compare it to the target, the engine's
  pre-absorb winner (`prior`), and the echo journal: disk==target → skip;
  disk==prior or an echo → apply; otherwise it is an unobserved local edit →
  we do NOT overwrite, and instead feed `Event::Local(disk_state)` so the
  local bytes win locally and ship, with the remote head preserved in history
  (recoverable via `tomo log`). Counterfactual repro: guard off → 4/8 race
  runs silently lost the local edit; guard on → 8/8 lossless. HONEST LIMIT:
  because the local edit is fed *after* the remote absorb it is stamped
  causally-after (LWW, local preference) rather than concurrent, so no
  conflict ROW is written — a true row would need the pre-absorb clock the
  session cannot reconstruct. Acceptable v0 semantics; nothing is lost.

- **SIGPIPE panic**: `tomo log | head` panics with "Broken pipe" once the
  reader closes (std println! behavior). Needs a global EPIPE-handling pass
  in the CLI (reset SIGPIPE to default or handle write errors). (M3
  integration agent, 2026-07-17.) — ADDRESSED (post-M6): `unsafe` is forbidden
  workspace-wide, so we cannot reset SIGPIPE via `libc::signal`. Instead every
  informational command (`log`/`restore --stdout`/`status`/`conflicts`/
  `db check`/`dev`) prints through a new `crate::out` helper (`outln!` +
  `out::bytes`) that catches `ErrorKind::BrokenPipe` on write/flush and exits 0
  quietly; any other write error is swallowed (a print path must never panic).
  The broken-pipe classification is a pure, unit-tested function
  (`out::guarded_write` over a fake failing writer). Verified: `tomo log hot.txt
  | head -1` prints one line, exits 0, empty stderr (was a panic + backtrace).
- **Stale `connected: true` after death** — FIXED at M4: signal-hook
  SIGTERM/SIGINT handler drains history, flushes index/status with
  connected:false, and reaps the serve child (which previously leaked as an
  orphan on every harness teardown). Status-file write throttling also means
  live counters can lag ~2s; scenarios now `settle_status` before any
  quiet-window snapshot. (Dogfood, 2026-07-17.)

## Improvements

- **Editor temp-file churn**: rename-based saves briefly sync the temp file
  (e.g. `.main.rs.swp.123`) to the peer before its deletion propagates.
  Converges, but wasteful and will pollute history at M3. Consider default
  ignore patterns for common editor temps (`*.swp*`, `*~`, `.#*`, `4913`)
  and/or stateful rename pairing in the canonicalizer. (Found reviewing M1
  watch design, 2026-07-17.) — ADDRESSED (post-M6): `tomo-config` now ships
  BUILT-IN default ignore rules (`DEFAULT_IGNORE_PATTERNS`: `**/*.swp`,
  `**/*.swx`, `**/.*.sw?`, `**/*~`, `**/.#*`, `**/#*#`, `**/4913`) applied
  BEFORE user rules (earliest in the last-match-wins list, so any user rule for
  the same pattern overrides them). Toggle with `[sync] default_ignores`
  (default `true`); the `tomo init` template documents it. Editor temps now
  never cross the wire or enter history by default. (Stateful rename pairing is
  a separate, still-open refinement.)
- **Zero-byte truncate intermediates get versioned**: `>`-style saves under
  light load can record a truthful-but-noisy 0-byte version between real
  ones. Consider a tiny same-path capture-coalescing window (history only,
  never sync) or skipping empty captures that are immediately superseded.
  (Dogfood, M3.) — ADDRESSED (post-M6): the adaptive `PressureController`'s
  rung 0 is now floored to `min_capture_window_ms` (default 75 ms, in
  `PressureConfig`) instead of a hard 0 ms — adaptive mode ONLY. A lone save is
  still versioned (it flushes 75 ms later; invariant #4 intact), but a same-path
  truncate+write pair coalesces into the single final state, dropping the 0-byte
  intermediate. The live sync path is untouched (invariant #3); `every-change`
  stays literally 0 ms. Property tests (no-lost-final-write, monotonic, decay)
  still pass; example tests cover the truncate+write coalescing and lone-save
  cases. SPEC §6.2 updated (0→75 ms entry window).
- **`tomo connect` idempotence**: re-running connect with the IDENTICAL
  target should revalidate (useful health check) instead of erroring;
  a different target should require `--force`. (Dogfood, M2.) — ADDRESSED
  (post-M6): `tomo connect` now parses the recorded `[remote]` and (via the
  pure, unit-tested `decide_connect`) revalidates in place when host+path are
  identical, refuses a DIFFERENT target with a message pointing at `--force`,
  and on `--force` strips the old `[remote]` and rewrites it before
  revalidating. New `--force` flag; help text and `connect` docs updated.
- **russh crypto backend**: aws-lc-rs builds static-musl fine on this VM
  (cmake needed), but consider the `ring` backend at M6 for binary size and
  build simplicity; russh 0.54.5 also emits a future-incompat warning —
  check for a russh update then.
- **First-class directory tracking**: v0 syncs files only; empty-dir
  existence can differ between sides (SPEC §5.4). Needed eventually for the
  git ambition (empty dirs, dir renames, permissions). Post-M6.
- **Local control socket**: `tomo status` reads a status file written by the
  watch process (M1 design). A local socket (SPEC future "API protocol")
  would give live queries without file staleness. Post-M6.

## Environment / build-run journal

- 2026-07-17: `rustup target add x86_64-unknown-linux-musl` failed with a
  rustup download-cache error on first attempt; retry needed before M6.
