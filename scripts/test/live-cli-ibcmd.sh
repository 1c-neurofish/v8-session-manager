#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

if [[ -z "${V8TR_IBCMD_REAL_CONFIG:-}" ]]; then
    echo "SKIPPED: V8TR_IBCMD_REAL_CONFIG is not set."
    echo "Set a dedicated live config with format=DESIGNER, builder=IBCMD and a file-based infobase."
    exit 0
fi

export V8TR_DESIGNER_REAL_CONFIG="$V8TR_IBCMD_REAL_CONFIG"
export V8TR_LIVE_CLI_OUTPUT_ROOT="${V8TR_LIVE_CLI_OUTPUT_ROOT:-$ROOT_DIR/target/manual-tests/live-cli-ibcmd}"

bash "$ROOT_DIR/scripts/test/live-cli-fixture.sh"
