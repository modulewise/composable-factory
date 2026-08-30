#!/bin/bash

set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"

if command -v composable &>/dev/null; then
  echo ""
  echo "==> Invoking via composable:"
  echo -n "    calc.add 2 3       => "; composable invoke config.toml -- calc.add 2 3
  echo -n "    calc.multiply 6 7  => "; composable invoke config.toml -- calc.multiply 6 7
fi
