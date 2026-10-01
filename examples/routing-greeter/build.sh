#!/bin/bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"

mkdir -p lib

echo "==> Building routing-greeter-factory and greeter core modules..."
cargo build --release --target wasm32-unknown-unknown -p routing-greeter-factory -p greeter

echo "==> Componentizing -> lib/routing-greeter-factory.wasm, lib/greeter.wasm..."
wasm-tools component new \
  ../target/wasm32-unknown-unknown/release/routing_greeter_factory.wasm \
  -o lib/routing-greeter-factory.wasm
wasm-tools component new \
  ../target/wasm32-unknown-unknown/release/greeter.wasm \
  -o lib/greeter.wasm
