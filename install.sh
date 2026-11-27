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
  # `docker` goes only if it is ours: with the `sdocker` choice it belongs to a real Docker
  if /usr/local/bin/docker --version 2>/dev/null | grep -q "build sasta"; then
    $SUDO rm -f /usr/local/bin/docker /usr/local/bin/docker-gui
  fi
  $SUDO rm -f /usr/local/bin/docker-linux-* /usr/local/share/applications/sasta-docker.desktop
  # the `sdocker` commands: links (Linux) or small scripts (macOS) that point into our lib folder
  for f in /usr/local/bin/sdocker /usr/local/bin/sdocker-gui; do
    if { [ -L "$f" ] && readlink "$f" | grep -q /lib/sasta-docker/; } || { [ ! -L "$f" ] && grep -q /lib/sasta-docker/ "$f" 2>/dev/null; }; then
      $SUDO rm -f "$f"
    fi
  done
  $SUDO rm -rf /usr/local/lib/sasta-docker
  [ "$OS" = Darwin ] && $SUDO pkgutil --forget dev.sasta.docker >/dev/null 2>&1 || true
  echo "Sasta Docker removed."
  exit 0
fi

# the command name: `docker` replaces Docker's command; `sdocker` keeps both side by side
NAME="${SASTA_CMD:-}"
if [ -z "$NAME" ] && [ -r /dev/tty ]; then
  printf "Which command do you want?
  1) docker   (replaces Docker's command)
  2) sdocker  (use it next to a real Docker)
Choose [1]: " >/dev/tty
  read -r NAME </dev/tty || NAME=""
  case "$NAME" in 2) NAME=sdocker ;; *) NAME=docker ;; esac
fi
NAME="${NAME:-docker}"
case "$NAME" in docker | sdocker) ;; *) echo "Sasta Docker: the command must be docker or sdocker (not '$NAME')" >&2; exit 1 ;; esac

case "$OS" in
  Linux)
    FILE="sasta-docker-linux-$ARCH.tar.gz"
    echo "Downloading $FILE ..."
    curl -fSL "$BASE/$FILE" -o "$TMP/$FILE"
    tar -xzf "$TMP/$FILE" -C "$TMP"
    $SUDO env SASTA_CMD="$NAME" sh "$TMP/sasta-docker-linux-$ARCH/install.sh"
    echo
    echo "Done. Try:  sudo $NAME run alpine echo hello      (the engine needs root; or: $NAME --rootless run alpine echo hello)"
    echo "Desktop app: $( [ "$NAME" = docker ] && echo docker-gui || echo "$NAME-gui" )"
    ;;
  Darwin)
    FILE="SastaDocker-macos-$ARCH.pkg"
    echo "Downloading $FILE ..."
    curl -fSL "$BASE/$FILE" -o "$TMP/$FILE"
    if [ "$NAME" = docker ]; then
      $SUDO installer -pkg "$TMP/$FILE" -target /
    else
      # unpack instead of installing: the package writes /usr/local/bin/docker, which here belongs to a real Docker
      pkgutil --expand-full "$TMP/$FILE" "$TMP/pkg"
      LIB=/usr/local/lib/sasta-docker
      $SUDO mkdir -p "$LIB" /usr/local/bin
      $SUDO cp "$TMP/pkg/Payload/usr/local/bin/"* "$LIB/"
      $SUDO chmod 755 "$LIB/"*
      # small scripts, not links: macOS would otherwise look for the other programs next to the link
      for p in "$NAME:docker" "$NAME-gui:docker-gui"; do
        $SUDO rm -f "/usr/local/bin/${p%:*}"
        printf '#!/bin/sh\nexec "%s/%s" "$@"\n' "$LIB" "${p#*:}" | $SUDO tee "/usr/local/bin/${p%:*}" >/dev/null
        $SUDO chmod 755 "/usr/local/bin/${p%:*}"
      done
    fi
    echo
    echo "Done. macOS runs Linux containers in a small VM:"
    echo "  brew install lima        (once)"
    echo "  $NAME vm init           (once)"
    echo "  $NAME run alpine echo hello"
    echo "Desktop app: $( [ "$NAME" = docker ] && echo docker-gui || echo "$NAME-gui" )"
    ;;
  *) echo "Sasta Docker: this installer is for Linux and macOS (on Windows use install.ps1)" >&2; exit 1 ;;
esac
