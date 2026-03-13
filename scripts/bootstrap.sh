#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET_BIN="$ROOT_DIR/target/release/giggity"
LINK_BIN="$ROOT_DIR/scripts/giggity"

if [[ ! -x "$TARGET_BIN" || "$ROOT_DIR/Cargo.toml" -nt "$TARGET_BIN" ]]; then
	(cd "$ROOT_DIR" && cargo build --release --package giggity)
elif find "$ROOT_DIR/crates" -type f -newer "$TARGET_BIN" -print -quit | grep -q .; then
	(cd "$ROOT_DIR" && cargo build --release --package giggity)
fi

ln -sf "$TARGET_BIN" "$LINK_BIN"
