#!/bin/bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"

./build.sh

echo "==> Generating logging-greeter.wasm via the factory..."
composable invoke factory-config.toml -- factory.factory.build > lib/logging-greeter.wasm

echo ""
echo "==> Generated WIT:"
wasm-tools component wit lib/logging-greeter.wasm | head -8

echo ""
echo "==> Invoking the intercepted greeter:"
composable invoke greeter-config.toml -- intercepted.greeter.greet World
