# Giggity

Giggity is a Rust-first tmux dashboard for developers who run a lot of local services and containers.

It keeps a system-wide snapshot of what is running on the machine, renders a compact tmux status segment, and provides an interactive popup for inspection and operator actions. The current release is V2: Docker, Podman, nerdctl, Kubernetes pods, compose-stack resources, richer host listener names, `launchd`, and `systemd` are supported today.

## What Giggity Does

Giggity continuously inventories developer-facing workloads across the machine and normalizes them into one view:

- containers from Docker
- containers from Podman
- containers from nerdctl / containerd-backed local setups
- Kubernetes pods from the current `kubectl` context
- synthetic compose-stack resources aggregated from labeled containers
- host processes that are actively listening on TCP ports
- `launchd` jobs on macOS
- `systemd` units on Linux

From that inventory, Giggity gives you:

- a tmux status-bar summary
- an interactive popup dashboard
- recent event memory for state transitions
- runtime-aware logs and control actions
- a config-driven view system for filtering, grouping, sorting, and rendering
- optional probes that can override raw runtime state with health checks

The intent is simple: if you are running databases, APIs, frontends, workers, queues, and local infrastructure, Giggity should tell you what is up, what is unhealthy, and what just broke without making you tab through five different tools.

## Feature Set

### System-wide monitoring

Giggity is not pane-scoped. It is machine-scoped.

It looks across the host and supported runtimes, then publishes a single unified snapshot through a local Unix socket daemon. That means multiple tmux sessions see the same state, and the popup/status line do not need to re-run full collector logic independently.

### Multi-runtime container support

Current runtimes:

- Docker via the Docker API
- Podman via `podman ps --all --format json`
- nerdctl via `nerdctl ps -a --format '{{json .}}'`
- Kubernetes via `kubectl get pods --all-namespaces -o json`

Giggity understands:

- running containers
- exited containers
- crashed containers
- restarting / starting containers
- published ports
- Compose project labels when the runtime exposes them
- namespace metadata from nerdctl when available
- Kubernetes pod phases and container waiting / termination reasons
- compose-stack rollups synthesized across multi-service projects

### Host service discovery

Giggity also tracks non-containerized services by finding TCP listeners with:

- `lsof` first
- `ss` on Linux as a fallback
- `netstat` as a final fallback

This covers the common local-service case where a developer has:

- a `postgres` process on `:5432`
- a Python dev server on `:8000`
- a Node app on `:3000`
- a background worker exposing an admin port

When a PID is available, Giggity enriches the display name with the full process command line from `ps`, so the dashboard can show `node server.js` or `python manage.py runserver` instead of five rows all named `node` or `python`.

### Kubernetes visibility

Giggity V2 adds Kubernetes pod collection through the current `kubectl` context.

Today that includes:

- pod name
- namespace
- phase
- restart counts
- waiting / termination reasons
- optional host ports when declared

The Kubernetes collector is intentionally pod-focused in V2. It does not yet model deployments, services, or rollouts as first-class resources.

### Compose stack resources

Giggity V2 also synthesizes stack-level resources for multi-service compose projects.

That means a compose app can now appear twice in useful ways:

- as its underlying container resources
- as a single stack rollup resource such as `myapp stack`

Stack resources aggregate:

- overall health
- service counts
- service names
- published ports
- compose project identity

Issue summaries prefer the stack rollup over every individual compose member so the tmux segment stays compact.

### Service manager visibility

On supported platforms, Giggity also collects:

- `launchd` services on macOS
- `systemd` units on Linux

This is useful when part of your stack is managed by the OS instead of a container runtime.

### Health states

Every resource is normalized into one of these states:

- `healthy`
- `starting`
- `degraded`
- `crashed`
- `stopped`
- `unknown`

Those states come from runtime/service-manager status first, then can be refined by user-defined probes.

### Health probes

Probes let you turn “the process is running” into “the service is actually healthy.”

Supported probe types:

- TCP probe
- HTTP probe
- command probe

Templates in probe values can reference:

- `{id}`
- `{name}`
- `{project}`
- `{port}`
- `{runtime}`

Examples:

- HTTP health endpoint: `http://127.0.0.1:{port}/health`
- TCP override for a known port
- command assertions against live output

### tmux status bar

Giggity renders a compact tmux segment through `#(...)`.

The segment is:

- theme-aware in tmux mode
- template-driven
- view-aware
- overrideable from tmux options
- summary-first, so it stays useful in narrow status bars

### Interactive popup TUI

The popup is for deeper inspection when the status line says something is wrong.

It supports:

- refresh-driven live inventory
- local filtering
- regrouping
- selection state
- details view
- log view
- confirmation before mutating actions

### Operator actions

Giggity currently supports these quick actions:

- `logs`
- `restart`
- `stop`
- `open-url`
- `copy-port`

Actions are runtime-aware where possible:

- Docker / Podman / nerdctl container logs use the matching runtime CLI
- `systemd` logs use `journalctl`
- restart / stop map to runtime-appropriate commands when supported
- URL opening uses `open` on macOS and `xdg-open` elsewhere
- port copy uses the OS clipboard path when available and falls back safely

### Event memory

Giggity does not only show current state; it also tracks recent transitions.

That matters for:

- host listeners that disappear
- containers that crash and restart
- services that briefly flap and recover

The popup details view can surface those recent events so the user has context for why the dashboard changed.

## Architecture

The codebase is a Cargo workspace with four crates:

- `giggity-core`
  - domain model
  - config loading and validation
  - snapshot protocol
  - state engine
  - view resolution and rendering
- `giggity-collectors`
  - Docker / Podman / nerdctl collection
  - Kubernetes pod collection
  - compose-stack synthesis
  - host listener collection
  - `launchd` / `systemd` collection
- `giggity-daemon`
  - Unix socket daemon
  - poll loop
  - probe application
  - action dispatch
- `giggity-cli`
  - CLI entrypoints
  - tmux-facing render flow
  - popup TUI
  - install-service support

Dependency direction is intentionally one-way:

- `giggity-core` <- `giggity-collectors` <- `giggity-daemon` <- `giggity-cli`

## Installation

### tmux / TPM install

Add Giggity to TPM:

```tmux
set -g @plugin 'jahquan/giggity'
run '~/.tmux/plugins/tpm/tpm'
```

Add the status segment manually:

```tmux
set -g status-right '#(~/.tmux/plugins/giggity/scripts/render.sh)'
bind-key G run-shell '~/.tmux/plugins/giggity/scripts/popup.sh'
```

The tmux plugin loader is intentionally minimal. It does not rewrite `status-left` or `status-right` for you.

### Local build / Cargo

Run directly from source:

```bash
cargo run --package giggity -- query
cargo run --package giggity -- render --format plain
cargo run --package giggity -- popup
```

### Nix

The repository includes a `flake.nix` with:

- `packages.default` for the CLI build
- `devShells.default` with Rust and verification tooling
- `checks` for package and shell validation

Common commands:

```bash
nix develop
nix build
nix run
nix flake check
```

### Plugin bootstrap behavior

The shell wrappers in `scripts/` call `scripts/bootstrap.sh`.

That script:

- checks whether `target/release/giggity` exists
- rebuilds when the workspace is newer than the built binary
- symlinks the release binary into `scripts/giggity`

That means tmux can use a stable script path while the actual binary is rebuilt in place.

## Quick Start

### 1. Render a summary

```bash
giggity render --format plain
```

### 2. Inspect the full resource list

```bash
giggity query
```

JSON output is also available:

```bash
giggity query --json
```

### 3. Open the popup

```bash
giggity popup
```

### 4. Validate config

```bash
giggity config validate
```

### 5. Install the daemon as a user service

```bash
giggity install-service --activate
```

## Runtime Support Matrix

### Containers

| Runtime | How Giggity Collects | Notes |
|---|---|---|
| Docker | Docker API via `bollard` | Supports local Docker Engine / Docker Desktop sockets |
| Podman | `podman ps --all --format json` | Uses the Podman CLI already on `PATH` |
| nerdctl | `nerdctl ps -a --format '{{json .}}'` | Works with containerd-based local setups |
| Kubernetes | `kubectl get pods --all-namespaces -o json` | Uses the current `kubectl` context and models pods as resources |

### Synthetic resources

| Resource | How Giggity Builds It | Notes |
|---|---|---|
| Compose stack | Aggregated from compose-labeled container resources | Only emitted for projects with more than one member container |

### Host and service managers

| Source | How Giggity Collects | Notes |
|---|---|---|
| Host listeners | `lsof`, `ss`, `netstat` | TCP listeners only |
| `launchd` | `launchctl` commands | macOS only |
| `systemd` | `systemctl` / `journalctl` | Linux only |

### Current scope boundary

V2 includes:

- Kubernetes pod collection
- compose stack synthetic resources
- richer host process command-name enrichment

Still out of scope in V2:

- Kubernetes services, deployments, and rollout objects as first-class resources
- compose stack mutating actions
- richer host process ancestry / process-tree grouping

## CLI Reference

### `giggity daemon`

Runs the Unix socket daemon in the foreground.

```bash
giggity daemon
```

Run it in the background:

```bash
giggity daemon --background
```

### `giggity render`

Renders the current view as a status line.

```bash
giggity render --format plain
giggity render --format tmux
giggity render --view ops
```

Tmux scripts use the `tmux` format so theme colors are emitted as tmux formatting sequences.

### `giggity popup`

Launches the interactive popup UI.

```bash
giggity popup
giggity popup --view ops
```

### `giggity query`

Prints the current snapshot as a table or JSON.

```bash
giggity query
giggity query --json
giggity query --view ops
```

### `giggity action`

Runs a resource action.

```bash
giggity action logs --resource docker:abcd1234
giggity action restart --resource docker:abcd1234 --confirm
giggity action stop --resource systemd:postgresql.service --confirm
giggity action open-url --resource host:123
giggity action copy-port --resource docker:abcd1234
```

Mutating actions require `--confirm`.

### `giggity config validate`

Validates config structure and reports warnings such as:

- invalid regex values
- missing named default views
- zero refresh intervals
- zero probe timeouts

### `giggity install-service`

Installs the daemon as a user service.

```bash
giggity install-service
giggity install-service --activate
```

## Popup UX

The popup is intended to be the operational view, not just a bigger copy of the status line.

### Keybindings

- `/` enter filter mode
- `v` switch view
- `g` cycle grouping
- `l` toggle logs
- `r` restart selected resource
- `s` stop selected resource
- `o` open primary URL
- `c` copy primary port
- `Enter` toggle details / logs
- `q` exit

### Popup behavior

- filtering is local to the popup session
- logs are lazy-loaded for the selected resource
- mutating actions require confirmation
- the popup refreshes using the daemon snapshot interval
- local grouping overrides do not permanently rewrite config

## Configuration

### Config file paths

- macOS: `~/Library/Application Support/giggity/config.toml`
- Linux: `~/.config/giggity/config.toml`

If no config file exists, Giggity uses built-in defaults.

### Top-level config

Current top-level config keys:

- `refresh_seconds`
- `startup_grace_seconds`
- `host_event_ttl_seconds`
- `cache_dir`
- `socket_path`
- `default_view`
- `sources`
- `probes`
- `views`

### Source toggles

The `sources` table controls which collectors run:

```toml
[sources]
docker = true
podman = true
nerdctl = true
kubernetes = true
host_listeners = true
launchd = true
systemd = true
```

### Views

Views are the primary customization unit in Giggity.

Each view can define:

- source overrides
- include rules
- exclude rules
- grouping
- sorting
- visible columns
- visible detail fields
- pinned resources
- aliases
- hide patterns
- severity overrides
- status-bar template behavior
- enabled quick actions
- theme colors

Example:

```toml
default_view = "default"

[views.default]
grouping = "severity"
sorting = "severity"
pinned = ["docker:postgres", "docker:redis"]
hide = ["^com\\.apple\\."]

[views.default.aliases]
"docker:my-long-container-id" = "api"

[views.default.severity_overrides]
"host:1234" = "degraded"

[views.default.status_bar]
template = "svc {total} ok {healthy} warn {degraded} down {crashed} stop {stopped} src {collector_warnings} [{issues}]"
separator = " "
max_issue_names = 4
show_empty = false
```

### Grouping modes

Supported grouping modes:

- `severity`
- `runtime`
- `project`
- `namespace`
- `compose_stack`
- `unit_domain`
- `none`

### Sorting modes

Supported sorting modes:

- `severity`
- `name`
- `last_change`
- `runtime`
- `port`

### Columns

Supported table columns:

- `name`
- `runtime`
- `state`
- `project`
- `ports`
- `urls`
- `updated`

### Detail fields

Supported detail sections:

- `labels`
- `metadata`
- `urls`
- `ports`
- `events`

### Quick actions

Views can enable these actions:

- `logs`
- `restart`
- `stop`
- `open_url`
- `copy_port`

### Match rules

Include / exclude rules can match on:

- `runtime`
- `kind`
- `state`
- `name_regex`
- `project_regex`
- `namespace_regex`
- `any_regex`
- `ports`
- `labels`

Example:

```toml
[views.ops]
grouping = "compose_stack"

[[views.ops.include]]
kind = ["compose_stack", "kubernetes_pod"]
state = ["degraded", "crashed", "starting"]

[[views.ops.exclude]]
name_regex = "^com\\.apple\\."
```

### Status bar templates

Supported placeholders:

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

Default template:

```text
svc {total} ok {healthy} warn {degraded} down {crashed} stop {stopped} src {collector_warnings} [{issues}]
```

### Themes

A view theme controls tmux-colored output:

```toml
[views.default.theme]
ok_color = "green"
warn_color = "yellow"
error_color = "red"
text_color = "white"
```

### Probes

Probe blocks are matched the same way views are matched, then executed against matching resources.

Example:

```toml
[[probes]]
name = "api-http"
probe = "http"
name_regex = "^api$"
url = "http://127.0.0.1:{port}/health"
expected_status = 200
timeout_millis = 1000

[[probes]]
name = "postgres-port"
probe = "tcp"
name_regex = "^postgres$"
port = 5432
timeout_millis = 1000

[[probes]]
name = "worker-check"
probe = "command"
name_regex = "^worker$"
program = "sh"
args = ["-lc", "echo ok"]
contains = "ok"
timeout_millis = 1000
```

## tmux Integration

### Minimal setup

```tmux
set -g status-right '#(~/.tmux/plugins/giggity/scripts/render.sh)'
bind-key G run-shell '~/.tmux/plugins/giggity/scripts/popup.sh'
```

### tmux option overrides

The shell wrappers read these tmux globals:

- `@giggity_view`
- `@giggity_refresh_seconds`
- `@giggity_startup_grace_seconds`
- `@giggity_max_issue_names`
- `@giggity_template`
- `@giggity_hide_patterns`
- `@giggity_docker_enabled`
- `@giggity_podman_enabled`
- `@giggity_nerdctl_enabled`
- `@giggity_kubernetes_enabled`
- `@giggity_host_enabled`
- `@giggity_launchd_enabled`
- `@giggity_systemd_enabled`

Example:

```tmux
set -g @giggity_view 'ops'
set -g @giggity_template 'svc {total} down {crashed} [{issues}]'
set -g @giggity_hide_patterns '^com\.apple\.,^port-'
set -g @giggity_max_issue_names 5
set -g @giggity_podman_enabled off
set -g @giggity_kubernetes_enabled on
```

These overrides are intentionally scoped to a focused subset of the config surface so tmux remains simple.

## State Model

Giggity’s state engine differentiates between:

- durable resources such as containers and managed units
- transient host listeners

That distinction matters because host listeners disappear frequently during normal development. Giggity keeps recent host events for a bounded TTL instead of treating every short-lived process like a durable managed service.

## Logs and Actions Behavior

### Logs

Current log behavior:

- Docker: `docker logs --tail`
- Podman: `podman logs --tail`
- nerdctl: `nerdctl logs --tail`
- Kubernetes: `kubectl logs -n <namespace> <pod> --all-containers=true --tail`
- `systemd`: `journalctl -n --no-pager -u ...`
- compose-stack resources: logs unavailable
- other resource types: logs unavailable

### Restart / stop

Current action support is runtime-specific and intentionally conservative.

Today that means:

- Docker / Podman / nerdctl containers support restart and stop
- `systemd` and `launchd` units support restart and stop
- ad-hoc host listeners support stop via `kill -TERM`
- Kubernetes pods support stop via `kubectl delete pod ... --wait=false`
- Kubernetes pods do not support restart as a first-class action in V2
- compose-stack resources do not support restart / stop actions in V2

Unsupported targets return safe errors instead of pretending the action succeeded.

### Open URL

`open-url` uses the first inferred or runtime-derived URL on the resource.

### Copy port

`copy-port` uses the first published or discovered host port on the resource.

## Runtime Fixtures and Smoke Tests

The repo includes runtime-neutral fixtures for real end-to-end verification.

Relevant files:

- `examples/containers/basic-http/Dockerfile`
- `examples/containers/basic-http/index.html`
- `examples/containers/basic-http/health`
- `examples/containers/compose.yaml`
- `scripts/runtime-fixture.sh`
- `scripts/smoke-runtime.sh`

### Fixture resources

The fixture flow builds and runs:

- `giggity-fixture-web`
- `giggity-fixture-worker`
- `giggity-fixture-crash`

Expected states:

- web: `healthy`
- worker: `healthy`
- crash: `crashed`

### Bring fixtures up manually

```bash
./scripts/runtime-fixture.sh docker up
./scripts/runtime-fixture.sh podman up
./scripts/runtime-fixture.sh nerdctl up
```

Tear them down:

```bash
./scripts/runtime-fixture.sh docker down
```

### Run an end-to-end smoke test

```bash
./scripts/smoke-runtime.sh docker
./scripts/smoke-runtime.sh podman
./scripts/smoke-runtime.sh nerdctl
```

These scripts:

- build the local fixture image
- start the runtime fixtures
- launch a temporary Giggity daemon
- assert that the runtime snapshot contains the expected states

### Non-default sockets

If your runtime is not on the default local socket, set the normal runtime environment first:

```bash
export DOCKER_HOST="unix://$HOME/.colima/default/docker.sock"
./scripts/smoke-runtime.sh docker
```

```bash
export CONTAINER_HOST="unix:///path/to/podman.sock"
./scripts/smoke-runtime.sh podman
```

## Verification and Quality

The repo is built with a verification-first workflow.

Typical verification commands:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test -q
cargo llvm-cov --workspace --branch --summary-only
shellcheck giggity.tmux scripts/*.sh
shfmt -d giggity.tmux scripts/*.sh
```

There is also mutation-test coverage for critical daemon logic.

## Troubleshooting

### Status line says `giggity idle`

Possible causes:

- all sources are disabled
- the selected view filtered out everything
- no supported runtime or listener is currently visible
- the daemon has not populated its first snapshot yet

### Collector warnings are non-zero

That usually means:

- a runtime CLI is not installed
- the runtime daemon/socket is not reachable
- a host command like `lsof` or `netstat` failed
- the selected platform does not support a given manager source

Use:

```bash
giggity query --json
giggity config validate
```

to inspect the current snapshot and config warnings directly.

### tmux plugin is not rebuilding

The wrappers rebuild automatically when:

- `target/release/giggity` does not exist
- `Cargo.toml` is newer than the binary
- any file under `crates/` is newer than the binary

You can also force a rebuild manually:

```bash
cargo build --release --package giggity
```

### Popup opens but actions fail

That usually means the underlying runtime action failed, not the popup itself. Check:

- runtime socket availability
- CLI availability on `PATH`
- permissions for the target runtime or service manager

## Roadmap

Current V2 scope is intentionally focused on:

- local container runtimes
- host listeners
- OS service managers
- tmux rendering and popup inspection

Current V2 ships the biggest next-step additions already:

- Kubernetes pod collection
- richer host command-name enrichment
- compose stack-level synthetic resources

Likely next high-value additions after V2:

- Kubernetes services / deployments / rollout awareness
- compose stack-level mutating actions
- live event streaming from Docker / Podman / Kubernetes
- richer host process ancestry and dependency views

Those are roadmap items, not current documented behavior.

## License

Add the license that matches how you want to publish Giggity before making the repository public.
