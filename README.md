# Tomo (友)

Real-time, two-way file sync between two machines over SSH — with full file
history, zero-friction remote bootstrap, and vector-clock conflict handling.
Named after the Japanese word for "together."

Edit source on your Mac; it appears on your Linux GPU server in milliseconds.
The server's build drops an artifact; it flows back. Every change is versioned.
Conflicts never block — they're recorded, surfaced, and recoverable.

**Status: v0 feature-complete on Linux.** All seven roadmap milestones are
implemented and every acceptance scenario (14 end-to-end tests, including
kill-9 crash recovery, offline queueing, 1 GiB transfers under churn, and
clock-skew immunity) passes. The macOS half of the release matrix awaits Mac
hardware; the watcher backend (FSEvents via `notify`) is already in place.

## Quick start

```bash
# On your laptop, inside the project you want to sync:
tomo init
tomo connect user@server /path/to/project   # pushes a static binary to the
                                            # server's .tomo/bin and validates
tomo watch                                  # foreground sync loop
```

That's it. No install on the server, no daemon, no root — Tomo pushes a
statically linked binary over SFTP (SHA-256 verified) and runs it over the
same SSH connection. All state lives in the project's `.tomo/` directory.

## Everyday commands

| Command | What it does |
|---|---|
| `tomo status [--json]` | Sync state: index root, counters, conflict badge |
| `tomo log [<path>] [--json]` | Version history — per file, or repo-wide recent activity |
| `tomo diff <path> [--version N] [--against M]` | Diff working tree vs history, or two versions |
| `tomo restore <path> [--version N] [--stdout]` | Undo to a previous version (default: previous save) |
| `tomo conflicts list / show <id> / resolve <id>` | Inspect and settle recorded conflicts |
| `tomo db check` | History-store integrity check |
| `tomo completions <shell>` | Shell completion scripts |

## How it works (the short version)

- **Pure sync engine.** `(index, event) → (index′, actions)` — no I/O, no
  clocks, no threads in the core crate; it's exhaustively property-tested.
- **Vector clocks, never wall time.** Index entries are multi-value registers
  (sets of concurrent causal heads); absorbing a version is a lattice join,
  so replicas converge under any delivery order. Survives a peer whose clock
  is three years wrong.
- **Conflicts never block.** Both sides independently materialize the same
  deterministic winner (edit beats delete, then content hash); the loser is
  always preserved in history and surfaced non-blockingly.
- **Content-addressed history.** FastCDC chunking + BLAKE3 + zstd in a single
  SQLite file. A one-character edit to a 10 MiB file stores ~1% new bytes.
  Adaptive capture versions every save under light load and coalesces
  checkpoint-style under storms — sync latency is never sacrificed.
- **Crash-safe by construction.** Staging + atomic rename everywhere;
  `kill -9` at any instant leaves no partial file at any final path.
- **Large files don't starve small ones.** Big content ships as
  content-defined chunks interleaved with live changes (measured: <7 s
  small-file latency during a 1 GiB transfer).

Full design: [`docs/SPEC.md`](docs/SPEC.md) (authoritative), with
[`docs/TESTING.md`](docs/TESTING.md) (acceptance roadmap),
[`docs/ROADMAP.md`](docs/ROADMAP.md) (milestones),
[`docs/RELEASING.md`](docs/RELEASING.md) (release matrix), and
[`docs/NOTES.md`](docs/NOTES.md) (build journal / backlog).

## Development

```bash
cargo build
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace              # 340 tests incl. proptest suites
./scenarios/run-all.sh --quick      # Tier-1 e2e (local + ssh link modes)
./scenarios/run-all.sh              # all 14 scenarios (~4 min)
```

The `scenarios/` directory contains the executable acceptance suite: two
"machines" on one box, real SSH to localhost, netem lag variants, faketime
clock skew, storm stress. See [`scenarios/README.md`](scenarios/README.md).

Releases: `scripts/release.sh` builds static musl artifacts (x86_64 +
aarch64 via zig) and a fat binary with embedded bootstrap payloads.

License: [MIT](LICENSE)
