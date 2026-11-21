#!/bin/sh
# Manual live check against Docker Hub (not part of the offline suites): digest pull, --platform, history, search.
#   scripts/live-hub-check.sh <docker-bin>
cp "${1:?docker binary}" /root/dk.live && chmod +x /root/dk.live
export RUST_DOCKER_HOME=$(mktemp -d /root/live.XXXXXX)
D=/root/dk.live
$D pull alpine:3.20 | tail -1
ref=$($D image inspect alpine:3.20 | grep -o 'alpine@sha256:[0-9a-f]*' | head -1)
echo "digest ref: $ref"
$D pull "$ref" | tail -1
$D pull --platform linux/amd64 alpine:3.20 | tail -1
$D history alpine:3.20
$D search alpine | head -3
rm -rf "$RUST_DOCKER_HOME" /root/dk.live
