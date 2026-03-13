#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
"$ROOT_DIR/scripts/bootstrap.sh" >/dev/null 2>&1
BIN="$ROOT_DIR/scripts/giggity"

tmux_opt() {
	tmux show-option -gqv "$1" 2>/dev/null || true
}

view="$(tmux_opt '@giggity_view')"
popup_args=()
if [[ -n "$view" ]]; then
	popup_args+=(--view "$view")
fi

popup_command="$BIN popup"
for arg in "${popup_args[@]}"; do
	popup_command+=" $(printf '%q' "$arg")"
done

exec tmux display-popup -E "$popup_command"
