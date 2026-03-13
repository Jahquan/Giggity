#!/usr/bin/env bash

set -euo pipefail

CURRENT_DIR="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"

"$CURRENT_DIR/scripts/bootstrap.sh" >/dev/null 2>&1 || true
