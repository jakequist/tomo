#!/usr/bin/env bash
# Assemble Tomo's Linux release artifacts and a fat host binary.
#
# Steps (docs/RELEASING.md, cross-release skill):
#   1. Build a static musl release binary for every available Linux triple.
#   2. Copy them to dist/tomo-<version>-<triple> and write dist/SHA256SUMS.
#   3. Rebuild the x86_64 host binary with --features embed-binaries and
#      TOMO_EMBED_DIR=dist so it embeds the artifacts from step 2.
#   4. Verify the fat binary reports the embedded inventory (tomo dev
#      embedded-binaries), asserting x86_64-musl is present.
#
# Idempotent (re-runnable; dist/ is rebuilt each run) and fails loudly.
# Darwin artifacts are NOT produced here — they require a macOS runner
# (docs/RELEASING.md). aarch64-musl is included only when a musl cross linker is
# available (cargo-zigbuild + zig); otherwise it is skipped with a notice.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

die() { echo "release.sh: ERROR: $*" >&2; exit 1; }
note() { echo ">> $*"; }

# --- version (single source: workspace [workspace.package].version) ----------
VERSION="$(sed -nE 's/^version[[:space:]]*=[[:space:]]*"([^"]+)".*/\1/p' Cargo.toml | head -1)"
[ -n "$VERSION" ] || die "could not determine version from Cargo.toml"
note "tomo version $VERSION"

X86_MUSL="x86_64-unknown-linux-musl"
ARM_MUSL="aarch64-unknown-linux-musl"

# --- toolchain detection -----------------------------------------------------
# zig + cargo-zigbuild give a working musl cross linker (Debian's aarch64 gcc is
# glibc-only and fails to link musl SQLite — see docs/RELEASING.md).
HAVE_ZIG=0
if command -v cargo-zigbuild >/dev/null 2>&1 && command -v zig >/dev/null 2>&1; then
  HAVE_ZIG=1
fi

installed_targets="$(rustup target list --installed 2>/dev/null || true)"

TRIPLES=("$X86_MUSL")
if [ "$HAVE_ZIG" = 1 ] && echo "$installed_targets" | grep -qx "$ARM_MUSL"; then
  TRIPLES+=("$ARM_MUSL")
else
  note "SKIP $ARM_MUSL (need cargo-zigbuild + zig and the rustup target); see docs/RELEASING.md"
fi

# Build a single crate/target as a static release binary.
build_one() {
  local triple="$1"; shift
  if [ "$HAVE_ZIG" = 1 ]; then
    cargo zigbuild "$@" --release --target "$triple"
  else
    cargo build "$@" --release --target "$triple"
  fi
}

assert_static() {
  local bin="$1"
  [ -f "$bin" ] || die "expected binary missing: $bin"
  file "$bin" | grep -q 'statically linked' || die "not statically linked: $bin"
}

DIST="$ROOT/dist"
note "resetting $DIST"
rm -rf "$DIST"
mkdir -p "$DIST"

# --- step 1+2: thin per-triple artifacts + checksums -------------------------
for triple in "${TRIPLES[@]}"; do
  note "building thin release binary: $triple"
  build_one "$triple" -p tomo
  bin="target/$triple/release/tomo"
  assert_static "$bin"
  cp "$bin" "$DIST/tomo-$VERSION-$triple"
done

note "writing dist/SHA256SUMS"
( cd "$DIST" && sha256sum tomo-"$VERSION"-* > SHA256SUMS )
cat "$DIST/SHA256SUMS"

# --- step 3: fat x86_64 host binary embedding the dist artifacts -------------
note "building fat $X86_MUSL binary (--features embed-binaries, TOMO_EMBED_DIR=dist)"
TOMO_EMBED_DIR="$DIST" build_one "$X86_MUSL" -p tomo --features embed-binaries
FAT="target/$X86_MUSL/release/tomo"
assert_static "$FAT"
cp "$FAT" "$DIST/tomo-$VERSION-$X86_MUSL.fat"

# --- step 4: verify the embedded inventory -----------------------------------
note "fat binary embedded inventory:"
INVENTORY="$("$FAT" dev embedded-binaries --json)"
echo "$INVENTORY"
echo "$INVENTORY" | grep -q "\"$X86_MUSL\"" \
  || die "fat binary does not embed $X86_MUSL"
for triple in "${TRIPLES[@]}"; do
  echo "$INVENTORY" | grep -q "\"$triple\"" \
    || die "fat binary does not embed built triple $triple"
done

note "release artifacts assembled in $DIST:"
ls -la "$DIST"
note "DONE (version $VERSION, triples: ${TRIPLES[*]})"
