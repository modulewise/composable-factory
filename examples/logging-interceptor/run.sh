#!/bin/bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"

./build.sh

echo "==> Invoking the logging greeter:"
composable invoke config.toml -- logging-greeter.greeter.greet World
