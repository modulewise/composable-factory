#!/bin/bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"

./build.sh

echo ""
echo "==> Invoking the weather component:"
echo "    get-point 42.3611 -71.0571"
point=$(composable invoke config.toml -- weather.weather.get-point 42.3611 -71.0571)
echo "$point"

office=$(jq -r '.properties["grid-id"]' <<<"$point")
grid_x=$(jq -r '.properties["grid-x"]' <<<"$point")
grid_y=$(jq -r '.properties["grid-y"]' <<<"$point")
echo "    get-forecast $office $grid_x $grid_y"
composable invoke config.toml -- weather.weather.get-forecast "$office" "$grid_x" "$grid_y" \
  | jq '.properties.periods[:2]'

echo "    get-alerts MA"
composable invoke config.toml -- weather.weather.get-alerts MA
