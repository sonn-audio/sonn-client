#!/usr/bin/env bash
# One-command install for Sonn Client.
#
#   curl -fsSL https://raw.githubusercontent.com/sonn-audio/sonn-client/main/install.sh | sudo bash
#
# Downloads the release build for this machine's architecture, installs it to /usr/local/bin, and
# hands over to `sonn-client install` for the systemd part. Everything after this -- which server,
# which sound card, which room -- is configured from the audioserver, not here.
set -euo pipefail

REPO="${SONN_CLIENT_REPO:-sonn-audio/sonn-client}"
VERSION="${SONN_CLIENT_VERSION:-latest}"
BIN_DIR="${SONN_CLIENT_BIN_DIR:-/usr/local/bin}"
BIN_NAME="sonn-client"

die() {
  echo "error: $*" >&2
  exit 1
}

need() {
  command -v "$1" >/dev/null 2>&1 || die "$1 is required but not installed"
}

[ "$(id -u)" -eq 0 ] || die "run as root (sudo bash) -- the service and /usr/local/bin need it"
need curl
need tar

case "$(uname -s)" in
  Linux) ;;
  *) die "only Linux is supported" ;;
esac

# Pi 5/4/3 on a 64-bit OS report aarch64; a 32-bit Pi OS reports armv7l, and Pi 1 / Zero armv6l.
case "$(uname -m)" in
  x86_64 | amd64) TARGET="x86_64-unknown-linux-gnu" ;;
  aarch64 | arm64) TARGET="aarch64-unknown-linux-gnu" ;;
  armv7l | armv7) TARGET="armv7-unknown-linux-gnueabihf" ;;
  armv6l | arm) TARGET="arm-unknown-linux-gnueabihf" ;;
  *) die "unsupported architecture: $(uname -m)" ;;
esac

if [ "$VERSION" = "latest" ]; then
  echo "Resolving latest release of ${REPO}..."
  VERSION="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" |
    sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n1)"
  [ -n "$VERSION" ] || die "could not resolve the latest release tag; set SONN_CLIENT_VERSION=vX.Y.Z"
fi
VERSION="${VERSION#v}"

ASSET="${BIN_NAME}-${VERSION}-${TARGET}.tar.gz"
URL="https://github.com/${REPO}/releases/download/v${VERSION}/${ASSET}"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

echo "Downloading ${ASSET}..."
curl -fsSL "$URL" -o "${TMP}/${ASSET}" || die "download failed: ${URL}"
tar -xzf "${TMP}/${ASSET}" -C "$TMP"
[ -f "${TMP}/${BIN_NAME}" ] || die "archive did not contain ${BIN_NAME}"

# Installed to a temporary name first and moved into place: an atomic rename means a running service
# is never handed a half-written binary.
install -m 0755 "${TMP}/${BIN_NAME}" "${BIN_DIR}/.${BIN_NAME}.new"
mv -f "${BIN_DIR}/.${BIN_NAME}.new" "${BIN_DIR}/${BIN_NAME}"
echo "Installed ${BIN_DIR}/${BIN_NAME} (${VERSION})"

"${BIN_DIR}/${BIN_NAME}" install

cat <<'EOF'

Done. Sound cards on this device:
EOF
"${BIN_DIR}/${BIN_NAME}" devices || true

cat <<'EOF'

The device is now looking for your audioserver over mDNS. Open the server's admin UI, find this
client under its devices, pick a sound card and assign it to a zone. Nothing else needs to be
installed or configured here.

Logs:    journalctl -u sonn-client -f
Restart: systemctl restart sonn-client
EOF
