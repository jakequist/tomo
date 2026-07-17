---
name: rust-hygiene
description: Tomo's Rust coding standards, lint policy, error handling, testing discipline, and dependency policy. Consult this skill before writing, modifying, or reviewing ANY Rust code in this repository — including "quick fixes", refactors, new crates, adding dependencies, or resolving clippy warnings. Also consult it when tests fail, when tempted to add an #[allow], or when deciding where code belongs in the workspace.
---

# Rust Hygiene for Tomo

## The loop (after every major change)

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
./scenarios/run-all.sh --quick   # once scenarios are implementable
```

Never hand off with any of these red. Full `run-all.sh` before declaring a
milestone done.

## TDD, concretely

1. Write the failing test first (unit or property). For engine work, that
   means simulated event streams and deterministic time — if you're reaching
   for `std::time::Instant`, `sleep`, threads, or `tempfile` inside
   `tomo-engine`, you're violating the purity contract; stop.
2. Make it pass minimally. 3. Refactor with tests green.
- Property tests use `proptest`. Every conflict/clock/pressure behavior in
  `docs/TESTING.md` Level 1 needs a property, not just examples.
- Flaky test → fix the flakiness, never retry-until-green, never add sleeps.
- Doc examples compile (they run under `cargo test`); write them for public
  APIs.

## Errors

- Library crates: `thiserror` enum per crate; variants carry context (path,
  peer, operation). No `.unwrap()`/`.expect()` in library code — the
  workspace lints deny them. Tests may `#[allow(clippy::unwrap_used, clippy::expect_used)]` at module scope.
- Only `crates/tomo` renders errors to humans; libraries never print. No
  `println!`/`eprintln!` outside the CLI crate (use `tracing` if/when
  observability lands).
- Panics are reserved for provable invariant violations, with a comment
  proving it.

## Lints

- `unsafe_code = "forbid"` — no exceptions in this project.
- Clippy `all=deny`, `pedantic=warn`. Do not blanket-allow pedantic lints:
  each `#[allow]` sits on the specific line with a one-line justification
  comment above it. If the same allow appears three times, discuss promoting
  it to the workspace list in the root `Cargo.toml` instead.
- `missing_docs = warn`: public items get doc comments that say *why*, not
  just what.

## Dependencies

- Every new dependency is a decision. Before adding: check it's maintained,
  widely used, MIT-compatible (`cargo deny check` — config in `deny.toml`),
  and that std doesn't already suffice.
- Add via `[workspace.dependencies]` in the root, reference with
  `dep.workspace = true`, and record one line of rationale in
  `docs/SPEC.md` §11.
- Hard rules from the musl static-build requirement: **never OpenSSL**
  (rustls only), `rusqlite` must use the `bundled` feature, prefer pure-Rust
  crates over C bindings when reasonable.

## Structure

- Respect the dependency direction in CLAUDE.md's crate table. `tomo-engine`
  must stay I/O-free; if a change needs I/O, it belongs in an adapter crate
  and the engine grows a new event/action variant instead.
- Small commits, imperative mood ("Add echo-suppression journal"), reference
  the test/scenario that motivated the change.
- Keep functions small; prefer exhaustive `match` over `if let` chains on
  engine events so new variants force compile errors at every decision point.
