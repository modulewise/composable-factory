#!/bin/bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"

./build.sh

echo ""
echo "==> Invoking the routing greeter:"
for locale in en-AU es-MX fr de-DE; do
  printf "    greet world %-6s => " "$locale"
  composable invoke config.toml -- routing-greeter.greet world "$locale"
done
