#!/usr/bin/env bash
# Builds the example server with the mini-chat feature set and runs the black-box suite.
# SKIP_BUILD=1 reuses an existing target/debug/cf-gears-example-server.
# Extra arguments are passed to pytest (e.g. `-k reaction`).
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "${HERE}/../.." && pwd)"
cd "${REPO}"

if [[ "${SKIP_BUILD:-0}" != "1" ]]; then
    cargo build --bin cf-gears-example-server --no-default-features \
        --features mini-chat,static-authn,static-authz,single-tenant,static-credstore
fi

exec python3 -m pytest "${HERE}" -v -p no:cacheprovider "$@"
