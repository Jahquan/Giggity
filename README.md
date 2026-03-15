# Giggity

Giggity is a Rust-first tmux dashboard for developers who run a lot of local services and containers.

It keeps a system-wide snapshot of what is running on the machine, renders a compact tmux status segment, and provides an interactive popup for inspection and operator actions.

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
- standalone health probes (HTTP, TCP, gRPC, command)

From that inventory, Giggity gives you:

- a tmux status-bar summary with condensed and per-runtime modes
- an interactive popup dashboard with group headers, state filtering, and bookmarks
- recent event memory for state transitions with flapping detection
- runtime-aware logs, control actions, and force kill
- streaming logs and events over the daemon socket
- desktop notifications on macOS with Slack and Telegram integration
- a config-driven view system for filtering, grouping, sorting, and rendering
- customizable popup sizing and per-resource jump-to

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

### Container data enrichment

Giggity enriches container metadata with:

- **Image info** — image name and tag extracted into metadata
- **Network info** — network names and IPs
- **Volume/mount info** — mount points as `source:destination` pairs
- **Environment variables** — extracted with automatic redaction of sensitive keys (PASSWORD, SECRET, TOKEN, KEY, API_KEY)
- **Uptime display** — human-readable uptime since last state change

### Host service discovery

Giggity also tracks non-containerized services by finding TCP listeners with:

- `lsof` first
- `ss` on Linux as a fallback
- `netstat` as a final fallback

When a PID is available, Giggity enriches the display name with the full process command line from `ps`, so the dashboard can show `node server.js` or `python manage.py runserver` instead of five rows all named `node` or `python`.

### Kubernetes visibility

Giggity includes Kubernetes pod collection through the current `kubectl` context:

- pod name
- namespace
- phase
- restart counts
- waiting / termination reasons
- optional host ports when declared

The Kubernetes collector is intentionally pod-focused. It does not yet model deployments, services, or rollouts as first-class resources.

### Compose stack resources

Giggity synthesizes stack-level resources for multi-service compose projects:

- as its underlying container resources
- as a single stack rollup resource such as `myapp stack`

Stack resources aggregate:

- overall health
- service counts
- service names
- published ports
- compose project identity

### Service manager visibility

On supported platforms, Giggity also collects:

- `launchd` services on macOS
- `systemd` units on Linux

### Health states

Every resource is normalized into one of these states:

- `healthy`
- `starting`
- `degraded`
- `crashed`
- `stopped`
- `unknown`

Those states come from runtime/service-manager status first, then can be refined by user-defined probes.

### State intelligence

Giggity includes automatic state analysis:

- **Port conflict detection** — flags resources listening on the same port
- **Restart flapping detection** — marks resources that crash/restart more than 3 times in 10 minutes
- **Uptime tracking** — `state_since` timestamp on every resource with human-readable display

### Health probes

Probes let you turn "the process is running" into "the service is actually healthy."

Giggity supports standalone probe resources alongside the existing probe-as-override system:

- **HTTP probes** — GET a URL, check status code, measure latency
- **TCP probes** — connect with timeout
- **gRPC probes** — TCP connect check (no full gRPC dependency)
- **Command probes** — run a command, check exit status and output

Probe features:

- configurable retry count with exponential backoff
- latency thresholds — warn and critical levels that override health state
- each standalone probe appears as its own resource in the dashboard
- metadata includes `latency_ms`, `last_check`, `probe_type`, and error details

Templates in probe values can reference `{id}`, `{name}`, `{project}`, `{port}`, `{runtime}`.

### Protocol streaming

Giggity supports streaming over the daemon socket:

- **StreamLogs** — tail logs for a resource with real-time updates
- **StreamEvents** — subscribe to state transition events for a view
- **CloseStream** — gracefully terminate a stream

### Notifications

- **Tmux status flash** — status bar flashes red when a resource crashes (30-second window)
- **Desktop notifications** — macOS `osascript` notifications on crash/recovery (configurable)
- **Notification muting** — mute all notifications for a duration via `m` key or API

### Integrations

Giggity supports two notification integrations:

- **Slack** — webhook-based crash/recovery alerts
- **Telegram** — bot API crash/recovery messages
- **Rate limiting** — configurable per-resource cooldown (default 5 minutes) to prevent spam

### tmux status bar

Giggity renders a compact tmux segment through `#(...)`.

Status bar features:

- theme-aware in tmux mode
- template-driven
- view-aware
- overrideable from tmux options
- **condensed mode** — `ok:5 warn:1 err:2` instead of resource names
- **per-runtime counts** — `docker:3 k8s:2 host:1`
- **crash blink** — blink attribute for first 60 seconds after a crash
- summary-first, so it stays useful in narrow status bars

### Interactive popup TUI

The popup is for deeper inspection when the status line says something is wrong.

Popup features:

- refresh-driven live inventory
- **grouped resource list** — visual group headers (yellow, bold) when grouping is active
- **state filtering** — cycle through All/Healthy/Crashed/Stopped/Degraded/Starting/Unknown with `F`
- **color-coded states** — green healthy, red crashed, yellow degraded, gray stopped, cyan starting
- local text filtering with **filter history** (last 10, navigate with Up/Down)
- **5 detail tabs** — Info, Logs, Events, Labels, Metadata (switch with 1-5)
- **bookmarks** — star resources with `b`, persisted to `~/.local/state/giggity/bookmarks.json`
- **resource diff** — `d` key shows what changed since last refresh
- **help overlay** — `?` key shows all keybindings
- **mouse support** — click to select, scroll wheel to navigate
- sort cycling
- confirmation before mutating actions
- **customizable size** — configurable via config, CLI args, or tmux options
- **`--resource` flag** — auto-select a resource on popup open

### Operator actions

Giggity supports these quick actions:

- `logs`
- `restart`
- `stop`
- `force-kill`
- `open-url`
- `copy-port`
- `copy-id`
- `bulk-restart`

Actions are runtime-aware:

- Docker / Podman / nerdctl containers support restart, stop, force kill, and logs
- `systemd` and `launchd` units support restart, stop, and force kill
- Kubernetes pods support restart (via `kubectl delete pod`), force kill (with `--grace-period=0 --force`), and logs
- Compose stacks support restart, stop, and kill via `docker compose -p <project>`
- ad-hoc host listeners support stop via `kill -TERM` and force kill via `kill -9`
- Probe resources do not support actions (safe error returned)
- URL opening uses `open` on macOS and `xdg-open` elsewhere
- port copy uses the OS clipboard path when available and falls back safely

### Event memory

Giggity does not only show current state; it also tracks recent transitions.

That matters for:

- host listeners that disappear
- containers that crash and restart
- services that briefly flap and recover

The popup details view can surface those recent events so the user has context for why the dashboard changed.

### Config management

Giggity also supports:

- **Config export** — `ExportConfig` request returns effective config as TOML
- **Config hot-reload** — status bar flash on config file changes
- **Enhanced validation** — checks for invalid runtimes, duplicate views, bad regex, unknown fields
- **Watch mode** — `--watch` flag for continuous status line rendering

## Architecture

The codebase is a Cargo workspace with four crates:

- `giggity-core`
  - domain model
  - config loading and validation
  - snapshot protocol with streaming support
  - state engine with port conflict and flapping detection
  - view resolution and rendering
- `giggity-collectors`
  - Docker / Podman / nerdctl collection with image/network/volume/env enrichment
  - Kubernetes pod collection
  - compose-stack synthesis
  - host listener collection
  - `launchd` / `systemd` collection
  - standalone health probes (HTTP, TCP, gRPC, command)
- `giggity-daemon`
  - Unix socket daemon
  - poll loop with probe collection
  - action dispatch (restart, stop, force kill, bulk restart)
  - streaming (logs, events)
  - notification dispatch (desktop, Slack, Telegram) with rate limiting
  - config hot-reload
- `giggity-cli`
  - CLI entrypoints
  - tmux-facing render flow
  - popup TUI with tabs, bookmarks, diff, help, state filter
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

## Quick Start

### 1. Render a summary

```bash
giggity render --format plain
```

### 2. Inspect the full resource list

```bash
giggity query
giggity query --json
```

### 3. Open the popup

```bash
giggity popup
giggity popup --view ops
giggity popup --resource docker:web
giggity popup --width 90% --height 70%
```

### 4. Watch mode

```bash
giggity render --format plain --watch
```

### 5. Validate config

```bash
giggity config validate
```

### 6. Install the daemon as a user service

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

### Health probes

| Probe type | How Giggity Checks | Notes |
|---|---|---|
| HTTP | `GET` with status code check | Supports latency thresholds and retries |
| TCP | `TcpStream::connect` with timeout | Simple connectivity check |
| gRPC | TCP connect check | No full gRPC dependency |
| Command | Subprocess with exit code / output check | Shell command execution |

## CLI Reference

### `giggity daemon`

Runs the Unix socket daemon in the foreground.

```bash
giggity daemon
giggity daemon --background
```

### `giggity render`

Renders the current view as a status line.

```bash
giggity render --format plain
giggity render --format tmux
giggity render --view ops
giggity render --format plain --watch
```

### `giggity popup`

Launches the interactive popup UI.

```bash
giggity popup
giggity popup --view ops
giggity popup --resource docker:web
giggity popup --width 90% --height 60%
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
giggity action force-kill --resource docker:abcd1234 --confirm
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
- duplicate view names
- unknown fields

### `giggity install-service`

Installs the daemon as a user service.

```bash
giggity install-service
giggity install-service --activate
```

## Popup UX

The popup is intended to be the operational view, not just a bigger copy of the status line.

### Keybindings

| Key | Action |
|---|---|
| `q` / `Esc` | quit |
| `/` | enter filter mode (Up/Down for history) |
| `F` | cycle state filter (all/healthy/crashed/stopped/degraded/starting/unknown) |
| `v` | switch view |
| `g` | cycle grouping (severity/runtime/project/namespace/compose_stack/unit_domain/none) |
| `S` | cycle sort key |
| `l` | toggle logs |
| `r` | restart selected resource (confirm) |
| `s` | stop selected resource (confirm) |
| `K` | force kill selected resource (confirm) |
| `o` | open primary URL |
| `c` | copy primary port |
| `y` / `Y` | copy resource id / name |
| `b` | toggle bookmark |
| `d` | toggle resource diff view |
| `m` / `M` | mute / unmute notifications |
| `1`-`5` | switch detail tab (Info/Logs/Events/Labels/Metadata) |
| `Enter` | toggle details / logs |
| `?` | help overlay |

### Popup behavior

- filtering is local to the popup session
- state filter restricts the list to a single health state
- grouping adds visual section headers to the resource list
- logs are lazy-loaded for the selected resource
- mutating actions require confirmation
- the popup refreshes using the daemon snapshot interval
- local grouping and sorting overrides do not permanently rewrite config
- bookmarks are persisted to `~/.local/state/giggity/bookmarks.json`
- resource diff shows state, port, label, and metadata changes since last refresh
- mouse click to select, scroll wheel to navigate

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
- `notifications`
- `integrations`
- `popup`
- `bookmarks`

### Source toggles

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

### Notifications

```toml
[notifications]
enabled = true
on_crash = true
on_recovery = true
```

### Integrations

```toml
[integrations]
cooldown_secs = 300

[integrations.slack]
webhook_url = "https://hooks.slack.com/services/..."
on_crash = true
on_recovery = false

[integrations.telegram]
bot_token = "123456:ABC..."
chat_id = "-100123456789"
on_crash = true
on_recovery = false
```

### Popup configuration

```toml
[popup]
width = "80%"
height = "80%"
```

Also configurable via tmux options `@giggity_popup_width` and `@giggity_popup_height`.

### Standalone probes

```toml
[[probes]]
name = "api-health"
probe_type = "http"
target = "http://localhost:8080/health"
expected_status = 200
interval_secs = 30
timeout_secs = 5
retries = 2
backoff_secs = 1
warn_latency_ms = 500
critical_latency_ms = 2000
```

### View-scoped probes

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
condensed = false
show_runtime_counts = false
```

### Grouping modes

- `severity`
- `runtime`
- `project`
- `namespace`
- `compose_stack`
- `unit_domain`
- `none`

### Sorting modes

- `severity`
- `name`
- `last_change`
- `runtime`
- `port`

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

- `{total}`, `{healthy}`, `{starting}`, `{degraded}`, `{crashed}`, `{stopped}`, `{unknown}`
- `{collector_warnings}`, `{warning_sources}`, `{issues}`

Default template:

```text
svc {total} ok {healthy} warn {degraded} down {crashed} stop {stopped} src {collector_warnings} [{issues}]
```

### Themes

```toml
[views.default.theme]
ok_color = "green"
warn_color = "yellow"
error_color = "red"
text_color = "white"
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
- `@giggity_popup_width`
- `@giggity_popup_height`

Example:

```tmux
set -g @giggity_view 'ops'
set -g @giggity_template 'svc {total} down {crashed} [{issues}]'
set -g @giggity_hide_patterns '^com\.apple\.,^port-'
set -g @giggity_popup_width '90%'
set -g @giggity_popup_height '70%'
```

## State Model

Giggity's state engine differentiates between:

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

### Restart / stop / force kill

Actions are runtime-specific:

| Runtime | Restart | Stop | Force Kill |
|---|---|---|---|
| Docker | `docker restart` | `docker stop` | `docker kill` |
| Podman | `podman restart` | `podman stop` | `podman kill` |
| nerdctl | `nerdctl restart` | `nerdctl stop` | `nerdctl kill` |
| Kubernetes | `kubectl delete pod --wait=false` | not supported | `kubectl delete pod --grace-period=0 --force` |
| Compose stack | `docker compose -p <project> restart` | `docker compose -p <project> stop` | `docker compose -p <project> kill` |
| `systemd` | `systemctl restart` | `systemctl stop` | `systemctl kill --signal=SIGKILL` |
| `launchd` | `launchctl kickstart -k` | `launchctl bootout` | `launchctl kill 9` |
| Host process | not supported | `kill -TERM` | `kill -9` |
| Probe | not supported | not supported | not supported |

Unsupported targets return safe errors instead of pretending the action succeeded.

## Verification and Quality

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

## License

MIT
