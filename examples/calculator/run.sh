#!/bin/bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"

./build.sh

FACTORY="lib/calculator-factory.wasm"
OUT="${1:-lib/calculator.wasm}"

echo ""
echo "==> Generating ${OUT} via the factory..."
composable invoke "$FACTORY" -- calculator-factory.factory.build > "$OUT"

echo ""
echo "==> Generated WIT:"
wasm-tools component wit "$OUT" | head -8

echo ""
echo "==> Invoking the calculator:"
echo -n "    calc.add 2 3       => "; composable invoke config.toml -- calc.add 2 3
echo -n "    calc.multiply 6 7  => "; composable invoke config.toml -- calc.multiply 6 7
