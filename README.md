# Tomo (友)

Real-time, two-way file sync between two machines over SSH — with full file
history, zero-friction remote bootstrap, and vector-clock conflict handling.
Named after the Japanese word for "together."

Edit source on your Mac; it appears on your Linux GPU server in milliseconds.
The server's build drops an artifact; it flows back. Every change is versioned.
Conflicts never block — they're recorded, surfaced, and recoverable.

**Status: pre-alpha scaffold.** Start here:

- [`docs/SPEC.md`](docs/SPEC.md) — the design (authoritative)
- [`docs/TESTING.md`](docs/TESTING.md) — TDD strategy + acceptance roadmap
- [`docs/ROADMAP.md`](docs/ROADMAP.md) — milestones
- [`scenarios/`](scenarios/) — executable end-to-end acceptance scenarios
- [`CLAUDE.md`](CLAUDE.md) — instructions for AI-assisted development

## Development

```bash
cargo build
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
./scenarios/run-all.sh --quick
```

License: [MIT](LICENSE)
