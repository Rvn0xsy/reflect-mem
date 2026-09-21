#!/bin/sh
# Install reflect-mem from GitHub Releases.
#
#   curl -fsSL https://raw.githubusercontent.com/Rvn0xsy/reflect-mem/main/install.sh | sh
#   curl -fsSL .../install.sh | sh -s -- v0.0.1     # pin a version
#   curl -fsSL .../install.sh | INSTALL_DIR=~/.local/bin sh
#
# Environment:
#   INSTALL_DIR           target directory (default /usr/local/bin)
#   REFLECT_MEM_REPO      owner/repo override (default Rvn0xsy/reflect-mem)
#   REFLECT_MEM_VERSION   same as the first positional argument

set -eu

REPO="${REFLECT_MEM_REPO:-Rvn0xsy/reflect-mem}"
VERSION="${1:-${REFLECT_MEM_VERSION:-latest}}"
INSTALL_DIR="${INSTALL_DIR:-/usr/local/bin}"
BIN="reflect-mem"

say() { printf '%s\n' "$*" >&2; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

[ "$(uname -s)" = "Linux" ] || [ "$(uname -s)" = "Darwin" ] || die "unsupported OS: $(uname -s)"

case "$(uname -s)/$(uname -m)" in
  Linux/x86_64)   target="x86_64-unknown-linux-gnu" ;;
  Darwin/arm64)   target="aarch64-apple-darwin" ;;
  Darwin/x86_64)  target="x86_64-apple-darwin" ;;
  *) die "no prebuilt binary for $(uname -s)/$(uname -m) — build from source with cargo" ;;
esac

if [ "$VERSION" = "latest" ]; then
  base="https://github.com/${REPO}/releases/latest/download"
else
  base="https://github.com/${REPO}/releases/download/${VERSION}"
fi

asset="${BIN}-${target}.tar.gz"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

say "reflect-mem ${VERSION} (${target})"

curl -fsSL "${base}/${asset}" -o "${tmp}/${asset}" \
  || die "download failed: ${base}/${asset}"
curl -fsSL "${base}/${asset}.sha256" -o "${tmp}/${asset}.sha256" \
  || die "checksum download failed"

( cd "$tmp" && { command -v sha256sum >/dev/null 2>&1 && sha256sum -c "${asset}.sha256" \
                 || shasum -a 256 -c "${asset}.sha256"; } ) >/dev/null || die "checksum mismatch"

tar -xzf "${tmp}/${asset}" -C "$tmp"

[ -w "$INSTALL_DIR" ] || [ ! -e "$INSTALL_DIR" ] || die \
  "${INSTALL_DIR} is not writable — re-run with sudo, or set INSTALL_DIR=~/.local/bin"
mkdir -p "$INSTALL_DIR"
install -m 0755 "${tmp}/${BIN}" "${INSTALL_DIR}/${BIN}"

say "installed -> ${INSTALL_DIR}/${BIN}"
"${INSTALL_DIR}/${BIN}" --version >&2 2>/dev/null || true

case ":${PATH}:" in
  *":${INSTALL_DIR}:"*) ;;
  *) say "note: ${INSTALL_DIR} is not on your PATH" ;;
esac
