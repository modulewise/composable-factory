#!/bin/bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"

echo "==> Invoking the github component:"
echo "    get-repository bytecodealliance wasmtime"
composable invoke config.toml -- github.github.get-repository bytecodealliance wasmtime \
  | jq '{"full-name", "stargazers-count", "forks-count", language, topics}'

echo "    get-latest-release bytecodealliance wasmtime"
composable invoke config.toml -- github.github.get-latest-release bytecodealliance wasmtime

echo "    search-repositories \"topic:webassembly language:rust\" 3"
composable invoke config.toml -- github.github.search-repositories "topic:webassembly language:rust" 3 \
  | jq '{"total-count", items: [.items[] | {"full-name", "stargazers-count"}]}'
