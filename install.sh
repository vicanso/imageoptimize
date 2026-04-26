#!/bin/bash
set -e

REPO="vicanso/imageoptimize"
INSTALL_DIR="${INSTALL_DIR:-/usr/local/bin}"
VERSION="${1:-${VERSION:-latest}}"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
CYAN='\033[0;36m'
NC='\033[0m'

info()  { printf "${CYAN}%s${NC}\n" "$1"; }
ok()    { printf "${GREEN}%s${NC}\n" "$1"; }
error() { printf "${RED}%s${NC}\n" "$1" >&2; exit 1; }

# Detect OS
case "$(uname -s)" in
  Linux*)  OS="linux" ;;
  Darwin*) OS="darwin" ;;
  *) error "Unsupported OS: $(uname -s)" ;;
esac

# Detect architecture
case "$(uname -m)" in
  x86_64)        ARCH="x86_64" ;;
  aarch64|arm64) ARCH="aarch64" ;;
  *) error "Unsupported architecture: $(uname -m)" ;;
esac

# Build artifact name
if [ "$OS" = "darwin" ]; then
  ARTIFACT="imageoptimize-darwin-${ARCH}.tar.gz"
else
  ARTIFACT="imageoptimize-linux-musl-${ARCH}.tar.gz"
fi

# Build download URL
if [ "$VERSION" = "latest" ]; then
  URL="https://github.com/${REPO}/releases/latest/download/${ARTIFACT}"
else
  URL="https://github.com/${REPO}/releases/download/${VERSION}/${ARTIFACT}"
fi

info "Platform : ${OS}/${ARCH}"
info "Version  : ${VERSION}"
info "Artifact : ${ARTIFACT}"
info "Install  : ${INSTALL_DIR}/imageoptimize"
echo ""

# Require curl
command -v curl >/dev/null 2>&1 || error "curl is required but not found"

# Download to a temp dir, clean up on exit
TMP_DIR=$(mktemp -d)
trap 'rm -rf "$TMP_DIR"' EXIT

info "Downloading ${URL} ..."
curl -fSL --progress-bar "$URL" -o "${TMP_DIR}/${ARTIFACT}" \
  || error "Download failed. Check that version '${VERSION}' exists at https://github.com/${REPO}/releases"

tar -xzf "${TMP_DIR}/${ARTIFACT}" -C "${TMP_DIR}"
chmod +x "${TMP_DIR}/imageoptimize"

# Install — use sudo only when the target dir isn't writable
if [ -w "$INSTALL_DIR" ]; then
  mv "${TMP_DIR}/imageoptimize" "${INSTALL_DIR}/imageoptimize"
else
  info "Requesting sudo to write to ${INSTALL_DIR} ..."
  sudo mv "${TMP_DIR}/imageoptimize" "${INSTALL_DIR}/imageoptimize"
fi

ok ""
ok "imageoptimize installed successfully!"
"${INSTALL_DIR}/imageoptimize" --version
