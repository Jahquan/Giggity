#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
"$ROOT_DIR/scripts/bootstrap.sh" >/dev/null 2>&1
BIN="$ROOT_DIR/scripts/giggity"

tmux_opt() {
	tmux show-option -gqv "$1" 2>/dev/null || true
}

args=()

view="$(tmux_opt '@giggity_view')"
if [[ -n "$view" ]]; then
	args+=(--view "$view")
fi

for key in refresh_seconds startup_grace_seconds max_issue_names template hide_patterns; do
	value="$(tmux_opt "@giggity_${key}")"
	if [[ -n "$value" ]]; then
		args+=(--tmux-option "${key}=${value}")
	fi
done

for key in docker_enabled podman_enabled nerdctl_enabled host_enabled launchd_enabled systemd_enabled; do
	value="$(tmux_opt "@giggity_${key}")"
	if [[ -n "$value" ]]; then
		args+=(--tmux-option "${key}=${value}")
	fi
done

exec "$BIN" render --format tmux "${args[@]}"
