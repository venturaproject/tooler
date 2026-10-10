#!/bin/sh
set -e

# ── config ────────────────────────────────────────────────────────────────────
BIN_NAME="tooler"
INSTALL_DIR="/usr/local/bin"
BASE_URL="https://internal.empresa.com/cli/releases"
VERSION_URL="https://internal.empresa.com/cli/version"

# ── detect OS ─────────────────────────────────────────────────────────────────
OS="$(uname -s)"
case "$OS" in
  Linux)  OS="linux"  ;;
  Darwin) OS="macos"  ;;
  *)
    echo "error: unsupported OS: $OS" >&2
    exit 1
    ;;
esac

# ── detect architecture ───────────────────────────────────────────────────────
ARCH="$(uname -m)"
case "$ARCH" in
  x86_64 | amd64)   ARCH="x86_64"  ;;
  aarch64 | arm64)  ARCH="aarch64" ;;
  *)
    echo "error: unsupported architecture: $ARCH" >&2
    exit 1
    ;;
esac

# ── resolve version ───────────────────────────────────────────────────────────
if [ -z "$TOOLER_VERSION" ]; then
  TOOLER_VERSION="$(curl -fsSL "$VERSION_URL" 2>/dev/null || true)"
fi

if [ -z "$TOOLER_VERSION" ]; then
  echo "error: could not determine version from $VERSION_URL" >&2
  echo "       Connect to the VPN and retry, or set TOOLER_VERSION manually:" >&2
  echo "       TOOLER_VERSION=v0.2.0 curl -fsSL ${BASE_URL%/releases}/install.sh | sh" >&2
  exit 1
fi

ARTIFACT="${BIN_NAME}-${OS}-${ARCH}"
URL="${BASE_URL}/${TOOLER_VERSION}/${ARTIFACT}"
CHECKSUM_URL="${BASE_URL}/${TOOLER_VERSION}/SHA256SUMS.txt"

echo "Installing tooler ${TOOLER_VERSION} (${OS}/${ARCH})..."

# ── download ──────────────────────────────────────────────────────────────────
TMP="$(mktemp)"
SUMS="$(mktemp)"
cleanup() { rm -f "$TMP" "$SUMS"; }
trap cleanup EXIT HUP INT TERM
if ! curl -fsSL "$URL" -o "$TMP"; then
  echo "error: download failed from $URL" >&2
  echo "       Make sure you are connected to the VPN." >&2
  exit 1
fi
if ! curl -fsSL "$CHECKSUM_URL" -o "$SUMS"; then
  echo "error: checksum manifest download failed from $CHECKSUM_URL" >&2
  exit 1
fi
EXPECTED="$(awk -v artifact="$ARTIFACT" '$2 == artifact { count++; hash=$1 } END { if (count == 1 && hash ~ /^[0-9a-fA-F]{64}$/) print hash; else exit 1 }' "$SUMS")" || {
  echo "error: checksum manifest has no unique valid entry for $ARTIFACT" >&2
  exit 1
}
if command -v sha256sum >/dev/null 2>&1; then ACTUAL="$(sha256sum "$TMP" | awk '{print $1}')"; else ACTUAL="$(shasum -a 256 "$TMP" | awk '{print $1}')"; fi
if [ "$EXPECTED" != "$ACTUAL" ]; then
  echo "error: checksum verification failed for $ARTIFACT" >&2
  exit 1
fi
chmod +x "$TMP"

# ── install ───────────────────────────────────────────────────────────────────
if [ -w "$INSTALL_DIR" ] || sudo -n true 2>/dev/null; then
  sudo mv "$TMP" "${INSTALL_DIR}/${BIN_NAME}"
  echo "Installed to ${INSTALL_DIR}/${BIN_NAME}"
else
  LOCAL_BIN="$HOME/.local/bin"
  mkdir -p "$LOCAL_BIN"
  mv "$TMP" "${LOCAL_BIN}/${BIN_NAME}"
  echo "Installed to ${LOCAL_BIN}/${BIN_NAME}"
  echo ""
  echo "Make sure ${LOCAL_BIN} is in your PATH:"
  echo '  export PATH="$HOME/.local/bin:$PATH"'
fi

echo ""
tooler --version 2>/dev/null && echo "Done. Run: tooler --help" || echo "Done. Open a new terminal and run: tooler --help"
