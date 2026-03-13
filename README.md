# Giggity

`giggity` is a Rust-first, system-wide tmux dashboard for developers. It inventories containers and local services, keeps a typed daemon snapshot on a Unix socket, renders a compact status-bar summary, and provides an interactive popup for inspecting, filtering, and acting on services.

## Sources

- Docker containers via the Docker API
- Podman containers via `podman ps --format json`
- nerdctl containers via `nerdctl ps --format '{{json .}}'`
- Host listeners via `lsof`, `ss`, or `netstat`
- `launchd` services on macOS
- `systemd` services on Linux

## Install

With TPM:

```tmux
set -g @plugin 'jahquan/giggity'
run '~/.tmux/plugins/tpm/tpm'
```

Add the segment manually:

```tmux
set -g status-right '#(~/.tmux/plugins/giggity/scripts/render.sh)'
bind-key G run-shell '~/.tmux/plugins/giggity/scripts/popup.sh'
```

The first render will build the Rust binary with `cargo build --release`.

## Nix

The repo includes a `flake.nix` with:

- `packages.default` for the `giggity` CLI
- `devShells.default` with Rust, coverage, mutation, and shell-lint tooling
- `checks` for the package build plus shell script lint/format checks

Common commands:

```bash
nix develop
nix build
nix run
nix flake check
```

## CLI

```bash
cargo run --package giggity -- query
cargo run --package giggity -- render --format plain
cargo run --package giggity -- popup
cargo run --package giggity -- action logs --resource docker:abcd1234
cargo run --package giggity -- config validate
cargo run --package giggity -- install-service --activate
```

## Runtime Fixtures

The repo includes a minimal local container fixture that works with Docker, Podman, and nerdctl:

- `examples/containers/basic-http/Dockerfile`
- `examples/containers/compose.yaml`
- `scripts/runtime-fixture.sh`
- `scripts/smoke-runtime.sh`

Bring the fixtures up under a specific runtime:

```bash
./scripts/runtime-fixture.sh docker up
./scripts/runtime-fixture.sh podman up
./scripts/runtime-fixture.sh nerdctl up
```

Run an end-to-end `giggity` smoke test for a runtime:

```bash
./scripts/smoke-runtime.sh docker
```

If your runtime uses a non-default socket, set the normal client environment first. Examples:

```bash
export DOCKER_HOST="unix://$HOME/.colima/<profile>/docker.sock"
./scripts/smoke-runtime.sh docker

export CONTAINER_HOST="unix:///path/to/podman-api.sock"
./scripts/smoke-runtime.sh podman
```

The fixture and smoke scripts intentionally use the runtime CLI already on `PATH`, so `docker`, `podman`, or `nerdctl` can be backed by any standard local setup as long as that CLI is already configured to reach the intended daemon.

The smoke test builds a local image, starts:

- `giggity-fixture-web` on `127.0.0.1:18081`
- `giggity-fixture-worker` without published ports
- `giggity-fixture-crash` with a non-zero exit code

Then it launches a temporary `giggity` daemon and verifies that the selected runtime reports `healthy`, `healthy`, and `crashed` respectively.

## Config

Primary config path:

- macOS: `~/Library/Application Support/giggity/config.toml`
- Linux: `~/.config/giggity/config.toml`

Example:

```toml
refresh_seconds = 2
default_view = "default"

[sources]
docker = true
podman = true
nerdctl = true
host_listeners = true
launchd = true
systemd = true

[views.default]
grouping = "severity"
sorting = "severity"
hide = ["^com\\.apple\\."]
pinned = ["docker:postgres", "docker:redis"]

[views.default.status_bar]
template = "svc {total} ok {healthy} warn {degraded} down {crashed} stop {stopped} src {collector_warnings} [{issues}]"
max_issue_names = 4
show_empty = false

[[probes]]
name = "api-http"
probe = "http"
name_regex = "^api$"
url = "http://127.0.0.1:{port}/health"
expected_status = 200

[[probes]]
name = "db-port"
probe = "tcp"
name_regex = "^postgres$"
port = 5432
```

Tmux can override a focused subset at runtime:

```tmux
set -g @giggity_view ops
set -g @giggity_max_issue_names 5
set -g @giggity_template 'svc {total} down {crashed} [{issues}]'
set -g @giggity_hide_patterns '^com\.apple\.,^port-'
```

Status-bar templates support these placeholders:

- `{total}`
- `{healthy}`
- `{starting}`
- `{degraded}`
- `{crashed}`
- `{stopped}`
- `{unknown}`
- `{collector_warnings}`
- `{warning_sources}`
- `{issues}`

## Popup

Popup keybindings:

- `/` filter
- `v` switch view
- `g` regroup
- `l` toggle logs
- `r` restart selected resource
- `s` stop selected resource
- `o` open primary URL
- `c` copy primary port
- `Enter` toggle details/logs
- `q` exit
