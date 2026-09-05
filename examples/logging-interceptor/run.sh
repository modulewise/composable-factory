#!/bin/bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"

./build.sh

echo "==> Invoking the intercepted greeter:"
composable invoke config.toml -- intercepted.greeter.greet World
