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

## SSH-config semantics (2026-07-18, `ssh-config-semantics` branch)

Extended `~/.ssh/config` support from IdentityFile-only to full connection
resolution, motivated by a real Mac failure: `Host vm1` with `HostName`,
`StrictHostKeyChecking no`, `UserKnownHostsFile /dev/null`, `ProxyJump p1`, and a
custom `IdentityFile` — `tomo sync vm1 …` failed "host key not in known_hosts"
(and could not reach vm1 anyway, since it is only reachable via p1).

- **Parser (`tomo-transport/sshconfig.rs`)** — still a pure, exhaustively
  unit-tested parser (text in, structured data out; the only I/O is
  `SshConfig::load`, which expands `Include` against the filesystem). Now
  resolves `HostName`/`User`/`Port`, `IdentityFile`(+`IdentitiesOnly`),
  `StrictHostKeyChecking` (`ask`→`yes`), `UserKnownHostsFile`, `ProxyJump`
  (recursive, cycle-guarded, depth cap 8, `none` disables), and `Include`
  (glob, relative to `~/.ssh`, in place). First-obtained-wins; `IdentityFile`
  accumulates. Unknown keywords ignored but their names collected. `%h` token
  in `HostName` intentionally NOT substituted (literal only) — rare, documented.
- **Connection (`ssh.rs`)** — the target is resolved into a `ResolvedRoute`
  (jumps + target). Host-key policy is a pure, unit-tested decision function
  (`decide_host_key`: known/unknown/changed × yes/no/accept-new). `accept-new`
  records via russh `learn_known_hosts_path` into the first non-`/dev/null`
  known_hosts. ProxyJump chains with russh `channel_open_direct_tcpip` +
  `client::connect_stream` over the channel's `ChannelStream`; jump handles are
  held in the session/`RemoteGuard` so the tunnel stays open. Each hop
  authenticates with its own identities.
- **`TOMO_SSH_CONFIG`** env override added (transport reads it instead of
  `~/.ssh/config`) — makes scenario 16 hermetic and is generally useful.
- **Connect log line** now names the resolved endpoint, e.g. `connecting to vm1
  (10.0.0.71 via p1) over SSH`; host-key notes ("accepting unverified host
  key…", "recorded new host key…") surface through the reporter (the library
  never prints — notes flow up via `RemoteGuard::notes`).
- **Scenario 16 (`16_ssh_config.sh`)** — hermetic `TOMO_SSH_CONFIG` against
  self-SSH: (a) alias→HostName + custom IdentityFile + `StrictHostKeyChecking
  no` + `/dev/null` converges with no known_hosts; (b) ProxyJump localhost→
  localhost proves the direct-tcpip chain end-to-end vs real sshd; (c)
  `accept-new` records once then reuses silently. 3× green; the real
  `~/.ssh/known_hosts` is checksummed unchanged.
- **CLI wiring** — kept minimal and SSH-config-scoped: `crates/tomo/src/
  transport.rs` (`describe_route`, `Transport::notes`) and `session.rs` (the
  enriched connect line + printing host-key notes). Note the OpenSSH client's
  own `-J localhost localhost` "jumphost loop" heuristic does NOT apply to tomo
  (russh + our resolver guard config-alias cycles, not same-host forwards),
  so localhost→localhost jumping is a valid, tested path.

## Host-key algorithm negotiation (2026-07-18, `hostkey-algo-negotiation` branch)

Follow-up bug from the ssh_config work, root-caused with a live repro on this VM
against v0.1.2. SYMPTOM: `tomo sync vm1 …` → "host key for p1 is not in
known_hosts" though `ssh p1` works. ROOT CAUSE: a known_hosts entry whose key
TYPE differs from what russh negotiates is reported Unknown — russh uses its
static host-key-algorithm order (ed25519 first) and negotiates ed25519, but the
file only has, e.g., ECDSA. OpenSSH avoids this by reading known_hosts first and
ordering `HostKeyAlgorithms` so already-recorded types are preferred. Exact
repro: `ssh-keyscan -t ecdsa 127.0.0.1 > kh; UserKnownHostsFile=kh` → fails; the
same file with all types (plain or hashed) works.

FIX (mirrors OpenSSH):
- `known_key_algos(files, host, port) -> Vec<russh::keys::Algorithm>` — the key
  types recorded for `host:port` across the hop's known_hosts. **Reuses russh's
  own `known_hosts::known_host_keys_path` line parser** (handles plain,
  `[host]:port`, comma-separated, and hashed `|1|salt|hash` entries and yields
  the recorded `PublicKey`s) → **NO new deps and no bespoke parser needed**; the
  coordinator's suggested `hmac`+`sha1` additions were avoided because russh
  already exposes exactly this. An `ssh-rsa` entry expands to the full RSA family
  (`rsa-sha2-512`/`-256`/`ssh-rsa`) so any RSA negotiation still matches.
- `preferred_key_order(known)` — recorded types first, then russh's remaining
  DEFAULT order; the set is never shrunk (empty ⇒ untouched default).
- Per-hop `client::Config` (the config was previously shared across the
  ProxyJump chain — now built per hop) sets `preferred.key` to that order.
  `StrictHostKeyChecking no` hops skip the scan (no lookup happens anyway).
- For an ecdsa-only file the produced order is: `ecdsa-sha2-nistp256`,
  `ssh-ed25519`, `ecdsa-sha2-nistp384`, `ecdsa-sha2-nistp521`, `rsa-sha2-512`,
  `rsa-sha2-256`, `ssh-rsa`.
- Tests: 8 pure unit tests for `known_key_algos`/`preferred_key_order` (plain,
  hashed-HMAC, `[host]:port` incl. port-mismatch negative, multiple types,
  ssh-rsa expansion, no-match/missing-file empty, dedup, ordering); scenario 16
  sub-check (d) — ecdsa-only known_hosts under default strict checking connects
  and converges (the p1 repro). 3× green; full suite (16) + `TOMO_LINK_MODE=ssh
  --quick` green; fmt/clippy/test workspace clean. Verified the live repro fails
  on v0.1.2 and passes with the fix.

## Known-hosts OpenSSH parity (2026-07-19, `knownhosts-parity` branch)

Third ssh-config follow-up, root-caused with real `ssh -G` data from the user's
Mac for the failing hop p1: `hostname p1`, `port 25601`, `stricthostkeychecking
ask`, user known-hosts = `~/.ssh/known_hosts ~/.ssh/known_hosts2`, global =
`/etc/ssh/ssh_known_hosts{,2}`. `ssh-keygen -F p1` showed only a PLAIN `p1`
entry, which does NOT match the `[p1]:25601` lookup key for a non-22 port; the
working `[p1]:25601` entry lived in one of the four default files Tomo never
read (we consulted exactly one, `~/.ssh/known_hosts`).

FIXES (all OpenSSH-parity):
- **Default known-hosts set.** No `UserKnownHostsFile` directive ⇒ user set is
  `~/.ssh/known_hosts` **and** `~/.ssh/known_hosts2`. The **global** set
  (`GlobalKnownHostsFile`, default `/etc/ssh/ssh_known_hosts{,2}`) is **always
  appended for lookup** (both verification and the algorithm scan). Recording
  (accept-new) still targets only the first non-`/dev/null` **user** file.
  New `GlobalKnownHostsFile` directive parsed; `ResolvedEndpoint` now carries
  `known_hosts_files` (user, defaults applied) + `global_known_hosts_files`, with
  `lookup_known_hosts()` (user++global) and `record_target()` helpers. Per-hop
  `client::Config` and the handler use the full lookup set.
- **Error transparency.** `HostKeyUnknown` now names the exact lookup key
  (`[host]:port` when port != 22) and lists every file consulted. Verbatim:
  `host key for [p1]:25601 not found (checked ~/.ssh/known_hosts,
  ~/.ssh/known_hosts2, /etc/ssh/ssh_known_hosts, /etc/ssh/ssh_known_hosts2) —
  connect once with `ssh p1` to record it, then retry` (paths shown absolute at
  runtime). A live capture: `host key for 127.0.0.1 not found (checked
  …/empty_kh, …/empty_kh2, /dev/null, …/empty_global) — connect once with `ssh
  127.0.0.1` …`.
- **`tomo dev ssh-route <target>` (+`--json`)** — the `ssh -G` analogue: per hop
  prints role/alias/hostname/port/effective-user/identity-files/agent-skipped/
  StrictHostKeyChecking/user+global known-hosts/consulted-set and the ProxyJump
  chain. Pure resolution, no network; honors `TOMO_SSH_CONFIG`. Rendering split
  into pure `route_view`/`render_human` (unit-tested).
- **Port-form matching (verified).** russh's `check_known_hosts_path` /
  `known_host_keys_path` follow OpenSSH: a plain `host` entry matches ONLY port
  22; a `[host]:port` entry matches ONLY that port. Proven by unit tests at both
  the verification level (`check_known_hosts_path`) and the algorithm-scan level.
  This is exactly the p1 failure — a plain `p1` entry can never satisfy a
  `[p1]:25601` lookup.
- **No new deps** (still reusing russh's line parser from the previous fix).
- Tests: file-set assembly (defaults, global-always-appended, override, record
  target skipping `/dev/null`/global), port-form matching (verification + scan),
  ssh-route rendering (human + json). Scenario 16 grew (e) key only in the
  SECOND `UserKnownHostsFile` (multi-file lookup) and (f) key only in a
  `GlobalKnownHostsFile` file (global consulted, never written), plus an
  ssh-route smoke asserting the printed port/hostname/known-hosts; every
  hermetic host now pins `GlobalKnownHostsFile /dev/null`. 3× green; full suite
  (16) + `TOMO_LINK_MODE=ssh --quick` green; fmt/clippy/test clean.

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

### Real Mac→Linux cross-platform sync validated (2026-07-17, Mac↔vm8)

Exercised the whole cross-platform path against a REAL Linux server (`vm8`,
x86_64 Linux, over real SSH) from the Apple-silicon Mac:

- **Cross-compile toolchain on macOS:** `brew install zig` (0.16.0) +
  `cargo install cargo-zigbuild` (0.23.0) + `rustup target add
  x86_64-unknown-linux-musl`. `cargo zigbuild --release --target
  x86_64-unknown-linux-musl -p tomo` produced a **statically linked** ELF
  (9.0 MB, `file` says "statically linked, stripped"; `ldd` on vm8: "not a
  dynamic executable") that runs on vm8 (`tomo 0.0.1`). Zig handles the C deps
  (bundled SQLite, zstd, blake3) cleanly; one benign linker warning
  ("deprecated linker optimization setting '1'"). This is exactly what CI's
  thin-linux / fat jobs do — the pipeline works from macOS.
- **Fat darwin host binary:** `TOMO_EMBED_DIR=<dist> cargo build --release -p
  tomo --features embed-binaries` on the native arm64 host embeds the musl
  artifact (`tomo dev embedded-binaries --json` → x86_64-unknown-linux-musl,
  0.0.1, 9402024 bytes). Needed because Mac(aarch64-darwin)→Linux(x86_64-musl)
  differs in BOTH arch and OS, so the dev-mode `current_exe` substitution is
  (correctly) refused — real cross-platform needs the embedded artifact.
- **Bootstrap:** `tomo connect jake@vm8 /tmp/…` with the fat binary pushed the
  embedded musl binary over SFTP to `.tomo/bin/tomo-0.0.1-x86_64-unknown-linux-musl`,
  exec'd it, and handshook at protocol v1 — "[embedded static artifact]".
- **Two-way sync:** Mac→vm8 and vm8→Mac both propagate in <1s.
- **Conflict under partition (SIGSTOP the vm8 serve child):** concurrent edits
  on both sides, heal → both converge to the IDENTICAL winner, **1 conflict row**
  recorded, conflict.txt carries **3 versions** (base + both edits), the losing
  (Mac) bytes recover byte-exact via `restore --version --stdout`. `db check`
  green on BOTH sides (darwin + linux-musl). Everything cleaned up after
  (remote /tmp dir removed, watch stopped, 0 strays). Mission step 4 ✅.

### macOS bring-up (2026-07-17, `darwin-support` branch, real Mac mini / Apple silicon)

- **Rust code is fully portable — zero source changes to build/test.** Fresh
  `rustup` stable (aarch64-apple-darwin, 1.97.1) + Xcode CLT: `cargo build`,
  `cargo test --workspace` (all unit/integration/doctests), `cargo clippy
  --workspace --all-targets -D warnings`, and `cargo fmt --check` all pass
  untouched. russh=`ring`, rusqlite/zstd bundled via `cc` — no OpenSSL, no
  aws-lc, no glibc friction. FSEvents backend (`notify` 8.2) works out of the
  box; scenarios 02 (echo/new-dir race) and 03 (editor atomic saves) — the
  flagged FSEvents risk areas — pass, and `map_event`'s imprecise-`Any`→re-stat
  mapping holds. Coalesced small-file sprays do NOT spuriously trip the
  `dir_appeared`→`NeedsRescan` guard (probed directly: `reconciling` never flips
  under a 500-file spray).

- **The real macOS work was the bash harness, not the product.** Fixes on
  `darwin-support` (all keep Linux behavior identical):
  - *`date +%s%N` / `stat -c` are GNU-only.* Added portable shims to
    `scenarios/lib/harness.sh` (`now_ms`/`now_ns`, `stat_size`/`stat_mtime`/
    `stat_mtime_ns`/`stat_inode`) that prefer native GNU, then coreutils `g*`
    (Homebrew: `brew install coreutils`), then a BSD/`perl` fallback. Ported the
    17 harness call sites plus scenarios 04/05/06/09/11/14. macOS ships a native
    `/sbin/sha256sum`, so scenario 04's hash check needed no change.
  - *Storm generators were fork-throttled on macOS.* Scenarios 06 and 14 built
    their storms with a `$(date +%s)` (and, in 06, `sleep`) fork PER iteration;
    macOS fork is dear enough that the "storm" fell below the ≥1000-write bar
    (06: 604; 14: 868). Switched the deadline to the `SECONDS` builtin (no fork)
    and, in 06, batched the pacing `sleep` across 10 writes. Now 06 ≈ 5.7k
    writes, 14 ≈ 84k unthrottled writes — genuine storms on both platforms,
    still coalescing to 1–2 versions with 0 conflicts.
  - *Watch pids were never registered for cleanup.* `start_watch` is always
    called as `WATCH="$(start_watch …)"`, so its `register_pid` ran in the
    command-substitution subshell and never reached the parent's CLEANUP_PIDS —
    watches reparented to init and accumulated (a scenario-06 orphan was still
    running mid-session, skewing timing). Added a teardown safety sweep:
    `pkill -9 -f "$WORK"` (unique tmpdir → scenario-isolated). Latent on Linux
    too; the sweep fixes both.
  - *Self-SSH host-key staleness + Linux-only setup.* `ensure_self_ssh` now
    clears stale `localhost`/`127.0.0.1`/`::1` known_hosts entries before
    re-scanning (a rotated host key otherwise makes `ssh` refuse before auth),
    and gates the `apt-get`/`service` path to Linux; on Darwin it points at
    Remote Login rather than spawning a rogue `/usr/sbin/sshd`.

- **Scenario 12 (ignore flip) needed a bigger reconnect timeout, not a fix.**
  After the flip removes the `target/` ignore rule, A's startup scan re-hashes
  the whole ~200 MiB `target/` tree BEFORE reporting connected; the DEBUG build's
  -O0 BLAKE3 takes ~18s to do that on this host (the Linux dev VM squeaked under
  the old 15s). Bumped the post-flip `wait_for` to 60s with a comment. Same
  -O0-hashing class the header of scenario 11 already documents.

- **Scenario 11 (1 GiB + churn) — interleaving holds; the ABSOLUTE latency bound
  is host-relative.** Small-file latency under the concurrent 1 GiB transfer
  peaks ~11.2s on macOS/APFS (M-series, release build) and PLATEAUS the instant
  the bulk transfer completes — batches keep landing continuously throughout
  (early batches 2.3s/7s), the process stays responsive (max `status` 8ms), the
  1 GiB arrives byte-identical, db green. That is the head-of-line-blocking
  property (interleaved, never starved), just with a peak that tracks this host's
  1 GiB-ship time rather than the dev VM's. Made the default bound platform-aware
  (Linux 10s unchanged, Darwin 20s) with a comment; a true starvation regression
  (every batch delayed by the full transfer, latency never plateauing) still
  trips it. NOT a product regression.

- **Scenario 13 (clock skew) skips on macOS by design.** libfaketime relies on
  `DYLD_INSERT_LIBRARIES`, which SIP strips for system binaries — the offset
  never reaches tomo's children. Added a Darwin-aware `skip` with that reason;
  invariant #7 is exercised on Linux and the engine's vector-clock ordering is
  platform-independent pure logic.

- **RESOLVED — ssh-mode scenarios (01–04) now green; taught the transport to
  read `~/.ssh/config`.** The blocker: this Mac is a REAL machine (not the
  throwaway VM) — `~/.ssh` has real keys (`id_tokyo`, `id_github`, …), an
  `~/.ssh/config` (global `IdentityFile ~/.ssh/id_tokyo`), and NO
  `id_ed25519`/`id_rsa`. System `ssh localhost` works (via `id_tokyo` from the
  config); but `tomo-transport` only tried ssh-agent (no `SSH_AUTH_SOCK` here)
  then the hardcoded `~/.ssh/{id_ed25519,id_rsa}` — it did NOT parse
  `~/.ssh/config`, so `tomo connect` failed "ssh-agent (no SSH_AUTH_SOCK)".
  Chose option (c), the real product fix (jake's call): a new pure `ssh_config`
  parser (`crates/tomo-transport/src/sshconfig.rs`, 13 unit tests: global +
  `Host` globs/`!`-negation, `IdentityFile` accumulation, `~`/`%d` expansion,
  quoting, dedup) resolves the `IdentityFile`s for the target host; the CLI
  (`SshParams::from_remote`) layers auth as agent → recorded `--identity` →
  `~/.ssh/config` keys → defaults, deduped. Also added `tomo connect --identity
  <path>` (persisted as `[remote] identity` so `tomo watch` reuses it;
  `tomo-config::Remote` gained the optional field). Verified by hand
  (`tomo connect jake@localhost` bootstraps + handshakes via `id_tokyo`) and by
  the ssh-mode suite: 01/02/03/04 all PASS under `TOMO_LINK_MODE=ssh`, and 04
  PASSes in the default run. SPEC §2 documents the auth order. Encrypted
  (passphrase) keys remain out of scope for v0.
