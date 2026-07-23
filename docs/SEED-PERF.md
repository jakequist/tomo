# Seed performance: plan of attack + hardening prerequisites

Status: **COMPLETE through Phase 2 (2026-07-23).** Hardening wave (H1-H12)
landed; Phases 0/1/2 shipped on the seed-perf branch; B1/B2/B3 fixed with
strict flags hard. Final: 20k-file seed 92.5 s → **2.78 s over SSH** (33×),
2.2 s local; live edit mid-seed ~0.2 s. Phase 3 NOT recommended (see §3b).
Original plan text below for the record. The
measured baseline and the mechanism analysis live in docs/NOTES.md (benchmark
pass 2) and on the site's benchmarks page. Decision context: Jake ruled the
120× seed gap worth closing, with the core incremental path made
regression-proof FIRST.

## 0. What we measured (the constraint on the plan)

20k files / 203 MB, localhost SSH, warm, v0.2.2: tomo seed 92.5 s vs rsync
0.75 s vs Mutagen 5.2 s. strace on the receiving side: **not disk-bound** —
all 20,430 crash-safety fsyncs total 1.8 s (~2%). 66% of receiver syscall
time is waiting (futex/epoll) between files. The seed is bounded by
**per-file pipeline cadence** (~4.5 ms/file of event-loop ticks, cross-thread
handoffs, and request/response pacing). Conclusion: optimize the *rhythm*,
not the durability.

## 1. Plan of attack (three phases, measure between each)

**Phase 0 — repeatable bench harness in-repo.** `scripts/bench-seed.sh`
(tree generation seeded + parameterized; outputs the table) so every phase
lands with before/after numbers and the site stays current. Optional
lightweight stage timers behind `TOMO_TIMING=1` (env-gated prints, no
behavior change).

**Phase 1 — de-cadence the existing path (no protocol change).** Batch
manifest/content frames per pump iteration instead of per-tick trickle;
replace the ≤4-chunks-per-tick interleave with a bytes-in-flight window
(16–32 MiB); eliminate per-file round-trip waits (ship next file's frames
without waiting for the previous apply's echo). Expected: cadence-bound →
transport/CPU-bound; order-of-magnitude class win.

**Phase 2 — batch the receiver's per-file costs.** History CAS inserts
grouped into transactions (per N files or T ms); bulk-mode fsync barriers
(atomic rename per file stays — process-crash safety unchanged; per-file
fsync becomes a periodic batch barrier during bulk only); micro-costs:
created-dirs cache (drop ~20k redundant mkdir/rmdir), parent-guard cache
(drop ~154k redundant readlinks) — see hardening item H9 before touching
that one. Expected: a few × on top of Phase 1. Target after 1+2: **< 5 s**
(beat Mutagen).

**Phase 3 — true bulk mode (only if 1+2 leave us wanting rsync parity).**
Genesis is already detectable (adoption machinery): a whole-index manifest +
streamed content run, batch-applied. Protocol addition (v5, established bump
pattern). Target ~1–2 s. Decide after re-measuring.

Non-negotiables throughout: invariant #3 (a live edit mid-seed ships at
normal latency, never queued behind bulk), #4 (final state versioned), #5
(conflicts non-blocking — seeds are genesis, adoption rules apply), #8
(kill -9 at ANY seed moment: clean recovery, no tree/DB corruption).

## 2. Hardening prerequisites (land BEFORE any Phase 1 code)

Each maps a planned change → the regression risk → the net that must exist
first. "Scenario" = e2e in scenarios/; "property" = proptest in the pure
crates.

- **H1. Seed-correctness scenario (the master net).** A scaled seed
  (2–5k files, env-tunable up like the storm bounds) with FULL
  postconditions: every file byte-identical, exactly one history version per
  file per side, index roots equal, zero staging/chunk debris, db check
  green. Every phase must keep this green. (New scenario 30.)
- **H2. Kill -9 mid-seed, both sides.** Receiver killed mid-seed → restart →
  full convergence, no duplicate versions, no corrupt DB; sender killed
  likewise; repeated-kill loop (kill every ~5 s until done). Extends the
  scenario-09 idiom to the seed shape. Phase 2's batch transactions make
  this the single most important net: a crash between batch boundaries must
  re-ingest idempotently.
- **H3. Interrupted-seed resume.** SIGSTOP-partition mid-seed, heal,
  converge; assert it resumes rather than restarts (bounded re-shipping —
  frame-count ceiling like net_quiet's idiom).
- **H4. Live edits during seed (invariant #3 under bulk).** While a seed
  streams, edit files on both sides (including files not yet seeded and
  files already landed): the edits arrive within the normal latency bound
  while bulk continues; final states versioned; the concurrent-edit conflict
  case resolves per adoption/standard rules. Phase 1's batching is exactly
  where a naive queue would break this.
- **H5. Engine reconcile batching property.** Pure-engine proptest: for a
  random index pair, ANY partition of the reconcile action stream into
  batches, any batch sizes, with random interleaving of live local events,
  converges both replicas to identical canonical bytes. This is the license
  to reorder/batch in the transport without fear.
- **H6. Chunk-assembly properties at high interleave.** Out-of-order chunk
  arrival across MANY concurrent assemblies, duplicate/superseded manifests
  mid-assembly, window accounting (bytes-in-flight never exceeds the cap;
  stall/resume; slow-reader backpressure with bounded memory). Unit +
  property level in the session's assembly logic; plus scenario 21 (ENOSPC)
  extended to hit DURING a bulk batch, not just a single big file.
- **H7. Echo-suppression under batched applies.** Property/unit: a bulk
  sequence of applies produces zero fabricated change events regardless of
  batch boundaries and rescan timing (the journal's dedup keyed correctly
  when applies cluster tighter than watcher latency).
- **H8. History CAS batch semantics.** Unit: a batched ingest of N versions
  ≡ N single ingests (same content addresses, same dedup); integrity check
  green after a simulated mid-batch abort; re-ingest after abort is
  idempotent (no duplicate versions — pairs with H2).
- **H9. Parent-guard cache safety (before the readlink cache).** The
  symlink write-escape guard currently re-walks every component per apply —
  that redundancy IS the safety. Before caching: unit tests that a parent
  REPLACED by a symlink mid-batch invalidates the cache and still refuses
  the write (the TOMO_TEST_FORCE hooks pattern if timing needs forcing).
  If invalidation can't be made airtight, the cache is dropped — 154k
  readlinks cost well under a second.
- **H10. Pressure controller under seed-shaped load.** Sim test: 20k
  distinct-path single-writes (seed shape ≠ storm shape: unique paths, one
  version each) — capture lag stays bounded, every final state versioned,
  ladder recovers to floor afterward.
- **H11. Adoption at scale.** Genesis with 1k pre-divergent files (both
  sides populated, subset differing): deterministic identical winners both
  sides under the batched path. Extends scenario 22's contract to bulk.
- **H12. CI perf floor.** A generous, env-tunable seed-throughput bound in
  the seed scenario (like TOMO_STORM_MIN_WRITES) so Phase 1/2 gains lock in
  and cannot silently regress on CI's 2-core runners.

## 2b. Bugs the hardening wave EXPOSED (registry — fixes land in phases)

The scenario nets surfaced three genuine product gaps (loud WARNs today,
each with a strict-mode flag that flips to hard-fail when its fix lands):

- **B1 — receiver crash mid-seed leaves a permanent history gap** (files land
  and converge but a chunk of them never get receiver-side versions;
  invariant #4's crash case). **FIXED (Phase 2):** the startup reconcile
  (`Session::reconcile_history_completeness`) diffs the index against the
  history store's `version_identities()` and re-captures every present head
  with no recorded version, bounded through the pressure controller (H10). Flag
  `TOMO_SEED_STRICT_HISTORY=1` now hard-on in scenarios 31/32.
- **B2 — sender crash + restart duplicates versions** (the non-idempotent
  crash-retry H8 predicted: version-row dedup was a distributed caller
  contract, not a store guarantee). **FIXED (Phase 2):** schema v3 adds a
  `versions_identity` UNIQUE index on `(path, state, clock)` and
  `record_version`/`record_versions` insert with `INSERT OR IGNORE`, so a
  crash-retry double-record is a no-op that returns the existing id. Same flag.
- **B3 — live edits queue behind a running seed** (invariant #3 violated
  during bulk: a live edit's latency scales with remaining seed size —
  measured 7.7s at 2k files). **FIXED (Phase 1):** the de-cadenced pipeline
  gives live changes a priority lane through the bulk stream (~0.64s at 2k in
  Phase 1; ~0.2s after Phase 2's receiver batching shortened the apply cadence).
  Flag `TOMO_SEED_STRICT_LIVE=1` hard-on; kept green through Phase 2.

SIGSTOP/CONT (pause, not crash) preserves complete history — B1/B2 are
crash-specific. Phase acceptance now includes flipping the matching strict
flags to hard in scenarios 31/32 and keeping them green.

Also carried into phase briefs from the unit wave: shared content-addressed
chunks are served once per requesting assembly (windowing must re-serve), and
`record_version` has no store-level idempotency (see B2).

## 3b. Phase 3 verdict (recommendation, 2026-07-23)

**Do not build Phase 3.** Post-Phase-2 the seed is 2.78 s vs rsync's 0.75 s —
a factor of 3.7, not orders of magnitude, on rsync's best-case localhost turf.
A dedicated bulk protocol (v5, a second transfer path to maintain and
crash-harden) buys at most ~2 s on a 20k tree. The complexity/benefit ratio
says stop here; revisit only if real-world trees (500k+ files) surface a wall.

## 3. Sequencing

1. Hardening wave: H1–H12 (tests only; parallelizable across agents — engine
   properties, session/assembly units, scenarios are disjoint).
2. Phase 0 harness, then Phase 1 → re-measure → Phase 2 → re-measure →
   site/NOTES update each time.
3. Phase 3 go/no-go on the numbers, with its own SPEC section if it goes.
