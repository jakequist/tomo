---
name: cross-release
description: Tomo's cross-compilation matrix, static musl builds, binary embedding for the SSH bootstrap, and the release checklist. Consult this skill when touching anything related to builds, releases, target triples, the remote bootstrap / binary push, glibc or linking errors, "GLIBC_x.y not found", build.rs, feature flags for embedding, or CI release jobs.
---

# Cross-compilation & Release for Tomo

## The matrix (docs/SPEC.md §3)

Release builds produce all of:

| Triple | Notes |
|---|---|
| `x86_64-unknown-linux-musl` | fully static; the workhorse server target |
| `aarch64-unknown-linux-musl` | fully static; ARM servers |
| `x86_64-apple-darwin` | needs macOS builder/CI runner |
| `aarch64-apple-darwin` | needs macOS builder/CI runner |

On this Linux VM you can build and test both musl targets
(`rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl`;
install `musl-tools`, and for aarch64 either `cross` or a
`gcc-aarch64-linux-gnu` linker with musl config). Darwin targets cannot be
built here — wire them into CI (macOS runners) and **flag for the human**
rather than faking it.

## Why musl, and what it forces

Static musl kills glibc-version roulette on old servers ("GLIBC_2.34 not
found"). Consequences, non-negotiable:

- `rusqlite` with `bundled` feature (SQLite compiled in, never dynamically
  linked).
- rustls everywhere; `openssl`/`openssl-sys` are banned in `deny.toml`.
- musl's allocator is slow → enable `mimalloc` as global allocator for musl
  release builds (feature-gated or `cfg(target_env = "musl")`).
- Verify staticness: `ldd target/x86_64-unknown-linux-musl/release/tomo`
  should report "not a dynamic executable"; `file` should say statically
  linked.

## Binary embedding for bootstrap

The release binary embeds sibling binaries for all supported triples
(`include_bytes!` of release artifacts) so `tomo connect` can push the right
one over SFTP with zero external downloads. Requirements:

- **Feature-gated** (e.g. `--features embed-binaries`): dev builds must NOT
  embed (keeps the edit-compile loop fast). The bootstrap code path, when
  binaries aren't embedded (dev), should fall back to looking for sibling
  target-dir artifacts so scenarios can still exercise the push.
- Embed a manifest (triple → SHA-256) alongside; the push verifies SHA-256
  after copy, and version handshake is the first protocol exchange.
- Naming: `.tomo/bin/tomo-<version>-<triple>` on the remote. Exact version
  match or re-push — no ranges, no "close enough".
- Unsupported remote triple (`uname -s`/`-m` mapping fails) → clean, explicit
  error. Never attempt a download.

## Release checklist

1. Full test loop green: fmt, clippy `-D warnings`, `cargo test --workspace`,
   `./scenarios/run-all.sh` including `--lag` variants.
2. `cargo deny check` clean (licenses MIT-compatible, no banned crates).
3. Build the matrix; verify staticness of both musl binaries; record SHA-256s.
4. Build the fat host binaries with `embed-binaries`; run scenario 04
   (bootstrap matrix) against the fat binary specifically.
5. Expect ~40–60 MB fat binary — fine for a dev tool; investigate only if it
   balloons past that.
6. Tag `v<version>`; version string must match exactly what the handshake
   reports (scenario 04 asserts re-push on any mismatch).
