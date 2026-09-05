#!/bin/bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"

PROJECTS=$(cargo metadata --no-deps --format-version 1 | jq -r '.packages[].name')

for project in $PROJECTS; do
  echo -e "\nBuilding $project..."

  target=wasm32-unknown-unknown
  cargo build -p "$project" --target $target --release

  cargo_name=$(echo "$project" | tr '-' '_')
  core_wasm="target/${target}/release/${cargo_name}.wasm"
  component="lib/${project}.wasm"
  wasm-tools component new "$core_wasm" -o "$component"

  echo -e "\nBuilt $component with WIT:"
  wasm-tools component wit lib/${project}.wasm | grep -E 'world|import|export' | sed 's/^/  /'
  echo -e "  }"
done
