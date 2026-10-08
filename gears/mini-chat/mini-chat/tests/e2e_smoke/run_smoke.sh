#!/usr/bin/env bash
# Runs the mini-chat black-box smoke suite.
#
#   ./run_smoke.sh               build the server binary, then run the suite
#   ./run_smoke.sh --no-build    run against the existing target/debug binary
#   ./run_smoke.sh --no-build -m "not slow" -k streaming    extra args go to pytest
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../../../../.." && pwd)"

BUILD=1
if [[ "${1:-}" == "--no-build" ]]; then
  BUILD=0
  shift
fi

if [[ "$BUILD" == 1 ]]; then
  (cd "$ROOT" && cargo build --bin cf-gears-example-server --no-default-features \
    --features mini-chat,static-authn,static-authz,single-tenant,static-credstore)
fi

cd "$HERE"
exec python3 -m pytest -q -rxXs "$@"
