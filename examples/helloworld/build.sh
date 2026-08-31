#!/bin/bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"

FACTORY="lib/helloworld-factory.wasm"

echo "==> Building helloworld-factory core module..."
cargo build --release --target wasm32-unknown-unknown

CORE="../../target/wasm32-unknown-unknown/release/helloworld_factory.wasm"

echo "==> Componentizing -> ${FACTORY}..."
wasm-tools component new "$CORE" -o "$FACTORY"
