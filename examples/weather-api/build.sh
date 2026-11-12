#!/bin/bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"

mkdir -p lib

echo "==> Building url-api-factory, templated-http-client, and json-mapper..."
( cd ../../components && ./build.sh >/dev/null )
for component in url-api-factory templated-http-client json-mapper; do
  cp "../../components/lib/${component}.wasm" lib/
done

if [[ ! -f lib/http-client.wasm ]]; then
  wkg oci pull -o lib/http-client.wasm ghcr.io/componentized/http/client:0.1.0
fi
