#!/bin/bash

# Runs the whole ABI suite, including the random types, which a plain
# `cargo test` skips.
#
#   ./run.sh                          seed 24301, 150 types per seed
#   ABI_SEEDS=24301,1,777 ./run.sh    several seeds
#   ABI_CASES=500 ./run.sh            more types per seed

set -euo pipefail

cd "$(dirname "$0")/../.."
exec cargo test --test abi -- --include-ignored --nocapture
