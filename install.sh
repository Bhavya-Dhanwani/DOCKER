#!/bin/sh
# Sasta Docker installer for Linux and macOS:
#   curl -fsSL https://raw.githubusercontent.com/Bhavya-Dhanwani/DOCKER/main/install.sh | sh
# Downloads the latest release for this machine and installs `docker` and `docker-gui` into /usr/local/bin.
# Remove it again with:  curl -fsSL https://raw.githubusercontent.com/Bhavya-Dhanwani/DOCKER/main/install.sh | sh -s -- --uninstall
set -e
REPO="Bhavya-Dhanwani/DOCKER"
BASE="https://github.com/$REPO/releases/latest/download"
OS=$(uname -s)
case "$(uname -m)" in
  x86_64 | amd64) ARCH=x86_64 ;;
  aarch64 | arm64) ARCH=aarch64 ;;
  *) echo "Sasta Docker: unsupported CPU $(uname -m)" >&2; exit 1 ;;
esac
SUDO=""
[ "$(id -u)" = 0 ] || SUDO="sudo"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

if [ "$1" = "--uninstall" ]; then
  $SUDO rm -f /usr/local/bin/docker /usr/local/bin/docker-gui /usr/local/bin/docker-linux-* /usr/local/share/applications/sasta-docker.desktop
  [ "$OS" = Darwin ] && $SUDO pkgutil --forget dev.sasta.docker >/dev/null 2>&1 || true
  echo "Sasta Docker removed."
  exit 0
fi

case "$OS" in
  Linux)
    FILE="sasta-docker-linux-$ARCH.tar.gz"
    echo "Downloading $FILE ..."
    curl -fSL "$BASE/$FILE" -o "$TMP/$FILE"
    tar -xzf "$TMP/$FILE" -C "$TMP"
    $SUDO sh "$TMP/sasta-docker-linux-$ARCH/install.sh"
    echo
    echo "Done. Try:  sudo docker run alpine echo hello      (the engine needs root; or: docker --rootless run alpine echo hello)"
    echo "Desktop app: docker-gui"
    ;;
  Darwin)
    FILE="SastaDocker-macos-$ARCH.pkg"
    echo "Downloading $FILE ..."
    curl -fSL "$BASE/$FILE" -o "$TMP/$FILE"
    $SUDO installer -pkg "$TMP/$FILE" -target /
    echo
    echo "Done. macOS runs Linux containers in a small VM:"
    echo "  brew install lima        (once)"
    echo "  docker vm init           (once)"
    echo "  docker run alpine echo hello"
    echo "Desktop app: docker-gui"
    ;;
  *) echo "Sasta Docker: this installer is for Linux and macOS (on Windows use install.ps1)" >&2; exit 1 ;;
esac
