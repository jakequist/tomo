# Tomo (友)

Ridiculously fast two-way file sync between two machines over SSH — with full
file history as a first-class feature. Named after the Japanese word for
"together."

Save a file on your laptop and it's on your Linux box in milliseconds. The
box's build drops an artifact and it flows back. Every save of every file is
versioned, so you can diff or roll back anything, any time. Conflicts never
block sync — a deterministic winner lands, the loser is preserved in history,
and you resolve on your own schedule (or not at all).

**Website & docs: [tomo-sync.dev](https://tomo-sync.dev)** · measured
[benchmarks](https://tomo-sync.dev/docs/benchmarks.html) · honest
[comparison](https://tomo-sync.dev/docs/compared.html) with rsync/Mutagen/Unison/Syncthing

## Quick start

```bash
# Install (static binary, SHA-256-verified; macOS + Linux):
curl -fsSL https://tomo-sync.dev/install.sh | sh
# (or grab a release binary from GitHub Releases and put it on your PATH)

cd ~/my-project
tomo init
tomo sync user@server:~/my-project
```

That one `sync` records the peer, pushes a static binary to the server's
`.tomo/bin/` (nothing to install remotely — it rides your existing SSH), and
starts syncing. On a terminal you get a live TUI: the sync stream with a
status heartbeat, a conflict center one keypress away (`c`), your full file
history browsable in another (`h`). `q` stops, `d` detaches and leaves it
syncing; re-attach from any terminal with `tomo attach`. Scripts, pipes, and
`--json` get a clean line/event stream instead — everything the TUI does has
a CLI equivalent.

## The numbers (20,000-file tree, localhost SSH, release build)

- save → arrival: **6 ms median**
- 100 files changed → all landed: **0.5 s**
- initial seed: **2.8 s** (rsync: 0.75 s; Mutagen: 5.2 s)

Methodology, raw numbers, and the cases where other tools win:
[benchmarks](https://tomo-sync.dev/docs/benchmarks.html).

## What it is / isn't

Two machines, real-time, bidirectional, with history — that's the product.
It is not a mesh (two machines only), not on Windows yet, and doesn't track
symlinks. `node_modules`, virtualenvs, caches, `.git`, and IDE dirs are
ignored by default (overridable); your build outputs sync, because artifacts
flowing back from a remote build is the whole point.

Sessions run detached (`tomo sync -d`), pause without stopping
(`tomo pause`), stream machine-readable events (`tomo events --json`), and
self-update (`tomo update`). Every session serves a local control socket
that the TUI, scripts, and any resident coding agent can drive.

## Shamelessly AI native

This project was built with Claude Code (Fable & Opus). The repo is kept
deliberately agent-friendly — hard invariants in `CLAUDE.md`, an
authoritative `docs/SPEC.md`, 770+ unit/property tests and 32 end-to-end
scenarios that tell an agent immediately whether it broke something. Fork it
and point your own coding agent at it; it should just work. Tomo even briefs
agents working inside a synced tree: `.tomo/README.md` explains why files
change on their own and how to recover anything from history.

## Building from source

```bash
cargo build --release          # Rust 1.97+; no OpenSSL, no C deps beyond
                               # bundled SQLite/zstd
cargo test --workspace         # unit + property tests
./scenarios/run-all.sh         # end-to-end suite (self-SSH, see scenarios/)
```

Linux release binaries are static musl builds; macOS is native
(Apple silicon + Intel). See `docs/SPEC.md` for the design record and
`docs/TESTING.md` for the acceptance-test map.

## License

MIT.
