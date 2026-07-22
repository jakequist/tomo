# CLAUDE.md — Tomo

Tomo (友, "together") is a real-time, two-way file sync tool between two machines
(typically a Mac laptop and a Linux server), written in Rust, with full file
history as a first-class feature. Long-term ambition: grow into a git
alternative. Read `docs/SPEC.md` before writing any code — it contains every
design decision already made and is authoritative. `docs/TESTING.md` defines the
acceptance-test roadmap. `docs/ROADMAP.md` orders the milestones.

## Your environment

You are working inside a sandboxed Linux VM. It is impossible to damage
anything. Install packages freely (`apt`, `cargo install`, etc.), configure
`sshd`, add SSH keys, run `tc netem`, kill processes with `-9` — whatever the
task needs. Do not ask for permission for environment setup; just do it.

End-to-end testing is done by **SSHing into this same machine** (localhost) and
syncing between two temp directories, simulating the Mac↔Linux pair. The
`scenarios/` directory contains the harness and per-scenario scripts. Consult
the `scenario-testing` skill before touching them.

## Non-negotiable invariants

These come from the design discussion and must never be violated. If a change
would violate one, stop and rethink.

1. **`.tomo/**` is hardcoded-ignored** at the lowest layer of the event
   pipeline. Not a default config entry — a constant. Tomo must never sync,
   watch, or version its own state directory.
2. **All state lives in `<project_root>/.tomo/`.** No global state. No writes to
   `$HOME`, `/etc`, XDG dirs, or anywhere else. (The remote side's binary also
   lives in the remote project's `.tomo/bin/`.)
3. **Sync latency is never sacrificed for history.** Adaptive debouncing applies
   only to history capture; the live sync path always ships the latest bytes
   immediately.
4. **The final state of every burst is always versioned.** Debouncing may drop
   intermediate versions, never the last one.
5. **Conflicts never block sync.** Last-writer-wins with a deterministic
   tiebreaker (vector clocks decide causality; when concurrent, compare content
   hash, then replica ID). Both sides must converge to the identical winner
   without negotiation. The loser is always preserved in history and surfaced
   non-blockingly in the CLI.
6. **The sync engine core is a pure state machine.** `(index, event) → (index',
   actions)`. No I/O, no clocks, no threads in `tomo-engine`. All I/O lives in
   adapter crates. This is what makes real TDD possible — do not erode it.
7. **Never trust wall clocks for ordering.** Vector clocks order everything they
   can. Wall time may be recorded for human display, never for decisions — with
   one narrow, provably-safe carve-out: at **genesis** (the first sync between
   two pre-existing trees), a path's concurrent heads have disjoint replica
   support, so their vector clocks contain *zero* information to order them. Only
   there does the winner fall back to wall-clock mtime (adopt the newer copy,
   docs/SPEC.md §5.3). The moment replicas share any causal history the clocks
   decide again and mtime is never consulted — the carve-out is genesis-only by
   construction, and the mode is a pure function of the head set so both replicas
   still converge without negotiation. mtime is never used to order anything a
   clock can.
8. **Crash safety via staging + atomic rename.** A partially transferred file
   must never be visible at its final path. `kill -9` at any moment must not
   corrupt the tree or the history DB.
9. **Linux release binaries are static musl builds.** No glibc, no OpenSSL
   (use rustls), SQLite via `rusqlite` `bundled` feature. The copy-and-exec
   bootstrap depends on this.

## Development loop (TDD is mandatory)

This project is built test-first, at two levels:

- **Unit/property level**: write the failing test before the implementation.
  `tomo-engine` and the history pressure controller are pure functions — cover
  them with `proptest` property tests and simulated event streams with
  deterministic time. Simulated event storms, not sleeps.
- **System level**: every feature milestone has acceptance tests in
  `docs/TESTING.md` and a corresponding executable scenario in `scenarios/`.
  A milestone is not done until its scenario passes.

After **every major change** (new feature, refactor, bug fix — anything beyond
a comment tweak), run, in order:

```bash
cargo fmt --all -- --check      # formatting (fix with: cargo fmt --all)
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace          # unit + integration tests
./scenarios/run-all.sh --quick  # fast scenario subset (when scenarios exist)
```

Before declaring any milestone complete, run the full `./scenarios/run-all.sh`.
Never commit or hand off with failing tests, clippy warnings, or formatting
drift. If a test is flaky, fix the flakiness — do not retry-until-green, do not
add sleeps. Scenario scripts poll with timeouts (see the harness); production
tests must be deterministic.

Consult the `rust-hygiene` skill for the full coding standards. The short
version:

- `unsafe_code = "forbid"` workspace-wide.
- No `.unwrap()` / `.expect()` in library code paths (tests are fine). Errors
  are `thiserror` enums per crate; the CLI renders them with context.
- Clippy `pedantic` is on as warnings; do not blanket-`allow` — justify each
  `#[allow]` with a comment on the line above it.
- Public items get doc comments. Doc examples must compile (`cargo test`
  runs them).
- Keep dependencies minimal and vetted; every new dependency is a decision —
  record notable ones in `docs/SPEC.md` §Dependencies with one line of
  rationale. Check licenses are MIT-compatible (`cargo deny check` once
  configured).
- **`main` is PR-gated**: do not push to it directly. Land changes via a
  feature branch and PR (`gh pr create`), wait for the CI checks
  (lint-and-test, scenarios, musl-static, cargo-deny, aarch64-musl-cross)
  to go green, then `gh pr merge --merge`. Enforcement is server-side (repository
  ruleset "protect-main"): direct pushes to main are rejected.
- Small commits, imperative-mood messages, reference the scenario/test that
  motivated the change.

## Workspace layout

| Crate | Role | I/O allowed? |
|---|---|---|
| `crates/tomo` | CLI binary (`clap`), all user-facing output | yes |
| `crates/tomo-engine` | Pure sync state machine, vector clocks, conflict resolution, pressure controller | **no** |
| `crates/tomo-history` | Content-addressed store (FastCDC chunking, BLAKE3, zstd) + SQLite metadata | yes |
| `crates/tomo-proto` | Wire protocol: framing, message types, serialization | no |
| `crates/tomo-transport` | SSH session, SFTP bootstrap (binary push), tunneled stdio channel | yes |
| `crates/tomo-watch` | FSEvents/inotify adapters → canonical change records, echo suppression | yes |
| `crates/tomo-config` | `.tomo/config.toml` parsing, ignore rules, path classes | read-only |

Dependency direction: `tomo` → everything; adapters (`watch`, `transport`,
`history`) → `engine`/`proto`/`config`; `engine` depends on nothing but std
(+ serde). Never let `engine` grow an I/O dependency.

## CLI surface (initial)

`tomo init`, `tomo sync [<ssh-target> <remote-path>] [--local-peer <path>]`
(the primary foreground sync loop — records the peer on first use and subsumes
the old connect-then-watch two-step; `tomo watch` remains as a hidden deprecated
alias), `tomo connect <ssh-target> <remote-path>` (record + validate a peer
without starting a session), `tomo status`, `tomo log <path>`,
`tomo restore <path> [--version <id>]`, `tomo conflicts [list|show|resolve]`
(`conflicts show <id-or-path> [--json]` renders the winner-vs-loser diff;
`conflicts resolve <id-or-path> --keep-current|--take-loser|--both`, plus
`--all` mass-ack and `--interactive` prompt loop — an id-or-path argument
resolves that path's newest unresolved conflict; `--both` writes a `<path>.theirs`
sidecar), `tomo events [--json]` (stream the running session's control-channel
event feed; docs/SPEC.md §13). Session lifecycle (UX-V2 §1, docs/SPEC.md §13.4):
`tomo sync -d|--detach` starts the session in the background and returns (prints
the pid + how to attach; the flock still refuses a second); `tomo attach
[--plain|--json]` joins the running session and streams its live view (Ctrl-C
detaches, never stops the session); `tomo stop` cleanly stops it (idempotent);
`tomo logs [-f] [-n N]` tails `.tomo/logs/session.log`. Machine-readable `--json`
output on status/log/conflicts/events/attach from day one — the scenarios depend
on it for assertions. Only one sync/serve session runs per project at a time (a
`.tomo/state/session.lock` flock; a second is refused); each session also
serves a control socket at `.tomo/state/ctl.sock` (event stream + command
channel).

## Skills

Project skills live in `.claude/skills/`. Use them:

- `rust-hygiene` — coding standards, lint policy, error handling, dependency
  policy. Consult before writing or reviewing Rust.
- `scenario-testing` — how to set up localhost SSH, run the e2e harness, add
  scenarios, inject network latency with `tc netem`. Consult before running or
  writing anything in `scenarios/`.
- `cross-release` — musl/darwin cross-compilation matrix, binary embedding for
  the bootstrap, release checklist. Consult when touching build/release/bootstrap.

## Things that will tempt you; resist them

- Adding a sleep to fix a race → find the real ordering bug.
- Testing the watcher against the real filesystem in unit tests → simulate
  events; real FS belongs in `scenarios/` only.
- Letting the CLI print from deep in a library crate → libraries return data,
  `tomo` renders it.
- Reaching for wall-clock timestamps to order events → vector clocks.
- Weakening invariant #1–#9 "temporarily" → no.
