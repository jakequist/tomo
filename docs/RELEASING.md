# Releasing Tomo

The release engineering for Tomo (M6). Read alongside the `cross-release` skill
(`.claude/skills/cross-release/SKILL.md`), which is authoritative, and SPEC §3
(remote bootstrap) and §11 (dependencies).

## The v0 matrix

| Triple | Built where | Static | Notes |
|---|---|---|---|
| `x86_64-unknown-linux-musl` | this VM / Linux CI | yes | the workhorse server target |
| `aarch64-unknown-linux-musl` | this VM / Linux CI (via zig) | yes | ARM servers |
| `x86_64-apple-darwin` | **macOS runner only** | n/a | not built on Linux |
| `aarch64-apple-darwin` | **macOS runner only** | n/a | not built on Linux |

Linux binaries are fully static musl builds (no glibc-version roulette on old
servers). Consequences, enforced elsewhere: `rusqlite` `bundled`, rustls (never
OpenSSL, banned in `deny.toml`), and `mimalloc` as the global allocator for
`cfg(target_env = "musl")` only.

## Versioning

- One source of truth: `[workspace.package].version` in the root `Cargo.toml`.
  `scripts/release.sh` reads it; the bootstrap binary name
  (`tomo-<version>-<triple>`) and the `Hello` handshake both derive from
  `CARGO_PKG_VERSION` via `crates/tomo/src/buildinfo.rs`.
- The version string must match **exactly** what the handshake reports —
  bootstrap does exact-match-or-re-push, no ranges (SPEC §3; scenario 04
  asserts re-push on any mismatch).
- Tag `v<version>`; the `release-artifacts` CI job runs only on `refs/tags/v*`.

## Binary embedding (the fat binary)

The SSH bootstrap pushes the right binary for the remote's triple with **zero
downloads** by embedding the release artifacts into the host binary.

- Feature-gated: `tomo-transport`'s `embed-binaries` feature (surfaced as
  `tomo`'s `embed-binaries` feature). **Off by default** so the dev
  edit-compile loop stays fast and dev builds carry no 40 MB payload.
- `crates/tomo-transport/build.rs` scans `$TOMO_EMBED_DIR` at build time for
  files named `tomo-<version>-<triple>` (any of the four v0 triples) and
  generates an `include_bytes!` table. Only files that exist are embedded, so
  the mechanism is testable with tiny stub files (see the `binsource` unit
  tests and the `embed-binaries` test job).
- Resolution order at bootstrap (`binsource::binary_for_triple`):
  1. **Embedded** exact triple **and** version match — served first.
  2. In a fat build, a triple we did not embed → clean
     `UnsupportedTarget` listing what *is* embedded. Never a download.
  3. Only a **non-embedded dev build** falls back to the debug-only
     `current_exe` substitution (gnu-serves-musl on localhost), which the CLI
     warns about loudly.
- Inspect a fat binary's payload: `tomo dev embedded-binaries [--json]`
  (hidden diagnostic command).

## Building the release (Linux half)

```bash
# One command assembles everything and self-verifies:
./scripts/release.sh
```

It is idempotent (rebuilds `dist/` each run) and fails loudly. It:

1. Builds a static musl release binary for each available Linux triple.
2. Copies them to `dist/tomo-<version>-<triple>` and writes `dist/SHA256SUMS`.
3. Rebuilds the x86_64 host binary with `--features embed-binaries` and
   `TOMO_EMBED_DIR=dist`, embedding the step-2 artifacts, and copies it to
   `dist/tomo-<version>-x86_64-unknown-linux-musl.fat`.
4. Runs `tomo dev embedded-binaries --json` against the fat binary and asserts
   every built triple (always x86_64-musl) is embedded.

Expect a fat binary in the ~25–60 MB range (two Linux triples ≈ 26 MB here;
larger once darwin artifacts are added on a mac). Investigate only if it
balloons well past that.

## aarch64-musl on this VM — the zig requirement

`aarch64-unknown-linux-musl` **does** build and link statically on this VM, but
**only via `cargo-zigbuild` + `zig`**, not with Debian's GNU cross toolchain.

- With `gcc-aarch64-linux-gnu` as the C compiler/linker, every C dependency
  (SQLite, zstd, mimalloc, aws-lc-sys) *compiles*, but the final link fails:

  ```
  undefined reference to `open64' / `stat64' / `fstat64' / `mmap64' / ...
  ```

  Cause: the glibc cross gcc compiles SQLite's C against glibc headers, which
  map `open`→`open64` etc. for large-file support. musl has no `*64` aliases
  (its `off_t` is always 64-bit), so those symbols are unresolved. This is a
  toolchain mismatch, not a code bug — Debian ships no `aarch64-linux-musl-gcc`.
- Fix: `zig cc` provides a proper musl cross toolchain. `scripts/release.sh`
  auto-detects `cargo-zigbuild` + `zig` and uses `cargo zigbuild` for the musl
  targets; without them it builds x86_64-musl only and skips aarch64 with a
  notice. CI installs zig (`mlugg/setup-zig`) and `cargo-zigbuild`
  (`taiki-e/install-action`) for the `aarch64-musl-cross` and
  `release-artifacts` jobs.
- Local setup used here: download the zig 0.13 tarball from ziglang.org, put
  `zig` on `PATH`, `cargo install cargo-zigbuild --locked`,
  `rustup target add aarch64-unknown-linux-musl`.

There is **no** `.cargo/config.toml` linker override: the default linker already
produces a static-PIE x86_64-musl binary, and zigbuild supplies its own linker
for aarch64, so an explicit (and, for aarch64-gnu, broken) override would only
get in the way.

## Darwin gap (flag for the human)

Darwin artifacts are **not** produced on this Linux VM and are not faked. They
require a macOS runner/builder. When one is available:

- Add `x86_64-apple-darwin` and `aarch64-apple-darwin` release builds on the
  mac, drop the artifacts into `dist/` next to the Linux ones, and the same
  `build.rs`/embedding path picks them up (the four triples are already wired
  through `triple.rs`, `binsource`, and `build.rs`).
- The `notify` crate already provides the FSEvents backend automatically when
  compiled for darwin — no `tomo-watch` code change is needed for the macOS
  file-watching adapter; it comes from `notify` for free.

## `cargo deny`

`cargo deny check` must be clean (`cargo install cargo-deny --locked`; CI runs
it via `EmbarkStudios/cargo-deny-action`). Config: `deny.toml`.

- Licenses: MIT-compatible allowlist.
- Bans: `openssl`/`openssl-sys` denied (rustls-only, musl static policy).
- Advisories currently ignored, each with a justification comment in
  `deny.toml`:
  - `RUSTSEC-2023-0071` (rsa Marvin attack) — no upstream fix; transitive via
    russh SSH keys; not remotely exploitable in our client use.
  - `RUSTSEC-2023-0089` (atomic-polyfill unmaintained) — informational only.
- `RUSTSEC-2026-0153` / `RUSTSEC-2026-0154` (russh / russh-cryptovec remote-DoS)
  were resolved by upgrading russh to `>=0.62` and are no longer ignored. That
  upgrade also switched the crypto backend to `ring` (`default-features = false`)
  for smaller, C-free static musl builds.

## Release checklist

1. Full test loop green: `cargo fmt --all -- --check`,
   `cargo clippy --workspace --all-targets -- -D warnings` (and the same with
   `--features tomo/embed-binaries`), `cargo test --workspace` (and
   `-p tomo-transport --features embed-binaries`),
   `./scenarios/run-all.sh` including `--lag` variants.
2. `cargo deny check` clean.
3. `cargo build --workspace --release --target x86_64-unknown-linux-musl` and
   confirm `file …/release/tomo` says statically linked; same for aarch64 via
   `cargo zigbuild`. Record the SHA-256s (`dist/SHA256SUMS`).
4. `./scripts/release.sh`; confirm the fat binary's
   `tomo dev embedded-binaries` inventory lists every built triple. Run a
   localhost `tomo connect` smoke with the fat binary and confirm the push says
   `[embedded static artifact]` (not the dev-substitution warning).
5. On a mac: build the two darwin triples, add them to `dist/`, rebuild the fat
   binaries.
6. Tag `v<version>` (must equal `CARGO_PKG_VERSION`); the `release-artifacts`
   CI job publishes the Linux `dist/`.
