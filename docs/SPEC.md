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
- **`~/.ssh/config` resolution.** The user-given target is resolved through
  `~/.ssh/config` *first*, exactly as the user's own `ssh` would — a minimal,
  pure parser in `tomo-transport` (`sshconfig.rs`) reads it, and the transport
  connects to the resolved endpoint. Reading the config means `tomo`
  authenticates and connects wherever `ssh host` already works — without it, a
  machine whose key is agent-less and non-default-named, or that is only
  reachable through a jump host (both common on macOS), fails even though
  `ssh host` succeeds. Supported directives (first-obtained-wins per
  `ssh_config(5)`, `IdentityFile` accumulates):
  - `Host` pattern blocks (`*`/`?`/`!`) and the global (pre-`Host`) section;
  - `HostName` (alias → real host; **literal only** — `%h`/other token
    substitution is not performed, which is rare in practice), `User`, `Port`;
  - `IdentityFile` + `IdentitiesOnly` (`yes` skips ssh-agent keys);
  - `StrictHostKeyChecking` (`yes`/`no`/`accept-new`/`ask` — `ask` is treated as
    `yes` since `tomo` is non-interactive);
  - `UserKnownHostsFile` (one or more paths; default `~/.ssh/known_hosts` +
    `~/.ssh/known_hosts2`; `/dev/null` = nothing known) and `GlobalKnownHostsFile`
    (default `/etc/ssh/ssh_known_hosts` + `…_known_hosts2`, always consulted for
    lookup, never recorded into);
  - `ProxyJump` (comma-separated `[user@]host[:port]` chain, each hop itself
    resolved recursively with a cycle guard and depth cap of 8; `none` disables);
  - `Include` (glob-expanded, relative to `~/.ssh`, processed in place).
  Unknown keywords are ignored (their names collected for a debug line). Set
  `TOMO_SSH_CONFIG=<path>` to point the transport at a specific config file
  instead of `~/.ssh/config` (test hermeticity and power-user redirection).
- **SSH authentication** tries keys in this order (first accepted wins):
  ssh-agent (unless `IdentitiesOnly yes`) → the `[remote] identity` recorded by
  `tomo connect --identity <path>` → the `IdentityFile`s that `~/.ssh/config`
  declares for the resolved host → the built-in `~/.ssh/id_ed25519`/`id_rsa`.
  Encrypted (passphrase) keys are out of scope for v0.
- **Host-key policy** honours the per-host `StrictHostKeyChecking`: `no` accepts
  any key unpinned (logs a note, records nothing); `accept-new` accepts and
  *records* an unknown key but rejects a *changed* key with the usual MITM error;
  `yes`/default keeps the strict behaviour. **Lookup** spans every user
  known-hosts file *and* the global set (OpenSSH parity); **recording**
  (accept-new) targets only the first writable user file (never the global set,
  never `/dev/null`). For a non-default port the `[host]:port` form is tried
  first, then — matching OpenSSH's "found matching key w/out port" compatibility
  — the plain port-less `host` form of the same files (a port-form
  match/mismatch always takes precedence; a plain-form match connects and logs a
  compat note; a plain-form mismatch is a full mismatch). Recording always uses
  the port-qualified form. The not-found error names both lookup keys tried
  (`[host]:port (and host without port)`) and lists every file consulted, so a
  report self-diagnoses. Before negotiation, Tomo scans those same files (with
  the same port fallback) for the key
  *types* already recorded and biases the host-key-algorithm order toward them
  (as OpenSSH does) — otherwise a host recorded only under, say, ECDSA, or a
  `[host]:port` entry for a non-default port, would be reported "not found"
  because the static library order negotiates ed25519 first. The set is never
  shrunk, so unknown hosts still negotiate normally (accept-new/`no` keep
  working). `tomo dev ssh-route <target>` prints the fully-resolved route (the
  `ssh -G` analogue: per-hop hostname/port/user/identities/policy and the
  known-hosts files consulted) for diagnosis.
- **ProxyJump** connects the first hop over TCP, then reaches each further hop by
  opening a `direct-tcpip` channel on the previous hop's session and running a
  fresh SSH client over that channel's byte stream — chained left-to-right, each
  hop authenticated with its own resolved identity settings. An unreachable hop
  produces an error naming which hop failed.
- A raw TCP/QUIC transport is a possible future optimization, not v0.

## 3. Remote bootstrap (zero friction)

On `tomo connect user@host:/path/to/project`:

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
├── README.md        # agent-context brief (see §4.1); regenerated, never synced
├── db/              # SQLite metadata + content-addressed chunk store
├── bin/             # (remote side) pushed binaries: tomo-<version>-<triple>
├── staging/         # in-flight transfers before atomic rename
├── state/           # persisted index, status.json, scan cache, session lock
└── logs/
```

`.tomo/**` is **hardcoded-ignored** at the lowest layer of the event pipeline
(a constant, not a config default) — Tomo must never watch, sync, or version
its own state. Same principle as git ignoring `.git`.

### 4.1 Agent-context README (decided 2026-07-21)

A synced tree is increasingly worked on by *coding agents*, and a tree whose
files mutate on their own — a peer's edits landing in milliseconds — violates
every assumption an agent brings ("the file I read is the file that's there";
"a diff I didn't write is a bug to fix"). Tomo therefore writes a small,
mostly-static **`.tomo/README.md`** addressed to that agent. It states, in the
product's voice: files here may change at any moment (re-read before editing;
never revert changes you didn't make — they are probably the peer's work
arriving); your saves propagate instantly, including half-finished ones; and the
killer fact — every save is versioned, so overwritten work is recoverable
(`tomo log` / `tomo restore`), and conflicts never block sync (both versions
survive, `tomo conflicts` lists them). It also names what does **not** sync
(node_modules, venvs, `.git`, IDE dirs, …), warns against killing the live
`sync`/`serve` process, and offers a one-line snippet to paste into the user's
own `CLAUDE.md`/`AGENTS.md`.

Rules that make it safe and useful:

- **Tomo never writes to `CLAUDE.md`/`AGENTS.md`** (or any file outside
  `.tomo/`). It only *offers* a snippet; adopting it is the user's choice.
- **It is generated per machine, and never synced.** Because `.tomo/**` is
  hardcoded-ignored (§4), each side writes its own copy with side-specific
  content: the literal path to the binary that serves *this* project (on a
  bootstrapped remote, `.tomo/bin/tomo-<ver>-<triple>` — not on `PATH`; from
  `std::env::current_exe`), and the peer identity as of the last write.
- **Version-stamped, churn-free.** The file carries a template-version marker
  (`<!-- tomo-readme-v1 -->`). It is (re)written only when absent or carrying a
  *different* marker; a matching marker is left untouched (so an upgrade
  regenerates it, but a steady session never rewrites it, and a user's edits to
  a current-version file are preserved). Rendering is a pure function; writing is
  best-effort — a failure warns and continues, since syncing matters more.
- **Written by `tomo init` and at session/serve startup on both sides**, so
  pre-existing projects gain it on the next sync and the remote gets it at
  bootstrap.

**Peer identity in `status.json`** (no protocol change). The README points at
live data — `cat .tomo/state/status.json` or `tomo status --json` — for who is on
the other end. `status.json` gains an additive `peer` block
(`{name, addr, source}`, fields null when unknown). The serving side learns it
from its environment: the initiator prepends `TOMO_PEER_NAME=<local hostname>` to
the remote command line (SSH `SendEnv`/`AcceptEnv` is *not* forwarded by default,
so the value is passed as `env NAME=value …` and shell-quoted so a hostile
hostname cannot break the command), and the client IP comes from
`SSH_CONNECTION`; `source` is `ssh-env`. The initiator side fills the block from
the configured `[remote]` (`source` is `config`). The block is additive — existing
`status.json` consumers (the scenarios) are unaffected.

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
  content hash, then the executable head, then larger canonical clock encoding
  — so they converge to the identical winner with zero negotiation. (Equal
  hashes mean identical content, so the SPEC's original "then replica ID"
  tiebreak is unreachable for state selection.) The winner is arbitrary but
  consistent; correctness comes from the guarantee that *nothing is lost*:
- **Genesis adoption tiebreak (decided 2026-07-21).** The standard order above
  is deliberately content-derived, so at *first contact between two
  pre-existing trees* it is a per-file coin flip: every file pair is concurrent
  (disjoint genesis clocks like `{mac:1}` vs `{vm:1}`), and "higher hash" has no
  relation to which copy the human actually wants. A stale copy routinely won.
  The fix: an entry is in **adoption mode** iff it has ≥2 heads whose vector
  clocks have **pairwise-disjoint replica support** (no replica id appears with
  a positive counter in any two heads). That is the exact, and only, signature
  of genesis — every head names solely its own originating replica, so the
  clocks provably carry zero ordering information. In adoption mode the order
  becomes: **Present > Tombstone, then larger `mtime_ms` (adopt the more
  recently modified copy), then** the existing hash → exec → canonical-clock
  chain as tiebreaks. `ContentSig` gains `mtime_ms` (wall-clock mtime in ms)
  purely as *carried metadata*: it is excluded from equality, from the canonical
  digest, and from all change detection (a bare `touch` is never a change), and
  is serialized only so it can reach the peer for this tiebreak. Because the
  mode is a pure function of the head set, both replicas compute the identical
  winner with zero negotiation (invariant #5). It is genesis-only *by
  construction*: after any first sync every version's clock includes the other
  replica's counter, so support overlaps forever after and steady-state or
  offline-then-reconnect divergence never consults mtime (invariant #7's
  carve-out). Tombstones cannot exist at genesis, so in practice adoption mode
  only ever arbitrates Present-vs-Present; the degenerate cases are still
  ordered so the relation stays total. **Clone caveat:** `git clone` (and any
  copy) stamps *fresh* mtimes on old content, so a genuinely older local edit
  that was never pushed can lose to a fresher-mtime clone of the same path. The
  loser is, as always, preserved in history and surfaced non-blockingly; this is
  an accepted trade for making the common case (adopt the machine that was
  actually just edited) correct.
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

#### File↔dir type replacement (edge-case 5, decided during apply-hardening)

Because directories are implicit, a *path* can flip between being a file and
being a directory. On the sending side a `foo` → `foo/…` replacement (or the
reverse) is observed as an ordinary pair of changes — `Removed(foo)` plus
`Modified(foo/child)` (the live watcher emits both; the startup/rescan
`scan_diff` walk derives the same pair) — and each propagates independently.

The interesting decisions are all on the **receiving** side, where the two
facts collide on one inode name. The applier resolves every such collision with
one total, deterministic rule:

> **The directory wins.** A directory is the implicit container of one or more
> *present* synced descendants — real data that cannot be silently dropped —
> whereas a file colliding with it is a single version whose bytes we can
> preserve to history. "Path P has a present descendant" is a pure function of
> the (converged) index, identical on both replicas, so both sides reach the
> same outcome with no negotiation. The colliding **file head converges to a
> tombstone** (recorded in history first — invariant #5), and the directory and
> its children remain.

Concretely, the receiver handles three shapes non-fatally (skip → note →
schedule a reconciling rescan; the session never dies — invariant #5):

| Situation on the receiver | Action |
|---|---|
| Applying `foo/x`, but `foo` exists as a **file** | Preserve `foo`'s bytes to history, remove it so the directory can be created, then write `foo/x`. The rescan emits `Removed(foo)` → tombstone. |
| Applying a **file** `foo`, but `foo` exists as a **directory** | Keep the directory; preserve the incoming file version to history; skip the write. The rescan emits `Removed(foo)` → tombstone. |
| Deleting `foo` (a file-removal), but `foo` is now a **directory** | Never `rm -r` on a file-removal: keep the directory, note, rescan. |

The **concurrent** case — replica A independently creates a file `foo` while
replica B independently creates a directory `foo/x`, both while partitioned —
falls out of the same rule. The engine's per-path conflict machinery cannot
express it (`foo` and `foo/x` are *different* paths, so both legitimately
"win"), which would leave `foo` needing to be a file and a directory at once.
The applier's dir-wins resolution makes it total: both replicas converge to
`foo` tombstoned + `foo/x` present on disk, with the losing file `foo` retained
in history. The applier never trusts wall clocks or replica identity for this —
it is a structural property of the index, so convergence is guaranteed. The
symlink write-escape guard (edge-case 4) runs *before* this resolution, so a
symlinked parent is refused rather than mistaken for a directory obstruction.

#### File→symlink replacement (v0 semantics, decided in the tier-2 batch)

**Symlinks are never synced in v0** (permissions/symlink fidelity across
macOS↔Linux is `[open]`, §12). The index tracks regular files only: the watcher
and the startup/rescan scan judge every path on its own `lstat`, and a symlink is
not a regular file, so it carries no `ContentSig`.

The consequence is deliberate and stated here so it is not mistaken for a bug:
**a tracked file being replaced by a symlink is observed as a deletion of that
file.** Concretely, when `foo` was a synced regular file and becomes a symlink,
the re-stat finds a non-regular type where a file used to be, so the change
resolves to `Removed(foo)` — exactly as if `foo` had been deleted. That deletion
propagates normally: the peer's `foo` is **tombstoned**, and (invariant #5) the
last regular-file bytes remain recoverable from history (`tomo log foo` /
`tomo restore foo`). The symlink itself is not shipped, created, or versioned on
either side. The reverse (a symlink replaced by a regular file) is an ordinary
`Modified` — the file syncs normally, the symlink having never been tracked.

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

**Built-in default ignores (decided).** Unless `[sync] default_ignores = false`,
Tomo prepends a small set of built-in `ignored` rules, applied *before* any user
rule (so a user rule for the same glob overrides them — last match wins). They
cover common editor/tool temp files and, critically, **git metadata**:

- editor/tool temp: `**/*.swp`, `**/*.swx`, `**/.*.sw?`, `**/*~`, `**/.#*`,
  `**/#*#`, `**/4913`.
- git metadata: `**/.git` and `**/.git/**` — the root repo, nested repos, and
  submodules. (`.git` is a directory in a clone but a *file* in a worktree/
  submodule, so the bare `**/.git` covers both.) Syncing `.git` would cross-
  contaminate two independent repositories' HEAD/index/objects; two `.git` trees
  must ignore each other entirely. A `.git/**` rule with an explicit class
  re-includes it for the rare user who wants it.
- dependency / environment / cache trees: `**/node_modules`, `**/.venv`,
  `**/venv`, `**/__pycache__`, `**/.pytest_cache`, `**/.mypy_cache`,
  `**/.ruff_cache`, `**/.terraform` — each paired with a `**/<dir>/**` for its
  contents, exactly like `.git`. All are large, machine-regenerable, and
  frequently *platform-specific* (native `node_modules` addons, absolute-path
  venvs, Terraform provider binaries), so carrying them across a Mac↔Linux pair
  is wasted bytes at best and broken on the peer at worst. Overridable like every
  default.
- IDE / editor project dirs (decided 2026-07-21, reversing the earlier "mixed
  intent" call): `**/.idea`, `**/.vscode`, `**/.vs`, `**/.fleet`, `**/.zed` —
  each paired with `**/<dir>/**` — plus Sublime's per-user
  `**/*.sublime-workspace`. They mix shareable settings with machine-local state
  (indexes, caches, absolute SDK paths) that churns constantly and is wrong on
  the peer; where a team checks them in, git carries the shared copy and tomo
  staying out avoids fighting it with per-machine churn. Teams that want them
  synced re-include with the standard two-rule pair.

**Deliberately NOT default-ignored (decided).** Tempting categories left out on
purpose. **Build-output dirs** (`target/`, `build/`, `dist/`): the product's
flagship use case is a remote build spraying artifacts that flow *back* to the
laptop as `synced+unversioned`, `pull`-only content — ignoring them by default
would break that headline feature, so a user opts out with a single one-line
`ignored` rule instead. **`.env`**: frequently the very file the remote needs to
run the app, so a default ignore would silently break deploys. **Eclipse's
`.settings/`/`.project`/`.classpath`**: names too generic and conventionally
committed, unlike the unambiguous dot-dirs above. **`*.sublime-project`**: the
shareable half of Sublime's pair — only the per-user workspace file is ignored.

**Ignore classes are enforced on receive as well as send (decided).** Class and
direction gate a change at *both* sync boundaries, not only when shipping. An
ignored-class (or wrong-direction) path is refused at ingress — never applied,
never absorbed into the engine, never recorded in history — exactly as it is
never shipped from local heads (including the reconcile head-shipping loop). This
is required for correctness, not just tidiness: a peer on an older binary, or a
stale index head left by a pre-upgrade sync, can still present a `.git` path;
send-side filtering alone would let it in. The decision is a pure function of
`(class, direction, flow) → Ship | Apply | Drop`. A single dim note per ignored
top-level prefix surfaces the refusal without spamming a line per file.

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
target is a different concurrent head. The M5 chunk-transfer variants were added
before anything shipped, so they did not move the protocol version off `1`.

**Protocol v2 — the executable bit (decided).** `ContentSig` gains an `exec`
flag (the Unix user-execute bit, git's model; see §12), so every `Modified`
change and the whole `IndexExchange` payload carry one extra byte per present
signature — a shape an older `postcard` decoder would misread. `PROTOCOL_VERSION`
is therefore bumped `1 → 2`. This is safe for the zero-friction bootstrap (§3):
an exact-version match reuses the pushed peer binary and *any* mismatch re-pushes
a fresh one, and the `Hello` handshake re-checks the binary version and re-pushes
on a mid-upgrade skew *before* any index is exchanged — so after a successful
handshake both ends always speak the same protocol version. The content hash
stays content-only, so the history CAS still deduplicates a chmod against the
identical bytes it already holds; only the sig's `exec` field (and the history
`versions.exec` column) records the mode.

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

`init`, `sync`, `connect`, `status`, `log <path>`, `restore <path>
[--version]`, `conflicts [list|resolve]`. All informational commands support
`--json` from day one (scenario assertions depend on it). Human output is
concise; conflict notifications are visible but never block.

**`sync` is the primary command (decided; renames/subsumes `watch`).** Earlier
drafts split "start syncing" into `tomo connect <target>` (record + validate the
peer) followed by `tomo watch` (run the loop). That two-step is now one:
`tomo sync [<host:path>] [--local-peer <path>] [--force]`.

- With a `<host:path>` target: records the `[remote]` if it is new (reusing
  `connect`'s write plumbing) and goes **straight into the live session** — the
  session's own bootstrap + `Hello` handshake *is* the validation, so there is no
  separate validation pass. An identical already-recorded peer just runs; a
  different target is refused unless `--force`.

**Target syntax: `host:path` colon form only (decided).** `sync` and `connect`
name the peer as a single rsync-style argument `[user@]host:PATH` (e.g.
`tomo sync dev@box:~/proj`). It is split at the **first `:` outside a `[...]`
group**, so bracketed IPv6 literals are safe (`[::1]:/srv` → host `[::1]`, path
`/srv`); an empty path after the colon is an error, and a bare target with no
colon is an error naming the `host:/path` form. The earlier two-argument
`<ssh-target> <remote-path>` form was **removed** — its unquoted-`~` footgun (the
*local* shell expanding `~` before tomo saw it) made it too error-prone; a stray
second positional is caught and turned into a message showing the combined
`host:/path` (or `host:~/path`) equivalent. A remote path of `~` or starting
`~/` is expanded **server-side** against the SSH user's home (SFTP
`realpath(".")`) before mkdir/bootstrap/serve-spawn, so `host:~/proj` lands in
the remote home; `~user/` is a clean "not supported" error. As a guard, a remote
path that is absolute *and* under the **local** `$HOME` is rejected before any
SSH with a copy-pasteable fix — that is almost always a shell that expanded an
unquoted `~` before tomo saw it.
- With no target args: runs against the configured `[remote]`, or a
  `--local-peer <path>` directory, or watch-only (printing a one-line hint) if
  neither is configured.
- `tomo connect` still exists as standalone **record + validate without starting
  a session** (a health check / one-shot bootstrap). `tomo watch` remains as a
  hidden, deprecated alias for a bare `tomo sync` (prints a one-line note).

**Single-session lock (decided).** A live `sync`/`serve` session holds an
exclusive advisory `flock` on `<project_root>/.tomo/state/session.lock` for its
lifetime (via `fd-lock`, §11), so a project can never have two concurrent
sessions racing its tree, index, staging, and history DB. Acquired in every
session mode (sync over SSH / local-peer / watch-only, and `serve --stdio`);
read-only commands (status/log/diff/conflicts/db/restore) never touch it. A
second session is refused fast with the holder's pid and age. **The lock is the
flock, not the file's contents** — the kernel releases it on process exit,
including `kill -9`, so there is no stale-pidfile logic; the file's `pid`/`mode`/
`since_unix_ms` bytes are diagnostics only. This is also what makes the M5
reconnect safe: a dead `serve` releases its lock via the kernel, and the
respawned one re-acquires it (offline-queue scenario 10). A remote `serve`
refused by its lock writes the error to stderr and exits nonzero; the sync side
surfaces that stderr tail so the user sees "another session is already running"
rather than a bare EOF.

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
| `unicode-normalization` (tomo-watch) | NFC canonicalization of path names entering from a *normalizing* local filesystem (APFS returns NFD from `readdir`), so an NFD name and its NFC original collapse to one `RelPath` and cannot ping-pong (macOS↔Linux filename semantics). Pure, `no_std`-friendly, the standard implementation (same crate `regex`/`idna` use); applied only when the startup FS probe reports normalization, so byte-preserving filesystems (Linux) stay byte-faithful. |
| `blake3` (watch/history) | Content hashing per §6.1; fast, pure Rust. |
| `postcard` (proto/persistence/watch) | Compact serde binary codec for frames, index persistence, and the startup-scan cache (`tomo-watch`); pure Rust, varint, stable. Chosen over bincode (maintenance mode) and JSON (can't encode non-string map keys). |
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
| `fd-lock` (tomo) | Single-session lock per project via flock: kernel-released even on kill -9 (no stale-pidfile logic), identical semantics on Linux and macOS. |
| `signal-hook` (tomo) | Clean SIGTERM/SIGINT shutdown: flush index/status/history, reap the serve child. Without it every terminated watch orphaned its child and left a stale "connected" status. |
| `mimalloc` (tomo, musl only) | musl's default allocator is slow (§3); mimalloc is the global allocator for `cfg(target_env = "musl")` builds only. `default-features = false` (no `secure`/telemetry); glibc/dev builds never pull it. Registered without `unsafe` in our code, so it coexists with workspace `forbid(unsafe_code)`. |
| `owo-colors` (tomo) | The CLI's visual identity (coral accent, glyphs, diff/log/status coloring). Pure-Rust, zero-dependency, `no_std`-friendly, and — crucially — carries no global runtime state: styling is decided once at startup by `crate::style` (stdout `IsTerminal` + `NO_COLOR`/`TOMO_COLOR`/`TERM`/locale) and every helper is a no-op when disabled, so piped, `NO_COLOR`, `--json`, and serve output stay byte-identical to plain text. Styling lives only in the `tomo` crate (libraries never print). |

Anticipated: `clap`, `serde`, `rusqlite` (bundled), `blake3`, `zstd`,
`fastcdc`, `notify` (or direct FSEvents/inotify), `russh`, `tokio`,
`thiserror`, `proptest` (dev), `tempfile` (dev), `mimalloc` (musl builds).
Licenses must be MIT-compatible; enforce with `cargo deny`.

## 12. Open questions **[open]**

- Rename detection (inode/content-hash heuristics) — v0 may treat rename as
  delete+create; history-level rename tracking matters for the git ambition.
- **Permissions — v0 subset decided: the executable bit.** `ContentSig` carries a
  single `exec: bool` (the Unix user-execute bit, exactly git's model). It is part
  of the signature (so a chmod-only change is a real change that propagates,
  versions, and — when concurrent — conflicts and resolves deterministically),
  the watcher observes `chmod` (permission/metadata events re-stat), the applier
  sets the final mode to `0o755`/`0o644` under staging+atomic-rename, history
  round-trips it (`versions.exec`, schema v2), and `tomo restore` restores it.
  The content hash stays content-only, so CAS dedup is unaffected. **Still
  `[open]`:** full mode/permission fidelity, a umask-aware apply mode, ownership,
  setuid/sticky bits, and extended attributes across macOS↔Linux.
- Symlink fidelity across macOS↔Linux (symlinks remain untracked in v0).
- History GC/compaction policy ("baked in forever" vs. disk reality — likely
  opt-in pruning, never silent).
- Multi-replica (>2) sync; the clock design already permits it.
- Windows support.

## 13. Control channel (local socket) — graduated from UX-V2 §2

Every session — a `tomo sync` loop and a remote `tomo serve --stdio` loop alike
(they share the session structure; a local agent on the remote machine is a
client too) — serves a **unix-domain socket** at
`<project_root>/.tomo/state/ctl.sock`. State stays inside `.tomo/` (invariant
#2), and `.tomo/**` is hardcoded-ignored (invariant #1), so the socket is never
watched or synced. `status.json` remains the cheap, remote-friendly, no-socket
poll; the socket is the low-latency **push** channel (a versioned event stream)
plus a **command channel**. It is served by a std-library `UnixListener` on a
dedicated accept thread (one handler thread per connection) — no async runtime,
no new dependencies (`serde_json` was already present). The engine stays a pure
state machine (invariant #6): the control server is an adapter in the `tomo`
crate, not an engine concern.

**Lifecycle.** The socket is bound at session startup and removed on clean
shutdown (and by the server's `Drop`). A stale socket left by a `kill -9`'d
predecessor is removed at startup before binding — the single-session flock
(already held) guarantees no live owner, so the removal is unconditional and
safe.

**Framing.** Newline-delimited JSON, one object per line. The client's **first
line** selects the mode:

- `{"v":1,"mode":"events"}` — the server streams event records until the client
  disconnects.
- `{"v":1,"mode":"command","cmd":{…}}` — the server executes one command,
  replies with one JSON result line, and closes.

**Versioning (API contract).** Every record carries `"v":1`. The schema is
**additive-only from the moment it ships**: no field is removed or repurposed,
new fields may be added, and unknown fields are ignored on parse (a newer
client/server interoperates with an older one). The scenarios assert on these
shapes (scenario 23), so the contract extends the existing `--json` guarantees
to the event stream.

**Slow-client policy (invariant #3: sync latency is never sacrificed).** Each
subscriber has a bounded queue (1024 lines). Publishing is non-blocking: a
subscriber that falls behind is disconnected — never allowed to back-pressure
the sync loop — after its `lagged` flag is set, so its consumer emits a final
best-effort `{"v":1,"event":"lagged"}` line before closing. The broadcast/queue
logic is a small, I/O-free, unit-tested module (fill / lag / disconnect); memory
is bounded by construction.

### 13.1 Event stream

Structured versions of everything the live session prints, plus session-state
changes and a periodic heartbeat. Each record is `{"v":1,"event":"<name>",…}`:

| `event` | Fields | Meaning |
|---|---|---|
| `connected` | `peer_name` (str\|null), `peer_addr` (str\|null) | Peer handshake completed. |
| `disconnected` | — | Peer session dropped (disconnect or clean shutdown). |
| `synced` | `path` (str), `size` (u64) | A file was applied from the peer (incoming). |
| `sent` | `path` (str), `size` (u64) | A local change was shipped to the peer (outbound). |
| `removed` | `path` (str) | A file was removed by a peer deletion. |
| `conflict` | `id` (i64\|null), `path` (str), `winner` (`"local"`\|`"peer"`), `adopted` (bool) | A concurrent edit was resolved (non-blocking, invariant #5). `id` matches `tomo conflicts list`; `adopted` marks a genesis first-sync adoption. |
| `transfer` | `path` (str), `done` (u64), `total` (u64) | In-flight large-file transfer progress. |
| `note` | `message` (str) | A one-off informational note not tied to a path. |
| `error` | `message` (str) | A non-fatal error worth surfacing. |
| `heartbeat` | `last_sync_ms_ago` (u64\|null), `unresolved_conflicts` (u64) | Periodic (~1 Hz while a subscriber is attached) liveness/status beat for the TUI status line. Emitted only when someone is watching, so an idle unobserved session stays fully idle. |
| `lagged` | — | Final best-effort line to a subscriber being dropped for lagging. |

The reporter that already renders the session's human output is tapped with a
broadcast handle, so the same call sites that print also publish records — no
logic is duplicated. Events needing data the print path lacks (a conflict's DB
id and winning side, the connected peer identity, the heartbeat) are emitted by
the session where that data is known.

### 13.2 Command channel

Every command reuses the **same functions** the equivalent CLI one-shot command
runs, so the socket grants no powers the CLI lacks. DB writes go through the
existing 5 s busy timeout; tree writes flow through the crash-safe staging +
atomic-rename apply path and the live watcher ships them — identical to a
`tomo conflicts resolve` in a second terminal. Replies are `{"v":1,"ok":true,…}`
on success or `{"v":1,"ok":false,"error":"<msg>"}` on failure.

| `cmd.type` | Fields | Reply payload |
|---|---|---|
| `ping` | — | `{"pong":true}` |
| `status` | — | `{"status":{…}}` — the live contents of `status.json`. |
| `conflicts_list` | `all` (bool, default false) | `{"conflicts":[…]}` — the same array `tomo conflicts list --json` produces. |
| `conflicts_resolve` | `id` (i64), `action` (`"keep"`\|`"take"`\|`"both"`) | `keep`: acknowledge (tree untouched). `take`: adopt the loser into the tree (crash-safe apply; the watcher ships it). `both`: **not yet wired** — replies `{"error":"unsupported"}` until the CLI's `--both` lands and is connected. |
| `stop` | — | `{"stopping":true}`, then the session shuts down cleanly (the same path as SIGTERM). |

### 13.3 CLI clients

- `tomo events [--json]` — attach to the running session's socket and stream the
  feed. Default output is human lines in the same shape the live session prints
  (reusing `style.rs`); `--json` emits the raw versioned records for scripts/CI.
  A clean error ("no running session — start one with `tomo sync`") if nothing
  is running; exits cleanly when the session stops. Multiple simultaneous
  subscribers are supported.
- `tomo dev ctl '<cmd-json>'` — a hidden diagnostic (mirrors `tomo dev
  ssh-route`): sends one command object over the command channel and prints the
  reply. Used by scenario 23 to exercise the command channel without the (future)
  TUI.
