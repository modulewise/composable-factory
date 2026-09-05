#!/bin/bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"

mkdir -p lib

echo "==> Building logging-interceptor-factory core module..."
cargo build --release --target wasm32-unknown-unknown

echo "==> Componentizing -> lib/logging-interceptor-factory.wasm..."
wasm-tools component new \
  ../../target/wasm32-unknown-unknown/release/logging_interceptor_factory.wasm \
  -o lib/logging-interceptor-factory.wasm

if [[ ! -f ../../components/lib/filesystem-loader.wasm ]]; then
  echo "==> Building filesystem-loader..."
  ( cd ../../components && ./build.sh >/dev/null )
  cp ../../components/lib/filesystem-loader.wasm lib/
fi

if [[ ! -f lib/hello.wasm ]]; then
  wkg oci pull -o lib/hello.wasm ghcr.io/modulewise/demo/hello:0.2.0
fi

if [[ ! -f lib/wasi-logging-to-stdout.wasm ]]; then
  wkg oci pull -o lib/wasi-logging-to-stdout.wasm ghcr.io/componentized/logging/to-stdout:v0.2.1
fi
