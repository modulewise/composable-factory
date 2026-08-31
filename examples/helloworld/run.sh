#!/bin/bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"

./build.sh

FACTORY="lib/helloworld-factory.wasm"
OUT="${1:-lib/helloworld.wasm}"

echo ""
echo "==> Generating ${OUT} via the factory..."
composable invoke "$FACTORY" -- helloworld-factory.factory.build > "$OUT"

echo ""
echo "==> Generated WIT:"
wasm-tools component wit "$OUT" | head -8

echo ""
echo "==> Invoking the greeter:"
echo -n "    greeter.say-hello world => "; composable invoke config.toml -- greeter.say-hello world
