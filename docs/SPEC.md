# Tomo — Design Specification

Status: v0 (pre-implementation). This document is authoritative. Decisions here
were made deliberately; changing one requires updating this file and stating
why. Sections marked **[open]** are intentionally undecided.

## 1. What Tomo is

Real-time, two-way file synchronization between two machines over SSH, with
complete file history as a first-class feature, zero-friction remote bootstrap,
and vector-clock-based conflict handling. CLI-only for now; a friendly API
protocol may come later. Written in Rust. MIT licensed. Long-term ambition:
evolve into a git alternative (content-addressed history is the foundation).

Primary use case: developer edits source on macOS; a more powerful Linux server
(GPU box) mirrors the tree in near-real-time; build artifacts created on the
server flow back to the Mac. Both directions, as fast as possible.

## 2. Topology and transport

- **Two replicas per sync pair** (star/mesh topologies are future work; vector
  clocks are sized for N replicas from day one so this extends naturally).
- **Everything runs over a single SSH connection.** The local `tomo` process
  starts the remote `tomo` process over SSH and speaks the binary wire protocol
  over the remote process's stdin/stdout (like `rsync -e ssh`). We inherit
  SSH's auth, encryption, and firewall traversal. No listening ports.
- Use a Rust SSH library (e.g. `russh`) with an SFTP subsystem for the
  bootstrap file push — do **not** shell out to `scp` (deprecated/absent on
  some systems).
- A raw TCP/QUIC transport is a possible future optimization, not v0.

## 3. Remote bootstrap (zero friction)

On `tomo connect user@host /path/to/project`:

1. Open SSH session. Detect remote OS/arch via `uname -s`/`uname -m`.
2. Look for `<remote_project>/.tomo/bin/tomo-<version>-<triple>`.
3. If present **and the version is an exact match** with the local binary:
   exec it. Any version mismatch, however small → push a fresh binary.
4. If absent/mismatched: push the matching embedded binary via SFTP to
   `.tomo/bin/`, `chmod +x`, verify SHA-256 after copy, then exec.
5. Version handshake as the first protocol exchange; mismatch → re-push.
6. Unsupported target triple → **fail cleanly with an explicit message**. No
   external downloads, ever. All variant binaries are embedded in the release
   binary (`include_bytes!` of release artifacts; skipped in dev builds so the
   edit-compile loop stays fast).

Supported triples for v0 releases:
`x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`,
`x86_64-apple-darwin`, `aarch64-apple-darwin`. Windows later.

**Linux binaries are fully static musl builds** (no glibc-version roulette on
old servers). Consequences: `rusqlite` with `bundled` feature; rustls, never
OpenSSL; swap in `mimalloc` (musl's allocator is slow). Expect ~40–60 MB fat
release binary; acceptable for a dev tool.

Tomo is strictly user-space and project-scoped. No root, no daemons, no global
install required on the remote.

## 4. State layout — no global state

All state lives in `<project_root>/.tomo/`:

```
.tomo/
├── config.toml      # user configuration
├── db/              # SQLite metadata + content-addressed chunk store
├── bin/             # (remote side) pushed binaries: tomo-<version>-<triple>
├── staging/         # in-flight transfers before atomic rename
└── logs/
```

`.tomo/**` is **hardcoded-ignored** at the lowest layer of the event pipeline
(a constant, not a config default) — Tomo must never watch, sync, or version
its own state. Same principle as git ignoring `.git`.

## 5. Sync engine — pure state machine

`tomo-engine` is a pure function of `(current_index, incoming_event) →
(new_index, actions)`. No I/O, no clocks, no threads. Platform watchers,
transport, and storage are thin adapters that feed it events and execute its
emitted actions. This is the testability backbone: unit/property tests drive
the engine with simulated event storms and deterministic time.

### 5.1 Event ingestion / echo suppression

The hardest correctness problem: a change applied *by Tomo* fires the local
watcher, which must not re-propagate it (ping-pong / deleted-file
resurrection). Approach: Tomo journals every write it performs (path + expected
post-write hash/size/mtime signature); watcher events matching a journaled
expectation are swallowed and the journal entry retired. Writes are performed
via temp-file-in-`.tomo/staging` + atomic rename.

Watcher realities to normalize in `tomo-watch`:
- macOS FSEvents: directory-granular, coalescing → rescan/stat to resolve.
- Linux inotify: per-file, non-recursive → maintain watch descriptors for the
  whole tree; handle overflow (`IN_Q_OVERFLOW`) by falling back to a rescan.
- Editor atomic saves (write-temp-then-rename, truncate-then-write) look like
  delete+create; the canonicalization layer must emit a single coherent
  "modified" change record, never a zero-byte intermediate.

### 5.2 Vector clocks

Every replica has a stable ID (generated at `tomo init`/first connect, stored
in `.tomo/`). Each file's version carries a vector clock. Comparison yields:
happens-before (fast-forward apply), equal (no-op), or concurrent (**conflict**).
Wall-clock time is recorded for display only and never used for ordering.

### 5.3 Conflict policy

- **Index entries are multi-value registers** (updated during M1; supersedes
  the earlier single-entry model). Each path's entry holds the *set of
  concurrent causal heads* (clock + state), Dynamo-sibling style, bounded by
  the replica count. Absorbing a version is a join-semilattice operation
  (drop dominated heads, add the new one), so replicas converge under
  arbitrary delivery order — including redelivery of superseded intermediate
  versions, where the single-entry merge-on-conflict model provably diverged.
  A local edit collapses the head set: its clock is the merge of all heads
  plus a tick, which keeps each replica's per-path version stream totally
  ordered. The on-disk file always shows the deterministic **winner** head.
- **Last-writer-wins, never blocks sync.** When clocks say concurrent, both
  sides independently materialize the same winner from the head set — Present
  beats Tombstone (delete-vs-edit preserves the edit as winner), then higher
  content hash, then larger canonical clock encoding — so they converge to
  the identical winner with zero negotiation. (Equal hashes mean identical
  content, so the SPEC's original "then replica ID" tiebreak is unreachable
  for state selection.) The winner is arbitrary but consistent; correctness
  comes from the guarantee that *nothing is lost*:
- The losing version, the winning version, and the vector-clock evidence are
  recorded as a conflict row in the history DB.
- CLI surfaces conflicts **non-blockingly**: status line in `tomo watch`
  output, badge in `tomo status`, `tomo conflicts list`, recovery via
  `tomo restore`. Optional desktop notifications later.
- Delete-vs-edit is a conflict like any other; the edited content is always
  preserved in history regardless of which side wins.

### 5.4 Directories (v0 semantics, decided at M1)

The index tracks **files only**. Directories are implicit: created on demand
when a synced file needs them, pruned when applying a deletion empties them.
Consequently the *existence* of empty directories is not synchronized (an
empty dir left behind by deleting a file's siblings may exist on one side
only). First-class directory tracking is future work (it matters for the git
ambition); scenarios compare synced file sets, not bare `diff -r`.

## 6. History — the killer feature

### 6.1 Storage

Content-addressed store: FastCDC content-defined chunking, BLAKE3 hashing,
zstd compression. A 1-character change to a 10 MB file stores ~one chunk, not
10 MB. This is also the future-git foundation. Metadata (versions, vector
clocks, conflict records, path index) in SQLite via `rusqlite` (bundled).
Revisit SQLite only if it measurably becomes the bottleneck.

### 6.2 Adaptive capture (purity under light load, debounce under pressure)

Think congestion controller for history:

- Every canonical change enters a per-file staging buffer.
- **Light load → flush after a tiny entry window**: rung 0's interval is a small
  `min_capture_window_ms` (default 75 ms, decided post-M6) rather than a hard
  0 ms, so literally every save still becomes a version (it flushes ~75 ms
  later) while a same-path truncate+write pair — the 0-byte intermediate a
  `>`-style save leaves, or vim's `4913` write-probe churn — coalesces into the
  single final state instead of recording the noisy transient. This applies to
  `adaptive` only; `every-change` stays literally 0 ms. It governs history
  capture only — the live sync path is never delayed (invariant #3) — and the
  final state of every burst is always versioned (invariant #4).
- Pressure signals tracked continuously: event arrival rate, staged bytes
  awaiting chunking, history write queue depth, chunking CPU time.
- Above threshold, per-file flush interval escalates (0→75 ms entry window →
  250 ms → 1 s → 5 s), coalescing bursts into checkpoints; decays back toward
  the entry window as pressure subsides.
- **Invariant:** live sync latency is unaffected — debouncing applies to
  history capture only. **Invariant:** the final state of every burst is
  always versioned.
- The pressure controller is a pure function (lives in `tomo-engine`),
  property-tested with synthetic storms: "final content always versioned",
  "monotonic escalation under sustained load", "decay to purity when idle".

Config: `history.mode = adaptive | every-change | interval(5s) | off`
(default `adaptive`).

## 7. Configuration (`.tomo/config.toml`)

Per-path rules with three classes (glob patterns, git-style precedence):

- `synced+versioned` — source files (default).
- `synced+unversioned` — flows between machines, no history (e.g. artifacts
  you want back on the Mac without versioning them).
- `ignored` — never crosses the wire, never versioned (e.g. `target/`).

Direction control per pattern (push-only / pull-only / both) — this is
load-bearing: a wrong ignore rule plus a server build spraying `target/` would
grow history at build speed. Also: `history.mode`, connection settings.
`.tomo/**` ignore is *not* expressible or removable here (see §4).

## 8. Wire protocol

Custom binary protocol, built for speed, tunneled over SSH stdio. Length-
prefixed frames; version-negotiated on handshake. Content transfer is
chunk-based (dedup: never resend a chunk the peer has — reuses the CAS from
§6.1). Small-file latency must not suffer head-of-line blocking behind large
transfers (interleave/prioritize). A friendly API protocol (likely JSON over a
local socket) is future work for tooling/debugging/UI.

**Chunk transfer (decided, updated during M5).** A `Modified` change under
1 MiB rides inline in a `Change` frame. At or above that threshold the sender
ships a `ChangeManifest` (the change plus the ordered list of FastCDC chunk
hashes — identical 16/64/256 KiB params and BLAKE3 ids to §6.1's store, so the
manifest is CAS-coherent) and retains no per-transfer chunk bytes (only a tiny
hash→range table so a `ChunkRequest` is served by `pread`ing exactly the wanted
ranges, never re-chunking). The receiver holds the change **without absorbing
it** and pulls the chunks it lacks with `ChunkRequest` (batches of 32), staging
received bytes under `.tomo/staging/chunks/` (invariant #8 — a `kill -9` leaves
only garbage there, never a torn file at its final path); it absorbs the change
into the engine and applies it *atomically* only once every chunk verifies and
the reassembled whole-file hash matches the signature — exactly as an inline
`Change` is absorbed-and-applied together. (Absorbing early would persist an
index state the disk lacks, so a crash mid-assembly would make the restart scan
read a phantom deletion and destroy the real file; a same-path change arriving
meanwhile still supersedes the assembly, so the clock is never needed early.) The sender answers a `ChunkRequest` by re-reading
and re-chunking the *current* file (silently skipping hashes it no longer
contains — the file changed, a fresh manifest is coming, invariant #3) and ships
`ChunkData` frames a few at a time so a live small-file `Change` always
interleaves between chunk batches rather than blocking head-of-line. Apply bytes
are sourced by signature — triggering frame, else current disk, else the CAS —
so a frame carrying one conflict head's bytes can still drive an Apply whose
target is a different concurrent head. The protocol version stays `1` (these
variants were added before anything shipped).

**Reconnect / offline queue (M5).** In `watch` a transport EOF or write error is
not fatal: the loop keeps watching, indexing, and versioning locally and
reconnects with exponential back-off (2 s → 30 s). On reconnect the normal
handshake (Hello → IndexExchange → head-shipping reconcile) *is* the offline
queue — it re-ships every head, tombstones included, the peer does not already
cover. Sends attempted while offline are dropped; reconcile covers them
(invariant #5). `serve` still exits on EOF; the reconnecting `watch` respawns it
(local child) or re-runs the bootstrap-lite (SSH, reusing the binary on a
version match).

## 9. CLI

`init`, `connect`, `watch`, `status`, `log <path>`, `restore <path>
[--version]`, `conflicts [list|resolve]`. All informational commands support
`--json` from day one (scenario assertions depend on it). Human output is
concise; conflict notifications are visible but never block.

## 10. Testing philosophy

See `docs/TESTING.md`. Summary: pure-core TDD with proptest at the unit level;
executable end-to-end scenarios in `scenarios/` (real CLI, SSH to localhost,
temp dirs, optional `tc netem` latency) mirroring the acceptance-test roadmap.
Cross-cutting invariants asserted at every convergence point: quiet network,
equal index roots, `.tomo/` never syncs, history DB integrity.

## 11. Dependencies (record rationale here as they're added)

| Crate | Why |
|---|---|
| `serde` (workspace) | Canonical ser/de for engine types, config, and wire messages; ubiquitous, no better option. |
| `thiserror` (adapters/CLI) | Per-crate error enums per the hygiene policy; zero runtime cost. |
| `toml` (tomo-config) | Parse `.tomo/config.toml`; the reference TOML implementation for serde. |
| `globset` (tomo-config) | Git-style glob sets for path-class rules; same engine ripgrep uses, battle-tested. |
| `proptest` (dev) | Property tests are mandated by docs/TESTING.md Level 1. |
| `tempfile` (dev) | Filesystem fixtures in adapter tests; RAII cleanup. |
| `notify` (tomo-watch) | Cross-platform FS watching (inotify now, FSEvents later for free); the de-facto standard. |
| `blake3` (watch/history) | Content hashing per §6.1; fast, pure Rust. |
| `postcard` (proto/persistence) | Compact serde binary codec for frames and index persistence; pure Rust, varint, stable. Chosen over bincode (maintenance mode) and JSON (can't encode non-string map keys). |
| `clap` (tomo) | CLI parsing per §9; the standard. |
| `clap_complete` (tomo) | Generates bash/zsh/fish completion scripts for `tomo completions <shell>`; the companion crate to clap, derives straight from the parsed command. |
| `serde_json` (tomo) | `--json` output surfaces and the status file; display-only, never the wire format. |
| `getrandom` (tomo) | Random replica IDs at `tomo init`; minimal OS-entropy shim, no big `rand` dependency. |
| `russh` + `russh-sftp` (tomo-transport) | Pure-Rust SSH client per §2 (no scp/OpenSSL); SFTP subsystem for the bootstrap push. Pinned `>=0.62` (fixes the RUSTSEC-2026-0153/0154 remote-DoS advisories). Backend selected via `default-features = false` + `ring` (not the default aws-lc-rs) for smaller, C-free static musl builds. |
| `tokio` (tomo-transport only) | russh requires it; confined inside the transport crate behind a blocking API — the engine loop stays sync. |
| `sha2` (tomo-transport) | SPEC §3 mandates SHA-256 verification of the pushed binary (blake3 is our content hash; sha256 is the bootstrap contract). |
| `fastcdc` (tomo-history) | Content-defined chunking per §6.1; the maintained pure-Rust implementation. |
| `zstd` (tomo-history) | Chunk compression per §6.1. C binding, but the canonical zstd crate; static-links fine under musl. |
| `rusqlite` bundled (tomo-history) | History metadata per §6.1; bundled SQLite is the musl static-build requirement. |
| `signal-hook` (tomo) | Clean SIGTERM/SIGINT shutdown: flush index/status/history, reap the serve child. Without it every terminated watch orphaned its child and left a stale "connected" status. |
| `mimalloc` (tomo, musl only) | musl's default allocator is slow (§3); mimalloc is the global allocator for `cfg(target_env = "musl")` builds only. `default-features = false` (no `secure`/telemetry); glibc/dev builds never pull it. Registered without `unsafe` in our code, so it coexists with workspace `forbid(unsafe_code)`. |

Anticipated: `clap`, `serde`, `rusqlite` (bundled), `blake3`, `zstd`,
`fastcdc`, `notify` (or direct FSEvents/inotify), `russh`, `tokio`,
`thiserror`, `proptest` (dev), `tempfile` (dev), `mimalloc` (musl builds).
Licenses must be MIT-compatible; enforce with `cargo deny`.

## 12. Open questions **[open]**

- Rename detection (inode/content-hash heuristics) — v0 may treat rename as
  delete+create; history-level rename tracking matters for the git ambition.
- Symlinks, permissions/xattr fidelity across macOS↔Linux.
- History GC/compaction policy ("baked in forever" vs. disk reality — likely
  opt-in pruning, never silent).
- Multi-replica (>2) sync; the clock design already permits it.
- Windows support.
