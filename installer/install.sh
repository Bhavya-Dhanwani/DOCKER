#!/bin/sh
# Installs Sasta Docker from the unpacked release folder: the `docker` command and the `docker-gui` app.
# Usage: sudo ./install.sh [prefix]      (default prefix /usr/local;  ./install.sh --uninstall removes it)
# The command is `docker`, or `sdocker` to keep a real Docker's name free (only these two):
#   sudo SASTA_CMD=sdocker ./install.sh        (or answer the question when it asks)
set -e
PREFIX="${1:-/usr/local}"
HERE=$(cd "$(dirname "$0")" && pwd)
LIB="$PREFIX/lib/sasta-docker"
if [ "$1" = "--uninstall" ]; then
  PREFIX=/usr/local; LIB="$PREFIX/lib/sasta-docker"
  # `docker` goes only if it is ours: with the `sdocker` choice it belongs to a real Docker
  if "$PREFIX/bin/docker" --version 2>/dev/null | grep -q "build sasta"; then
    rm -f "$PREFIX/bin/docker" "$PREFIX/bin/docker-gui"
  fi
  rm -f "$PREFIX/bin/docker-linux-"* "$PREFIX/share/applications/sasta-docker.desktop"
  find "$PREFIX/bin" -type l -lname "*/lib/sasta-docker/*" -delete 2>/dev/null || true
  rm -rf "$LIB"
  echo "Sasta Docker removed."; exit 0
fi
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
if [ "$NAME" = docker ]; then
  DEST="$PREFIX/bin"
else
  DEST="$LIB"   # the programs live here; only the chosen name is linked into bin
fi
install -d "$PREFIX/bin" "$DEST"
for f in docker docker-gui docker-linux-*; do
  [ -f "$HERE/$f" ] && install -m 755 "$HERE/$f" "$DEST/$f"
done
if [ "$NAME" != docker ]; then
  ln -sf "$DEST/docker" "$PREFIX/bin/$NAME"
  ln -sf "$DEST/docker-gui" "$PREFIX/bin/$NAME-gui"
fi
GUI="$PREFIX/bin/docker-gui"; [ "$NAME" = docker ] || GUI="$PREFIX/bin/$NAME-gui"
if [ -d "$PREFIX/share" ] && [ -f "$HERE/sasta-docker.desktop" ]; then
  install -d "$PREFIX/share/applications"
  sed "s#^Exec=.*#Exec=$GUI#" "$HERE/sasta-docker.desktop" > "$PREFIX/share/applications/sasta-docker.desktop"
fi
echo "Installed:  $NAME  ${GUI##*/}"
echo "Try:  sudo $NAME run alpine echo hello      (the engine needs root; or use: $NAME --rootless ...)"
