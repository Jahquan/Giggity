#!/usr/bin/env bash

set -euo pipefail

if [[ $# -lt 2 ]]; then
	echo "usage: $0 <docker|podman|nerdctl> <up|down|status>" >&2
	exit 1
fi

runtime="$1"
action="$2"

ROOT_DIR="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
FIXTURE_DIR="$ROOT_DIR/examples/containers/basic-http"
IMAGE_NAME="giggity/basic-http:local"
WORKER_IMAGE="busybox:1.36.1"
WEB_CONTAINER="giggity-fixture-web"
WORKER_CONTAINER="giggity-fixture-worker"
CRASH_CONTAINER="giggity-fixture-crash"

require_runtime() {
	if ! command -v "$runtime" >/dev/null 2>&1; then
		echo "runtime not found on PATH: $runtime" >&2
		exit 1
	fi
}

require_runtime_ready() {
	for _ in $(seq 1 20); do
		case "$runtime" in
		docker | nerdctl)
			if run_runtime version >/dev/null 2>&1; then
				return 0
			fi
			;;
		podman)
			if run_runtime info >/dev/null 2>&1; then
				return 0
			fi
			;;
		esac
		sleep 1
	done

	echo "runtime is installed but not reachable: $runtime" >&2
	if [[ "$runtime" == "docker" && -n "${DOCKER_HOST:-}" ]]; then
		echo "DOCKER_HOST=$DOCKER_HOST" >&2
	fi
	exit 1
}

run_runtime() {
	"$runtime" "$@"
}

remove_container() {
	run_runtime rm -f "$1" >/dev/null 2>&1 || true
}

build_image() {
	(
		cd "$FIXTURE_DIR"

		if [[ "$runtime" == "podman" ]] && run_runtime image exists "$WORKER_IMAGE" >/dev/null 2>&1; then
			run_runtime build --pull=never -t "$IMAGE_NAME" .
			return
		fi

		if [[ "$runtime" == "docker" ]]; then
			DOCKER_BUILDKIT=0 run_runtime build -t "$IMAGE_NAME" .
		else
			run_runtime build -t "$IMAGE_NAME" .
		fi
	)
}

bring_up() {
	build_image

	remove_container "$WEB_CONTAINER"
	remove_container "$WORKER_CONTAINER"
	remove_container "$CRASH_CONTAINER"

	run_runtime run -d \
		--name "$WEB_CONTAINER" \
		--label com.giggity.fixture=true \
		--label com.giggity.fixture.role=web \
		-p 18081:8080 \
		"$IMAGE_NAME" >/dev/null

	run_runtime run -d \
		--name "$WORKER_CONTAINER" \
		--label com.giggity.fixture=true \
		--label com.giggity.fixture.role=worker \
		"$WORKER_IMAGE" sh -c "sleep 3600" >/dev/null

	run_runtime run \
		--name "$CRASH_CONTAINER" \
		--label com.giggity.fixture=true \
		--label com.giggity.fixture.role=crash \
		"$IMAGE_NAME" sh -c "exit 2" >/dev/null 2>&1 || true

	run_runtime ps -a --filter "name=giggity-fixture"
}

bring_down() {
	remove_container "$WEB_CONTAINER"
	remove_container "$WORKER_CONTAINER"
	remove_container "$CRASH_CONTAINER"
	run_runtime ps -a --filter "name=giggity-fixture"
}

show_status() {
	run_runtime ps -a --filter "name=giggity-fixture"
}

require_runtime
require_runtime_ready

case "$action" in
up)
	bring_up
	;;
down)
	bring_down
	;;
status)
	show_status
	;;
*)
	echo "unknown action: $action" >&2
	exit 1
	;;
esac
