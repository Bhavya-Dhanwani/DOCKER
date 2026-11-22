#!/bin/sh
# Installs Sasta Docker from the unpacked release folder: the `docker` command and the `docker-gui` app.
# Usage: sudo ./install.sh [prefix]      (default prefix /usr/local;  ./install.sh --uninstall removes it)
set -e
PREFIX="${1:-/usr/local}"
HERE=$(cd "$(dirname "$0")" && pwd)
if [ "$1" = "--uninstall" ]; then
  PREFIX=/usr/local
  rm -f "$PREFIX/bin/docker" "$PREFIX/bin/docker-gui" "$PREFIX/bin/docker-linux-"* "$PREFIX/share/applications/sasta-docker.desktop"
  echo "Sasta Docker removed."; exit 0
fi
install -d "$PREFIX/bin"
for f in docker docker-gui docker-linux-*; do
  [ -f "$HERE/$f" ] && install -m 755 "$HERE/$f" "$PREFIX/bin/$f"
done
if [ -d "$PREFIX/share" ] && [ -f "$HERE/sasta-docker.desktop" ]; then
  install -d "$PREFIX/share/applications"
  sed "s#^Exec=.*#Exec=$PREFIX/bin/docker-gui#" "$HERE/sasta-docker.desktop" > "$PREFIX/share/applications/sasta-docker.desktop"
fi
echo "Installed to $PREFIX/bin:  docker  docker-gui"
echo "Try:  sudo docker run alpine echo hello      (the engine needs root; or use: docker --rootless ...)"
