#!/bin/sh
# Tomo installer — https://github.com/jakequist/tomo
#
#   curl -fsSL https://tomo-sync.dev/install.sh | sh
#
# Detects OS/arch, downloads the latest release binary from GitHub Releases,
# verifies its SHA-256 against the release's published SHA256SUMS, and
# installs to ~/.local/bin (override with TOMO_INSTALL_DIR). No sudo, no
# dependencies beyond curl + a sha256 tool. POSIX sh — works on macOS and any
# Linux (the binaries are fully static).
set -eu

REPO="jakequist/tomo"
BASE="https://github.com/${REPO}/releases/latest/download"
INSTALL_DIR="${TOMO_INSTALL_DIR:-$HOME/.local/bin}"

say()  { printf '%s\n' "$*" >&2; }
fail() { say "install failed: $*"; exit 1; }

# --- platform detection ----------------------------------------------------
os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
  Linux)  os_tag=linux ;;
  Darwin) os_tag=macos ;;
  *) fail "unsupported OS: $os (Tomo supports Linux and macOS)" ;;
esac
case "$arch" in
  x86_64|amd64)  arch_tag=x86_64 ;;
  aarch64|arm64) arch_tag=arm64 ;;
  *) fail "unsupported architecture: $arch (supported: x86_64, arm64)" ;;
esac
asset="tomo-${os_tag}-${arch_tag}"

# --- sha256 tool (linux: sha256sum; macos: shasum) -------------------------
if command -v sha256sum >/dev/null 2>&1; then
  sha_of() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null 2>&1; then
  sha_of() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
  fail "need sha256sum or shasum to verify the download"
fi

command -v curl >/dev/null 2>&1 || fail "curl is required"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

say "→ downloading ${asset} (latest release)…"
if ! curl -fsSL -o "$tmp/tomo" "${BASE}/${asset}"; then
  fail "could not download ${BASE}/${asset}
  (if Tomo has no published release yet, this is expected — watch
   https://github.com/${REPO}/releases for v0.1.0)"
fi

say "→ verifying SHA-256…"
curl -fsSL -o "$tmp/SHA256SUMS" "${BASE}/SHA256SUMS" \
  || fail "could not download SHA256SUMS for verification"
want="$(grep " ${asset}\$" "$tmp/SHA256SUMS" | cut -d' ' -f1)"
[ -n "$want" ] || fail "no checksum entry for ${asset} in SHA256SUMS"
got="$(sha_of "$tmp/tomo")"
[ "$want" = "$got" ] || fail "checksum mismatch (expected $want, got $got) — aborting"

# --- install ---------------------------------------------------------------
mkdir -p "$INSTALL_DIR"
chmod +x "$tmp/tomo"
# Atomic-ish: move into place (same filesystem as target via a sibling temp).
mv -f "$tmp/tomo" "$INSTALL_DIR/tomo"

say "✓ installed $("$INSTALL_DIR/tomo" --version) to ${INSTALL_DIR}/tomo"

case ":$PATH:" in
  *":$INSTALL_DIR:"*) : ;;
  *) say ""
     say "note: ${INSTALL_DIR} is not on your PATH. Add it, e.g.:"
     say "  export PATH=\"${INSTALL_DIR}:\$PATH\"" ;;
esac

say ""
say "get started:"
say "  cd your-project && tomo init"
say "  tomo sync user@host /path/to/remote/copy   # records the peer + syncs"
