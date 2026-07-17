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

- **SIGPIPE panic**: `tomo log | head` panics with "Broken pipe" once the
  reader closes (std println! behavior). Needs a global EPIPE-handling pass
  in the CLI (reset SIGPIPE to default or handle write errors). (M3
  integration agent, 2026-07-17.)
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
  watch design, 2026-07-17.)
- **Zero-byte truncate intermediates get versioned**: `>`-style saves under
  light load can record a truthful-but-noisy 0-byte version between real
  ones. Consider a tiny same-path capture-coalescing window (history only,
  never sync) or skipping empty captures that are immediately superseded.
  (Dogfood, M3.)
- **`tomo connect` idempotence**: re-running connect with the IDENTICAL
  target should revalidate (useful health check) instead of erroring;
  a different target should require `--force`. (Dogfood, M2.)
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
