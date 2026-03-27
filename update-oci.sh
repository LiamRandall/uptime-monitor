#!/usr/bin/env bash
set -euo pipefail

if [ $# -ne 1 ]; then
    echo "Usage: $0 <version>" >&2
    echo "Example: $0 0.2.0" >&2
    exit 1
fi

VERSION="$1"
REGISTRY="ghcr.io/liamrandall"

echo "Building standalone component (no cron export, for Cosmonic Control)..."
cargo build -p uptime-monitor --release --target wasm32-wasip2 --no-default-features

echo "Pushing uptime-monitor:${VERSION}..."
wash oci push "${REGISTRY}/uptime-monitor:${VERSION}" \
    target/wasm32-wasip2/release/uptime_monitor.wasm

echo "Building cron service..."
cargo build -p uptime-monitor-service --release --target wasm32-wasip2

echo "Pushing uptime-monitor-service:${VERSION}..."
wash oci push "${REGISTRY}/uptime-monitor-service:${VERSION}" \
    target/wasm32-wasip2/release/uptime-monitor-service.wasm

echo "Done. Pushed both components as ${VERSION}."
