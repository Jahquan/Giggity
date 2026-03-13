#!/usr/bin/env bash

set -euo pipefail

if [[ $# -lt 1 ]]; then
	echo "usage: $0 <docker|podman|nerdctl>" >&2
	exit 1
fi

runtime="$1"
ROOT_DIR="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
FIXTURE_SCRIPT="$ROOT_DIR/scripts/runtime-fixture.sh"
BIN_BOOTSTRAP="$ROOT_DIR/scripts/bootstrap.sh"
BIN="$ROOT_DIR/scripts/giggity"
TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/giggity-smoke.XXXXXX")"
CONFIG_PATH="$TMP_DIR/config.toml"
QUERY_PATH="$TMP_DIR/query.json"
SOCKET_PATH="$TMP_DIR/giggity.sock"
CACHE_DIR="$TMP_DIR/cache"
DAEMON_PID=""

cleanup() {
	if [[ -n "$DAEMON_PID" ]]; then
		kill "$DAEMON_PID" >/dev/null 2>&1 || true
		wait "$DAEMON_PID" >/dev/null 2>&1 || true
	fi
	"$FIXTURE_SCRIPT" "$runtime" down >/dev/null 2>&1 || true
	rm -rf "$TMP_DIR"
}

trap cleanup EXIT

if ! command -v "$runtime" >/dev/null 2>&1; then
	echo "runtime not found on PATH: $runtime" >&2
	exit 1
fi

"$FIXTURE_SCRIPT" "$runtime" up
"$BIN_BOOTSTRAP" >/dev/null

cat >"$CONFIG_PATH" <<EOF
refresh_seconds = 1
cache_dir = "$CACHE_DIR"
socket_path = "$SOCKET_PATH"

[sources]
docker = $([[ "$runtime" == "docker" ]] && echo true || echo false)
podman = $([[ "$runtime" == "podman" ]] && echo true || echo false)
nerdctl = $([[ "$runtime" == "nerdctl" ]] && echo true || echo false)
host_listeners = false
launchd = false
systemd = false
EOF

"$BIN" daemon --config "$CONFIG_PATH" >"$TMP_DIR/daemon.log" 2>&1 &
DAEMON_PID="$!"

for _ in $(seq 1 20); do
	if "$BIN" --config "$CONFIG_PATH" query --json >"$QUERY_PATH" 2>"$TMP_DIR/query.err"; then
		if python3 - "$QUERY_PATH" "$runtime" <<'PY'; then
import json
import sys

query_path, runtime = sys.argv[1], sys.argv[2]
with open(query_path, "r", encoding="utf-8") as handle:
    payload = json.load(handle)

resources = [resource for resource in payload["resources"] if resource["runtime"] == runtime]
names = {resource["name"]: resource["state"] for resource in resources}
if (
    names.get("giggity-fixture-web") == "healthy"
    and names.get("giggity-fixture-worker") == "healthy"
    and names.get("giggity-fixture-crash") == "crashed"
):
    raise SystemExit(0)
raise SystemExit(1)
PY
			break
		fi
	fi
	sleep 1
done

python3 - "$QUERY_PATH" "$runtime" <<'PY'
import json
import sys

query_path, runtime = sys.argv[1], sys.argv[2]
with open(query_path, "r", encoding="utf-8") as handle:
    payload = json.load(handle)

resources = [resource for resource in payload["resources"] if resource["runtime"] == runtime]
names = {resource["name"]: resource["state"] for resource in resources}

if names.get("giggity-fixture-web") != "healthy":
    raise SystemExit(f"missing healthy web fixture in {names}")
if names.get("giggity-fixture-worker") != "healthy":
    raise SystemExit(f"missing healthy worker fixture in {names}")
if names.get("giggity-fixture-crash") != "crashed":
    raise SystemExit(f"missing crashed fixture in {names}")

print(json.dumps({"runtime": runtime, "resources": resources}, indent=2))
PY

"$BIN" --config "$CONFIG_PATH" render --format plain
