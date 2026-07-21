# macOS Handoff — instructions for the Claude session on the Mac mini

You are picking up Tomo (this repo) on macOS. The product is **feature-complete
v0 on Linux**: all 7 milestones done, 14/14 e2e scenarios green, 340 tests,
release pipeline building static Linux binaries. Your mission is the darwin
half. Read `CLAUDE.md` first (invariants are non-negotiable), then
`docs/SPEC.md`; consult `.claude/skills/*` before touching Rust or scenarios.
`docs/NOTES.md` is the shared build journal — log findings there.

## Your mission, in order

1. **Make the workspace build and test green on macOS.**
   `cargo build && cargo test --workspace` plus fmt/clippy per CLAUDE.md.
   Expect little friction in Rust code (no OpenSSL, no aws-lc — russh uses
   `ring`; SQLite/zstd bundle via cc — you need Xcode Command Line Tools).
2. **Validate the FSEvents watcher for real.** The `notify` crate supplies the
   FSEvents backend; our platform surface is `crates/tomo-watch/src/watcher.rs
   ::map_event` (a pure function, unit-tested — extend those tests with the
   event kinds FSEvents actually emits). Known risk areas, all found the hard
   way on Linux and likely to differ on FSEvents:
   - Coalesced/imprecise events (`EventKind::Any`, `Modify(Name(Any))`) map to
     "re-stat" Dirties and self-correct — verify that holds.
   - A directory APPEARING triggers a reconciling rescan (watch-establishment
     race). FSEvents is directory-granular; verify new-dir + immediate-file
     creation never loses the file (scenario 02's nested-dir burst).
   - Editor atomic saves (vim temp+rename, VSCode) must land as one coherent
     version — scenario 03 is the acceptance test.
3. **Get the scenario harness running on macOS.** This is where the real work
   likely is — the harness is bash written against GNU userland:
   - `date +%s%N` (nanoseconds) in `wait_for` does not work with BSD date →
     `brew install coreutils` and use `gdate`, or make the helper portable.
   - Check `stat`, `sed -i`, `sha256sum` (vs `shasum -a 256`), `pgrep` flag
     compatibility as you go.
   - `tc netem` does not exist on macOS: lag variants must **skip** cleanly
     (the harness already skips when tc is unavailable — verify).
   - Scenario 13 uses `faketime`; libfaketime on macOS fights SIP — skip it
     (exit 77 with the reason) rather than fighting dyld.
   - Self-SSH: enable Remote Login (System Settings → Sharing) so
     `ssh localhost` works for ssh-mode scenarios.
   Target: scenarios 01–03, 05–08, 10, 12, 14, 15 green in local mode on macOS;
   ssh mode for 01–04 once self-SSH works. 09/11 are heavy but should work;
   13 may skip. (15 is the single-session lock — `fd-lock` uses `flock`, which
   behaves identically on macOS, so it should pass in local mode.)
4. **Real cross-platform sync test.** Jake can give you SSH access to the
   Linux dev VM. `tomo sync user@<linux-host>:/path` from the Mac (one command:
   it records the peer and starts syncing) must bootstrap (dev-mode builds push
   their own binary — for Mac→Linux you need
   either a release fat binary or the linux thin artifact; coordinate with a
   release build, or test Linux→Mac where the Linux fat binary embeds darwin
   artifacts once CI produces them). Verify two-way sync, history, conflicts.
5. **Turn the darwin CI jobs green.** `.github/workflows/release.yml` has
   `thin-darwin` and `fat-darwin` jobs (macos-14 runners) that have never run
   against darwin-compiling code. Trigger via `workflow_dispatch` (a dry run
   builds everything but publishes nothing): `gh workflow run release.yml`.
   Iterate until green. When they pass, the first tagged release (`git tag
   v0.1.0 && git push --tags`) publishes the complete four-triple matrix
   automatically.

## Working conventions (same as the Linux build)

- **`main` is branch-protected — direct pushes are rejected; land
  everything via PR.** Work on branch
  `darwin-support` (or smaller topic branches off it), push, `gh pr create
  --fill`, wait for the required CI checks (lint-and-test, scenarios,
  musl-static, cargo-deny, aarch64-musl-cross) to pass, then
  `gh pr merge --merge`. No review approvals are required — CI green is the
  gate. The Linux-side scenarios job runs on ubuntu runners, so your macOS
  work must keep Linux green too (it will unless you touch shared code).
- Small imperative-mood commits referencing
  the test/scenario that motivated them; the full quality gate before any
  hand-off: `cargo fmt --all -- --check && cargo clippy --workspace
  --all-targets -- -D warnings && cargo test --workspace && ./scenarios/run-all.sh`.
- Never weaken a Linux-passing test to make macOS pass — make behavior
  converge or gate on platform explicitly with a comment saying why.
- The 9 invariants in CLAUDE.md override everything. In particular: no wall
  clocks for ordering, `.tomo/**` never syncs, staging + atomic rename for
  every write (verify rename atomicity assumptions hold on APFS — they do,
  but scenario 03/09 will prove it).
- Log every macOS-specific finding in `docs/NOTES.md`; pull `main` often —
  the Linux-side session may still be landing fixes.

## Environment quick-start on the Mac

```bash
xcode-select --install                # toolchain for cc-built deps
curl https://sh.rustup.rs -sSf | sh   # rustup; then:
rustup target add x86_64-apple-darwin # (arm64 native is default on M-series)
brew install coreutils jq             # harness needs gdate + jq
git clone git@github.com:jakequist/tomo.git && cd tomo
cargo build && cargo test --workspace
```

Good luck — the Linux side is stable ground; everything you find that differs
is by definition interesting. 友

## Filename semantics validation (macOS↔Linux, Tier-1 edge case 3)

The `filename-semantics` branch shipped the platform-generic machinery for the
two APFS filename hazards, built and tested entirely on Linux (which has neither
behavior). **APFS is the whole point of this work and it can only be validated
on your Mac.** Everything below is a Linux-side implementation that *assumes* a
runtime probe correctly detects APFS's behavior — your job is to prove that
assumption on real hardware.

### What was built (all in this repo now)

- **FS probe at session startup** — `crates/tomo/src/fsprobe.rs`. Creates probe
  files under `.tomo/state/` and observes them to fill
  `FsSemantics { case_insensitive, normalizes_unicode }`. Pure interpretation
  (`interpret_case`, `interpret_norm`) is unit-tested; the I/O shim
  (`probe`/`probe_case`/`probe_norm`) is what needs *real-APFS* validation. The
  result is recorded (additively) in `status.json` as the `fs` block and drives
  the two guards below.
- **NFC canonicalization on ingest** — `crates/tomo-watch/src/norm.rs`, wired
  into the watcher's and the scanner's `relativize` (via a `normalize_unicode`
  flag threaded through `Canonicalizer::new`, `Watcher::start`, `scan_diff`).
  Applied **only** when the probe reports `normalizes_unicode`, so Linux stays
  byte-faithful (a Linux user's genuinely-NFD names are preserved — scenario 20
  phase A/B asserts this).
- **Case-collision ingress guard** — `crates/tomo/src/fsguard.rs` (pure detector)
  + `Session::case_collision_refused` in `session.rs`. On a case-insensitive FS,
  an inbound apply for `P` that case-folds onto a different existing `Q` is
  refused (keeps `Q`), the incoming bytes are preserved to history, a `⚠ case
  collision:` note is emitted, and it counts as a conflict — sync never blocks.
- **Debug test hook** — `TOMO_TEST_FORCE_FS` (`cfg(debug_assertions)` only)
  forces the probe result (`case-insensitive`, `normalizing`, or both) so the
  guards can be exercised on a Linux VM. Release builds ignore it entirely.

### What YOU must verify on real APFS (none of this can run on Linux)

1. **Probe detects APFS correctly.** On a default (case-insensitive) APFS
   volume, `tomo status --json | jq .fs` must show
   `{"case_insensitive": true, "normalizes_unicode": true}`. Also test a
   **case-sensitive APFS** volume (create one with Disk Utility, or
   `hdiutil create -fs 'Case-sensitive APFS'`) → `case_insensitive: false`.
   Confirm the probe leaves **no** `.tomo-fsprobe-*` residue under
   `.tomo/state/` after startup.
2. **NFD readdir round-trip.** Create a file on the Mac with a precomposed (NFC)
   name — e.g. `café.txt` (U+00E9) — inside a Tomo project, and confirm:
   (a) the watcher/scan derive the **NFC** `RelPath` (so it matches a Linux
   peer's NFC original — no duplicate-file ping-pong), and (b) that NFC
   `RelPath`, when joined and *read back*, opens the file even though APFS stored
   the name as NFD. (This "NFC lookup finds the NFD file" property is exactly
   what the Linux VM cannot test — `crates/tomo-watch/src/scan.rs`'s
   `relativize_normalizes_nfd_to_nfc_only_when_flag_set` test only covers the
   name derivation, not the round-trip read.)
3. **NFC/NFD ping-pong is gone, end to end.** Sync a Mac↔Linux pair. On Linux
   create two files whose names are the NFC and NFD encodings of the same string
   (distinct on Linux). Confirm the Mac does not enter an endless
   create/delete ping-pong, and that a single NFC name authored on Linux arrives
   on the Mac as an openable file. (Scenario 20 phase A/B proves Linux keeps them
   distinct; the Mac side of this is yours.)
4. **Case-collision guard fires for real.** On the Mac (real case-insensitive
   APFS, **no** `TOMO_TEST_FORCE_FS`), have the Linux peer hold both `Foo.txt`
   and `foo.txt` with different bytes and sync. Confirm the Mac keeps the first,
   refuses the second (no silent overwrite of `Foo.txt`), logs the `⚠ case
   collision:` note, preserves the refused bytes (`tomo log foo.txt` recovers
   them), counts a conflict, and stays connected. This is scenario 20 phase C
   without the debug hook — the real thing.
5. **Linux→Mac NFC arrival.** A plain accented filename authored on Linux (NFC)
   must sync to the Mac and be openable in Finder/editors by its expected name.

Log every APFS finding in `docs/NOTES.md`. If the probe misfires on any real
volume (network/exFAT/case-sensitive APFS), fix `interpret_*`/`probe_*` in
`fsprobe.rs` — the pure interpreters are unit-tested, so add the failing
observation as a case there first.
