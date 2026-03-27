#!/usr/bin/env bash
set -euo pipefail

if [ $# -ne 1 ]; then
    echo "Usage: $0 <version>" >&2
    echo "Example: $0 0.2.0" >&2
    exit 1
fi

VERSION="$1"
REGISTRY="ghcr.io/liamrandall"

echo "Building workspace..."
wash build

echo "Pushing uptime-monitor:${VERSION}..."
wash push "${REGISTRY}/uptime-monitor:${VERSION}" \
    target/wasm32-wasip2/release/uptime_monitor.wasm

echo "Pushing uptime-monitor-service:${VERSION}..."
wash push "${REGISTRY}/uptime-monitor-service:${VERSION}" \
    target/wasm32-wasip2/release/uptime-monitor-service.wasm

echo "Done. Pushed both components as ${VERSION}."
