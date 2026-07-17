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
   Target: scenarios 01–03, 05–08, 10, 12, 14 green in local mode on macOS;
   ssh mode for 01–04 once self-SSH works. 09/11 are heavy but should work;
   13 may skip.
4. **Real cross-platform sync test.** Jake can give you SSH access to the
   Linux dev VM. `tomo connect user@<linux-host> /path` from the Mac must
   bootstrap (dev-mode builds push their own binary — for Mac→Linux you need
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
