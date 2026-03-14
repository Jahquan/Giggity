use std::collections::{BTreeMap, BTreeSet};

use bollard::API_DEFAULT_VERSION;
use bollard::Docker;
use bollard::query_parameters::ListContainersOptions;
use giggity_core::config::Config;
use giggity_core::model::{
    CollectorWarning, HealthState, PortBinding, ResourceKind, ResourceRecord, RuntimeKind,
    guess_local_urls_for_ports,
};

use crate::CollectionOutput;
use crate::command::run_command;

#[cfg(test)]
use std::path::PathBuf;
#[cfg(test)]
use std::sync::{Mutex, OnceLock};

#[cfg(test)]
static DOCKER_SOCKET_OVERRIDE: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();

pub async fn collect(config: &Config) -> CollectionOutput {
    let mut output = CollectionOutput::default();

    if config.sources.docker {
        match collect_docker().await {
            Ok(records) => output.resources.extend(records),
            Err(error) => output.warnings.push(CollectorWarning {
                source: "docker".into(),
                message: error.to_string(),
            }),
        }
    }

    if config.sources.podman {
        match collect_podman().await {
            Ok(records) => output.resources.extend(records),
            Err(error) => output.warnings.push(CollectorWarning {
                source: "podman".into(),
                message: error.to_string(),
            }),
        }
    }

    if config.sources.nerdctl {
        match collect_nerdctl().await {
            Ok(records) => output.resources.extend(records),
            Err(error) => output.warnings.push(CollectorWarning {
                source: "nerdctl".into(),
                message: error.to_string(),
            }),
        }
    }

    let mut stacks = synthesize_compose_stacks(&output.resources);
    enrich_compose_stacks(&mut stacks).await;
    output.resources.extend(stacks);

    output
}

async fn collect_docker() -> anyhow::Result<Vec<ResourceRecord>> {
    let docker = docker_client()?;
    let containers = docker
        .list_containers(Some(ListContainersOptions {
            all: true,
            ..Default::default()
        }))
        .await?;

    Ok(containers
        .into_iter()
        .map(|container| {
            let id = container.id.unwrap_or_default();
            let labels: BTreeMap<String, String> =
                container.labels.unwrap_or_default().into_iter().collect();
            let name = container
                .names
                .unwrap_or_default()
                .into_iter()
                .next()
                .unwrap_or_else(|| id.chars().take(12).collect())
                .trim_start_matches('/')
                .to_string();
            let project = labels
                .get("com.docker.compose.project")
                .cloned()
                .or_else(|| labels.get("io.podman.compose.project").cloned());
            let ports = container
                .ports
                .unwrap_or_default()
                .into_iter()
                .filter_map(|port| {
                    let host_port = port.public_port?;
                    let container_port = Some(port.private_port);
                    Some(PortBinding {
                        host_ip: port.ip,
                        host_port,
                        container_port,
                        protocol: port
                            .typ
                            .map(|protocol| protocol.to_string())
                            .unwrap_or_else(|| "tcp".into()),
                    })
                })
                .collect::<Vec<_>>();
            let state = docker_state(
                container.state.as_ref().map(|value| value.to_string()),
                container.status.as_deref(),
            );
            let urls = guess_local_urls_for_ports(&ports);
            let mut metadata = BTreeMap::new();
            metadata.insert("container_id".into(), id.clone());
            if let Some(status) = &container.status {
                metadata.insert("status".into(), status.clone());
            }
            ResourceRecord {
                id: format!("docker:{id}"),
                kind: ResourceKind::Container,
                runtime: RuntimeKind::Docker,
                project,
                name,
                state,
                runtime_status: container.status,
                ports,
                labels,
                urls,
                metadata,
                last_changed: chrono::Utc::now(),
            }
        })
        .collect())
}

fn docker_client() -> anyhow::Result<Docker> {
    #[cfg(test)]
    if let Some(path) = docker_socket_override().lock().expect("lock").clone() {
        return Docker::connect_with_unix(
            path.to_str().expect("utf-8 socket path"),
            120,
            API_DEFAULT_VERSION,
        )
        .map_err(anyhow::Error::from);
    }

    if let Ok(path) = std::env::var("GIGGITY_DOCKER_SOCKET")
        && !path.trim().is_empty()
    {
        return Docker::connect_with_unix(&path, 120, API_DEFAULT_VERSION)
            .map_err(anyhow::Error::from);
    }

    if let Ok(host) = std::env::var("DOCKER_HOST")
        && let Some(path) = host.strip_prefix("unix://")
        && !path.trim().is_empty()
    {
        return Docker::connect_with_unix(path, 120, API_DEFAULT_VERSION)
            .map_err(anyhow::Error::from);
    }

    Docker::connect_with_local_defaults().map_err(anyhow::Error::from)
}

async fn collect_podman() -> anyhow::Result<Vec<ResourceRecord>> {
    let output = run_command("containers", "podman", &["ps", "--all", "--format", "json"]).await?;
    let items: Vec<PodmanPsItem> = serde_json::from_str(&output)?;
    Ok(items
        .into_iter()
        .map(|item| podman_record(item, RuntimeKind::Podman))
        .collect())
}

async fn collect_nerdctl() -> anyhow::Result<Vec<ResourceRecord>> {
    let output = run_command(
        "containers",
        "nerdctl",
        &["ps", "-a", "--format", "{{json .}}"],
    )
    .await?;
    let mut records = Vec::new();
    for line in output.lines().filter(|line| !line.trim().is_empty()) {
        let item: NerdctlPsItem = serde_json::from_str(line)?;
        records.push(nerdctl_record(item));
    }
    Ok(records)
}

fn synthesize_compose_stacks(resources: &[ResourceRecord]) -> Vec<ResourceRecord> {
    let mut grouped: BTreeMap<(RuntimeKind, String), Vec<&ResourceRecord>> = BTreeMap::new();

    for resource in resources {
        if resource.kind != ResourceKind::Container {
            continue;
        }
        let Some(project) = resource.compose_project() else {
            continue;
        };
        grouped
            .entry((resource.runtime, project.to_string()))
            .or_default()
            .push(resource);
    }

    grouped
        .into_iter()
        .filter(|(_, members)| members.len() > 1)
        .map(|((runtime, project), members)| compose_stack_record(runtime, &project, &members))
        .collect()
}

async fn enrich_compose_stacks(resources: &mut [ResourceRecord]) {
    for resource in resources {
        let _ = enrich_compose_stack(resource).await;
    }
}

async fn enrich_compose_stack(resource: &mut ResourceRecord) -> anyhow::Result<()> {
    if resource.kind != ResourceKind::ComposeStack {
        return Ok(());
    }
    let Some(project) = resource.compose_project().map(str::to_owned) else {
        return Ok(());
    };

    let Some(mut details) = compose_project_details(resource.runtime, &project).await? else {
        return Ok(());
    };

    resource.state = compose_stack_state(
        details.healthy,
        details.starting,
        details.degraded,
        details.crashed,
        details.stopped,
        details.unknown,
    );
    resource.runtime_status = Some(format!(
        "{}/{} running",
        details.running_count, details.service_count
    ));
    resource
        .metadata
        .insert("compose_running_count".into(), details.running_count.to_string());
    resource
        .metadata
        .insert("compose_service_count".into(), details.service_count.to_string());
    resource.metadata.insert(
        "compose_services".into(),
        details.services.join(","),
    );
    if let Some(status) = details.project_status.take() {
        resource.metadata.insert("compose_status".into(), status);
    }
    if let Some(config_files) = details.config_files.take() {
        resource
            .metadata
            .insert("compose_config_files".into(), config_files);
    }

    Ok(())
}

async fn compose_project_details(
    runtime: RuntimeKind,
    project: &str,
) -> anyhow::Result<Option<ComposeProjectDetails>> {
    let Some(program) = compose_program(runtime) else {
        return Ok(None);
    };
    let ps_output = run_command(
        "containers",
        program,
        &[
            "compose",
            "--project-name",
            project,
            "ps",
            "-a",
            "--format",
            "json",
        ],
    )
    .await?;
    let entries = parse_compose_ps_output(&ps_output)?;
    if entries.is_empty() {
        return Ok(None);
    }

    let mut details = ComposeProjectDetails::default();
    let mut services = BTreeSet::new();
    for entry in &entries {
        let service_name = entry
            .service
            .as_ref()
            .or(entry.name.as_ref())
            .cloned()
            .unwrap_or_else(|| project.to_string());
        services.insert(service_name);
        if entry.state.as_deref() == Some("running") {
            details.running_count += 1;
        }
        match compose_entry_state(runtime, entry) {
            HealthState::Healthy => details.healthy += 1,
            HealthState::Starting => details.starting += 1,
            HealthState::Degraded => details.degraded += 1,
            HealthState::Crashed => details.crashed += 1,
            HealthState::Stopped => details.stopped += 1,
            HealthState::Unknown => details.unknown += 1,
        }
    }
    details.service_count = entries.len();
    details.services = services.into_iter().collect();

    if runtime == RuntimeKind::Docker
        && let Some(project_row) = compose_ls_details(program, project).await?
    {
        details.project_status = project_row.status;
        details.config_files = project_row.config_files;
    }

    Ok(Some(details))
}

async fn compose_ls_details(
    program: &str,
    project: &str,
) -> anyhow::Result<Option<ComposeLsEntry>> {
    let output = run_command("containers", program, &["compose", "ls", "-a", "--format", "json"])
        .await?;
    let entries: Vec<ComposeLsEntry> = serde_json::from_str(&output)?;
    Ok(entries.into_iter().find(|entry| entry.name == project))
}

fn parse_compose_ps_output(output: &str) -> anyhow::Result<Vec<ComposePsEntry>> {
    let trimmed = output.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    if trimmed.starts_with('[') {
        return serde_json::from_str(trimmed).map_err(anyhow::Error::from);
    }

    output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).map_err(anyhow::Error::from))
        .collect()
}

fn compose_entry_state(runtime: RuntimeKind, entry: &ComposePsEntry) -> HealthState {
    let status = entry.status.as_deref();
    match runtime {
        RuntimeKind::Docker => docker_state(entry.state.clone(), status),
        _ => container_state_from_strings(entry.state.as_deref(), status),
    }
}

fn compose_program(runtime: RuntimeKind) -> Option<&'static str> {
    match runtime {
        RuntimeKind::Docker => Some("docker"),
        RuntimeKind::Podman => Some("podman"),
        RuntimeKind::Nerdctl => Some("nerdctl"),
        _ => None,
    }
}

fn compose_stack_record(
    runtime: RuntimeKind,
    project: &str,
    members: &[&ResourceRecord],
) -> ResourceRecord {
    let mut ports = Vec::new();
    let mut seen_ports = BTreeSet::new();
    let mut services = BTreeSet::new();
    let mut labels = BTreeMap::new();
    let mut latest_change = members
        .first()
        .map(|member| member.last_changed)
        .unwrap_or_else(chrono::Utc::now);

    let mut healthy = 0_usize;
    let mut starting = 0_usize;
    let mut degraded = 0_usize;
    let mut crashed = 0_usize;
    let mut stopped = 0_usize;
    let mut unknown = 0_usize;

    for member in members {
        latest_change = latest_change.max(member.last_changed);
        services.insert(member.name.clone());
        for port in &member.ports {
            let key = (
                port.host_ip.clone(),
                port.host_port,
                port.container_port,
                port.protocol.clone(),
            );
            if seen_ports.insert(key) {
                ports.push(port.clone());
            }
        }
        if labels.is_empty() {
            labels = member.labels.clone();
        }
        match member.state {
            HealthState::Healthy => healthy += 1,
            HealthState::Starting => starting += 1,
            HealthState::Degraded => degraded += 1,
            HealthState::Crashed => crashed += 1,
            HealthState::Stopped => stopped += 1,
            HealthState::Unknown => unknown += 1,
        }
    }

    let state = compose_stack_state(healthy, starting, degraded, crashed, stopped, unknown);
    let total = members.len();
    let urls = guess_local_urls_for_ports(&ports);
    let mut metadata = BTreeMap::from([
        ("service_count".into(), total.to_string()),
        ("healthy_count".into(), healthy.to_string()),
        ("starting_count".into(), starting.to_string()),
        ("degraded_count".into(), degraded.to_string()),
        ("crashed_count".into(), crashed.to_string()),
        ("stopped_count".into(), stopped.to_string()),
        ("unknown_count".into(), unknown.to_string()),
        (
            "services".into(),
            services.into_iter().collect::<Vec<_>>().join(","),
        ),
        ("compose_project".into(), project.to_string()),
    ]);
    metadata.insert("runtime".into(), runtime.to_string());

    if !labels.contains_key("com.docker.compose.project")
        && !labels.contains_key("io.podman.compose.project")
    {
        match runtime {
            RuntimeKind::Podman => {
                labels.insert("io.podman.compose.project".into(), project.to_string());
            }
            _ => {
                labels.insert("com.docker.compose.project".into(), project.to_string());
            }
        }
    }

    ResourceRecord {
        id: format!("compose:{runtime}:{project}"),
        kind: ResourceKind::ComposeStack,
        runtime,
        project: Some(project.to_string()),
        name: format!("{project} stack"),
        state,
        runtime_status: Some(format!("{healthy}/{total} healthy")),
        ports,
        labels,
        urls,
        metadata,
        last_changed: latest_change,
    }
}

fn compose_stack_state(
    healthy: usize,
    starting: usize,
    degraded: usize,
    crashed: usize,
    stopped: usize,
    unknown: usize,
) -> HealthState {
    let total = healthy + starting + degraded + crashed + stopped + unknown;
    if total == 0 {
        HealthState::Unknown
    } else if crashed > 0 {
        HealthState::Crashed
    } else if degraded > 0 {
        HealthState::Degraded
    } else if starting > 0 {
        HealthState::Starting
    } else if healthy > 0 {
        if stopped > 0 || unknown > 0 {
            HealthState::Degraded
        } else {
            HealthState::Healthy
        }
    } else if stopped == total {
        HealthState::Stopped
    } else {
        HealthState::Unknown
    }
}

#[derive(Debug, Default)]
struct ComposeProjectDetails {
    healthy: usize,
    starting: usize,
    degraded: usize,
    crashed: usize,
    stopped: usize,
    unknown: usize,
    running_count: usize,
    service_count: usize,
    services: Vec<String>,
    project_status: Option<String>,
    config_files: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct ComposePsEntry {
    #[serde(rename = "Name")]
    #[serde(default)]
    name: Option<String>,
    #[serde(rename = "Service")]
    #[serde(default)]
    service: Option<String>,
    #[serde(rename = "State")]
    #[serde(default)]
    state: Option<String>,
    #[serde(rename = "Status")]
    #[serde(default)]
    status: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct ComposeLsEntry {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Status")]
    #[serde(default)]
    status: Option<String>,
    #[serde(rename = "ConfigFiles")]
    #[serde(default)]
    config_files: Option<String>,
}

fn docker_state(state: Option<String>, status: Option<&str>) -> HealthState {
    match state.as_deref().unwrap_or_default() {
        "running" => {
            let status = status.unwrap_or_default();
            if status.contains("unhealthy") {
                HealthState::Degraded
            } else if status.contains("health: starting") || status.contains("restarting") {
                HealthState::Starting
            } else {
                HealthState::Healthy
            }
        }
        "created" | "restarting" => HealthState::Starting,
        "paused" => HealthState::Degraded,
        "exited" | "dead" => {
            if status.unwrap_or_default().contains("Exited (0)") {
                HealthState::Stopped
            } else {
                HealthState::Crashed
            }
        }
        _ => HealthState::Unknown,
    }
}

fn podman_record(item: PodmanPsItem, runtime: RuntimeKind) -> ResourceRecord {
    let labels = item.labels.unwrap_or_default();
    let project = labels
        .get("com.docker.compose.project")
        .cloned()
        .or_else(|| labels.get("io.podman.compose.project").cloned());
    let id = item.id.clone().unwrap_or_else(|| item.names.join("-"));
    let ports = match item.ports {
        PodmanPorts::Empty => Vec::new(),
        PodmanPorts::Strings(values) => values
            .into_iter()
            .flat_map(|value| parse_port_specs(&value))
            .collect::<Vec<_>>(),
        PodmanPorts::Objects(values) => values
            .into_iter()
            .filter_map(|value| {
                Some(PortBinding {
                    host_ip: value.host_ip.filter(|entry| !entry.is_empty()),
                    host_port: value.host_port?,
                    container_port: value.container_port,
                    protocol: value.protocol.unwrap_or_else(|| "tcp".into()),
                })
            })
            .collect::<Vec<_>>(),
    };
    let mut metadata = BTreeMap::new();
    if let Some(status) = &item.status {
        metadata.insert("status".into(), status.clone());
    }
    ResourceRecord {
        id: format!("{}:{id}", runtime),
        kind: ResourceKind::Container,
        runtime,
        project,
        name: item
            .names
            .first()
            .cloned()
            .unwrap_or_else(|| id.chars().take(12).collect()),
        state: container_state_from_strings(item.state.as_deref(), item.status.as_deref()),
        runtime_status: item.status,
        ports: ports.clone(),
        labels,
        urls: guess_local_urls_for_ports(&ports),
        metadata,
        last_changed: chrono::Utc::now(),
    }
}

fn nerdctl_record(item: NerdctlPsItem) -> ResourceRecord {
    let state = item.state.clone().or_else(|| item.status.clone());
    let ports = parse_port_specs(&item.ports);
    let namespace = item
        .namespace
        .clone()
        .or_else(|| item.labels.get("nerdctl/namespace").cloned());
    let mut metadata = BTreeMap::new();
    if let Some(namespace) = namespace {
        metadata.insert("namespace".into(), namespace);
    }
    if let Some(status) = item.status.clone() {
        metadata.insert("status".into(), status.clone());
    }
    ResourceRecord {
        id: format!("nerdctl:{}", item.id),
        kind: ResourceKind::Container,
        runtime: RuntimeKind::Nerdctl,
        project: item.labels.get("com.docker.compose.project").cloned(),
        name: item.names,
        state: container_state_from_strings(state.as_deref(), item.status.as_deref()),
        runtime_status: item.status.or(state),
        ports: ports.clone(),
        labels: item.labels,
        urls: guess_local_urls_for_ports(&ports),
        metadata,
        last_changed: chrono::Utc::now(),
    }
}

fn container_state_from_strings(state: Option<&str>, status: Option<&str>) -> HealthState {
    let normalized = state.unwrap_or_default().to_ascii_lowercase();
    if normalized.contains("running") || normalized.contains("up") {
        if status
            .unwrap_or_default()
            .to_ascii_lowercase()
            .contains("unhealthy")
        {
            HealthState::Degraded
        } else {
            HealthState::Healthy
        }
    } else if normalized.contains("start")
        || normalized.contains("create")
        || normalized.contains("restart")
    {
        HealthState::Starting
    } else if normalized.contains("exit")
        || normalized.contains("dead")
        || normalized.contains("error")
    {
        if status
            .unwrap_or_default()
            .to_ascii_lowercase()
            .contains("(0)")
        {
            HealthState::Stopped
        } else {
            HealthState::Crashed
        }
    } else {
        HealthState::Unknown
    }
}

#[cfg(test)]
fn docker_socket_override() -> &'static Mutex<Option<PathBuf>> {
    DOCKER_SOCKET_OVERRIDE.get_or_init(|| Mutex::new(None))
}

pub fn parse_port_specs(spec: &str) -> Vec<PortBinding> {
    spec.split(',')
        .filter_map(|segment| {
            let segment = segment.trim();
            if segment.is_empty() {
                return None;
            }

            let (host_side, target_side) = segment.split_once("->")?;
            let target = target_side.split('/').next()?.trim();
            let protocol = target_side.split('/').nth(1).unwrap_or("tcp").trim();
            let host = host_side.rsplit(':').next()?.trim();

            Some(PortBinding {
                host_ip: None,
                host_port: host.parse().ok()?,
                container_port: target.parse().ok(),
                protocol: protocol.to_string(),
            })
        })
        .collect()
}

#[derive(Debug, serde::Deserialize)]
struct PodmanPsItem {
    #[serde(rename = "Id", alias = "ID")]
    id: Option<String>,
    #[serde(rename = "Names", default)]
    names: Vec<String>,
    #[serde(rename = "Status")]
    status: Option<String>,
    #[serde(rename = "State")]
    state: Option<String>,
    #[serde(rename = "Labels")]
    labels: Option<BTreeMap<String, String>>,
    #[serde(rename = "Ports", default)]
    ports: PodmanPorts,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(untagged)]
enum PodmanPorts {
    #[default]
    Empty,
    Strings(Vec<String>),
    Objects(Vec<PodmanPort>),
}

#[derive(Debug, serde::Deserialize)]
struct PodmanPort {
    #[serde(default)]
    host_ip: Option<String>,
    #[serde(default)]
    host_port: Option<u16>,
    #[serde(default)]
    container_port: Option<u16>,
    #[serde(default)]
    protocol: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct NerdctlPsItem {
    #[serde(rename = "ID")]
    id: String,
    #[serde(rename = "Names")]
    names: String,
    #[serde(rename = "State")]
    state: Option<String>,
    #[serde(rename = "Status")]
    status: Option<String>,
    #[serde(rename = "Ports", default)]
    ports: String,
    #[serde(rename = "Labels", default)]
    labels: BTreeMap<String, String>,
    #[serde(rename = "Namespace")]
    namespace: Option<String>,
}

#[cfg(test)]
#[allow(clippy::await_holding_lock)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;

    use super::{
        NerdctlPsItem, PodmanPort, PodmanPorts, PodmanPsItem, collect, collect_docker,
        collect_nerdctl, collect_podman, compose_stack_record, compose_stack_state,
        container_state_from_strings, docker_client, docker_socket_override, docker_state,
        nerdctl_record, parse_port_specs, podman_record, synthesize_compose_stacks,
    };
    use crate::command::{EnvVarGuard, command_overrides, run_command, test_lock, write_script};
    use giggity_core::config::Config;
    use giggity_core::model::{
        HealthState, ResourceKind, ResourceRecord, RuntimeKind, guess_local_urls_for_ports,
    };
    use tempfile::tempdir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixListener;

    fn reset_overrides() {
        command_overrides().lock().expect("lock").clear();
        *docker_socket_override().lock().expect("lock") = None;
    }

    async fn spawn_docker_socket(
        socket_path: &Path,
        status_line: &str,
        body: &str,
    ) -> tokio::task::JoinHandle<()> {
        let _ = std::fs::remove_file(socket_path);
        let listener = UnixListener::bind(socket_path).expect("bind socket");
        let body = body.to_string();
        let status_line = status_line.to_string();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut buffer = [0_u8; 4096];
            let _ = stream.read(&mut buffer).await.expect("read");
            let response = format!(
                "HTTP/1.1 {status_line}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("write response");
        })
    }

    #[tokio::test]
    async fn collector_skips_disabled_sources() {
        let mut config = Config::default();
        config.sources.docker = false;
        config.sources.podman = false;
        config.sources.nerdctl = false;

        let output = collect(&config).await;
        assert!(output.resources.is_empty());
        assert!(output.warnings.is_empty());
    }

    #[test]
    fn parses_port_specs() {
        let ports = parse_port_specs("0.0.0.0:8080->80/tcp, :::8443->443/tcp");
        assert_eq!(ports.len(), 2);
        assert_eq!(ports[0].host_port, 8080);
        assert_eq!(ports[1].container_port, Some(443));
    }

    #[test]
    fn container_states_map_to_dashboard_states() {
        assert_eq!(
            container_state_from_strings(Some("running"), Some("Up 2s")),
            HealthState::Healthy
        );
        assert_eq!(
            container_state_from_strings(Some("up"), Some("Up 2s (unhealthy)")),
            HealthState::Degraded
        );
        assert_eq!(
            container_state_from_strings(Some("exited"), Some("Exited (1) 2s ago")),
            HealthState::Crashed
        );
        assert_eq!(
            container_state_from_strings(Some("dead"), Some("Exited (0) 2s ago")),
            HealthState::Stopped
        );
        assert_eq!(
            container_state_from_strings(Some("restarting"), Some("Restarting")),
            HealthState::Starting
        );
        assert_eq!(
            container_state_from_strings(Some("mystery"), None),
            HealthState::Unknown
        );
    }

    #[test]
    fn docker_state_maps_health_and_exit_conditions() {
        assert_eq!(
            docker_state(Some("running".into()), Some("Up 2s (unhealthy)")),
            HealthState::Degraded
        );
        assert_eq!(
            docker_state(Some("running".into()), Some("Up 2s (health: starting)")),
            HealthState::Starting
        );
        assert_eq!(
            docker_state(Some("created".into()), None),
            HealthState::Starting
        );
        assert_eq!(
            docker_state(Some("exited".into()), Some("Exited (0) 1s ago")),
            HealthState::Stopped
        );
        assert_eq!(
            docker_state(Some("dead".into()), Some("Exited (1) 1s ago")),
            HealthState::Crashed
        );
        assert_eq!(
            docker_state(Some("paused".into()), None),
            HealthState::Degraded
        );
        assert_eq!(docker_state(None, None), HealthState::Unknown);
    }

    #[test]
    fn podman_records_capture_project_ports_and_urls() {
        let record = podman_record(
            PodmanPsItem {
                id: Some("abc123".into()),
                names: vec!["web".into()],
                status: Some("Up 10s".into()),
                state: Some("running".into()),
                labels: Some(BTreeMap::from([(
                    "io.podman.compose.project".into(),
                    "stack".into(),
                )])),
                ports: PodmanPorts::Strings(vec!["0.0.0.0:8080->80/tcp".into()]),
            },
            RuntimeKind::Podman,
        );
        assert_eq!(record.project.as_deref(), Some("stack"));
        assert_eq!(record.ports[0].host_port, 8080);
        assert_eq!(record.state, HealthState::Healthy);
        assert_eq!(record.urls[0].as_str(), "http://127.0.0.1:8080/");
    }

    #[test]
    fn podman_records_parse_object_ports() {
        let record = podman_record(
            PodmanPsItem {
                id: Some("def456".into()),
                names: vec!["api".into()],
                status: Some("Up 3s".into()),
                state: Some("running".into()),
                labels: Some(BTreeMap::new()),
                ports: PodmanPorts::Objects(vec![PodmanPort {
                    host_ip: Some(String::new()),
                    host_port: Some(8080),
                    container_port: Some(8080),
                    protocol: Some("tcp".into()),
                }]),
            },
            RuntimeKind::Podman,
        );

        assert_eq!(record.ports.len(), 1);
        assert_eq!(record.ports[0].host_port, 8080);
        assert_eq!(record.ports[0].container_port, Some(8080));
        assert_eq!(record.urls[0].as_str(), "http://127.0.0.1:8080/");
    }

    #[test]
    fn nerdctl_records_capture_namespace_and_compose_project() {
        let record = nerdctl_record(NerdctlPsItem {
            id: "def456".into(),
            names: "api".into(),
            state: Some("running".into()),
            status: Some("Up 2m".into()),
            ports: "0.0.0.0:9000->9000/tcp".into(),
            labels: BTreeMap::from([("com.docker.compose.project".into(), "stack".into())]),
            namespace: Some("dev".into()),
        });
        assert_eq!(record.project.as_deref(), Some("stack"));
        assert_eq!(record.metadata["namespace"], "dev");
        assert_eq!(record.ports[0].host_port, 9000);
    }

    #[test]
    fn guess_urls_only_emits_known_http_ports() {
        let urls = guess_local_urls_for_ports(&parse_port_specs("0.0.0.0:8080->80/tcp"));
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].as_str(), "http://127.0.0.1:8080/");

        let none = guess_local_urls_for_ports(&parse_port_specs("0.0.0.0:2222->22/tcp"));
        assert!(none.is_empty());
    }

    #[test]
    fn compose_stack_helpers_aggregate_labeled_containers() {
        let web = ResourceRecord {
            id: "docker:web".into(),
            kind: ResourceKind::Container,
            runtime: RuntimeKind::Docker,
            project: Some("app".into()),
            name: "web".into(),
            state: HealthState::Healthy,
            runtime_status: Some("Up".into()),
            ports: parse_port_specs("0.0.0.0:8080->80/tcp"),
            labels: BTreeMap::from([("com.docker.compose.project".into(), "app".into())]),
            urls: guess_local_urls_for_ports(&parse_port_specs("0.0.0.0:8080->80/tcp")),
            metadata: BTreeMap::new(),
            last_changed: chrono::Utc::now(),
        };
        let worker = ResourceRecord {
            id: "docker:worker".into(),
            kind: ResourceKind::Container,
            runtime: RuntimeKind::Docker,
            project: Some("app".into()),
            name: "worker".into(),
            state: HealthState::Crashed,
            runtime_status: Some("Exited (1)".into()),
            ports: Vec::new(),
            labels: BTreeMap::from([("com.docker.compose.project".into(), "app".into())]),
            urls: Vec::new(),
            metadata: BTreeMap::new(),
            last_changed: chrono::Utc::now(),
        };

        let stacks = synthesize_compose_stacks(&[web.clone(), worker.clone()]);
        assert_eq!(stacks.len(), 1);
        assert_eq!(stacks[0].kind, ResourceKind::ComposeStack);
        assert_eq!(stacks[0].runtime, RuntimeKind::Docker);
        assert_eq!(stacks[0].state, HealthState::Crashed);
        assert_eq!(stacks[0].project.as_deref(), Some("app"));
        assert_eq!(stacks[0].name, "app stack");
        assert_eq!(stacks[0].metadata["service_count"], "2");
        assert!(stacks[0].metadata["services"].contains("web"));

        let direct = compose_stack_record(RuntimeKind::Docker, "app", &[&web, &worker]);
        assert_eq!(direct.kind, ResourceKind::ComposeStack);
        assert_eq!(direct.state, HealthState::Crashed);
    }

    #[test]
    fn compose_stack_state_covers_mixed_service_outcomes() {
        assert_eq!(compose_stack_state(0, 0, 0, 0, 0, 0), HealthState::Unknown);
        assert_eq!(compose_stack_state(2, 0, 0, 0, 0, 0), HealthState::Healthy);
        assert_eq!(compose_stack_state(0, 1, 0, 0, 0, 0), HealthState::Starting);
        assert_eq!(compose_stack_state(1, 0, 1, 0, 0, 0), HealthState::Degraded);
        assert_eq!(compose_stack_state(1, 0, 0, 1, 0, 0), HealthState::Crashed);
        assert_eq!(compose_stack_state(0, 0, 0, 0, 2, 0), HealthState::Stopped);
        assert_eq!(compose_stack_state(0, 0, 0, 0, 0, 1), HealthState::Unknown);
        assert_eq!(compose_stack_state(1, 0, 0, 0, 1, 0), HealthState::Degraded);
        assert_eq!(compose_stack_state(1, 0, 0, 0, 0, 1), HealthState::Degraded);
        assert_eq!(compose_stack_state(0, 0, 0, 0, 1, 1), HealthState::Unknown);
    }

    #[test]
    fn compose_stack_synthesis_skips_non_container_and_single_member_groups() {
        let stack_container = ResourceRecord {
            id: "podman:web".into(),
            kind: ResourceKind::Container,
            runtime: RuntimeKind::Podman,
            project: Some("stack".into()),
            name: "web".into(),
            state: HealthState::Starting,
            runtime_status: Some("Starting".into()),
            ports: Vec::new(),
            labels: BTreeMap::new(),
            urls: Vec::new(),
            metadata: BTreeMap::new(),
            last_changed: chrono::Utc::now(),
        };
        let host = ResourceRecord {
            id: "host:123".into(),
            kind: ResourceKind::HostProcess,
            runtime: RuntimeKind::Host,
            project: Some("stack".into()),
            name: "host".into(),
            state: HealthState::Healthy,
            runtime_status: None,
            ports: Vec::new(),
            labels: BTreeMap::new(),
            urls: Vec::new(),
            metadata: BTreeMap::new(),
            last_changed: chrono::Utc::now(),
        };

        assert!(synthesize_compose_stacks(&[host]).is_empty());
        assert!(synthesize_compose_stacks(&[stack_container]).is_empty());
    }

    #[test]
    fn compose_stack_record_injects_runtime_specific_project_labels() {
        let first = ResourceRecord {
            id: "podman:web".into(),
            kind: ResourceKind::Container,
            runtime: RuntimeKind::Podman,
            project: Some("stack".into()),
            name: "web".into(),
            state: HealthState::Stopped,
            runtime_status: Some("Exited".into()),
            ports: Vec::new(),
            labels: BTreeMap::new(),
            urls: Vec::new(),
            metadata: BTreeMap::new(),
            last_changed: chrono::Utc::now(),
        };
        let second = ResourceRecord {
            name: "worker".into(),
            state: HealthState::Unknown,
            ..first.clone()
        };

        let stack = compose_stack_record(RuntimeKind::Podman, "stack", &[&first, &second]);
        assert_eq!(stack.labels["io.podman.compose.project"], "stack");
        assert_eq!(stack.metadata["stopped_count"], "1");
        assert_eq!(stack.metadata["unknown_count"], "1");
        assert_eq!(stack.state, HealthState::Unknown);
    }

    #[test]
    fn compose_stack_record_tracks_starting_and_degraded_members_and_injects_docker_label() {
        let first = ResourceRecord {
            id: "docker:web".into(),
            kind: ResourceKind::Container,
            runtime: RuntimeKind::Docker,
            project: Some("stack".into()),
            name: "web".into(),
            state: HealthState::Starting,
            runtime_status: Some("Starting".into()),
            ports: Vec::new(),
            labels: BTreeMap::new(),
            urls: Vec::new(),
            metadata: BTreeMap::new(),
            last_changed: chrono::Utc::now(),
        };
        let second = ResourceRecord {
            name: "worker".into(),
            state: HealthState::Degraded,
            runtime_status: Some("Unhealthy".into()),
            ..first.clone()
        };

        let stack = compose_stack_record(RuntimeKind::Docker, "stack", &[&first, &second]);
        assert_eq!(stack.labels["com.docker.compose.project"], "stack");
        assert_eq!(stack.metadata["starting_count"], "1");
        assert_eq!(stack.metadata["degraded_count"], "1");
        assert_eq!(stack.state, HealthState::Degraded);
    }

    #[tokio::test]
    async fn run_command_returns_stdout_and_errors() {
        let ok = run_command("containers", "sh", &["-c", "printf ok"])
            .await
            .expect("stdout");
        assert_eq!(ok, "ok");

        let error = run_command("containers", "sh", &["-c", "echo nope >&2; exit 7"])
            .await
            .expect_err("command failure");
        assert!(error.to_string().contains("sh failed: nope"));
    }

    #[tokio::test]
    async fn collect_podman_and_nerdctl_use_override_commands() {
        let _guard = test_lock().lock().expect("lock");
        reset_overrides();
        let dir = tempdir().expect("tempdir");
        let podman = write_script(
            dir.path(),
            "podman",
            "printf '[{\"Id\":\"pod-1\",\"Names\":[\"web\"],\"State\":\"running\",\"Status\":\"Up\",\"Ports\":[\"0.0.0.0:8080->80/tcp\"]}]'",
        );
        let nerdctl = write_script(
            dir.path(),
            "nerdctl",
            "printf '{\"ID\":\"nerd-1\",\"Names\":\"api\",\"State\":\"running\",\"Status\":\"Up\",\"Ports\":\"0.0.0.0:9000->9000/tcp\",\"Labels\":{\"com.docker.compose.project\":\"stack\"}}\\n'",
        );
        command_overrides()
            .lock()
            .expect("lock")
            .insert("podman".into(), podman);
        command_overrides()
            .lock()
            .expect("lock")
            .insert("nerdctl".into(), nerdctl);

        let podman_records = collect_podman().await.expect("podman");
        let nerdctl_records = collect_nerdctl().await.expect("nerdctl");

        assert_eq!(podman_records.len(), 1);
        assert_eq!(podman_records[0].name, "web");
        assert_eq!(nerdctl_records.len(), 1);
        assert_eq!(nerdctl_records[0].metadata["status"], "Up");
        reset_overrides();
    }

    #[tokio::test]
    async fn collect_podman_supports_object_port_output() {
        let _guard = test_lock().lock().expect("lock");
        reset_overrides();
        let dir = tempdir().expect("tempdir");
        let podman = write_script(
            dir.path(),
            "podman",
            "printf '[{\"Id\":\"pod-1\",\"Names\":[\"web\"],\"State\":\"running\",\"Status\":\"Up\",\"Ports\":[{\"host_ip\":\"\",\"host_port\":18081,\"container_port\":8080,\"protocol\":\"tcp\"}]}]'",
        );
        command_overrides()
            .lock()
            .expect("lock")
            .insert("podman".into(), podman);

        let podman_records = collect_podman().await.expect("podman");
        assert_eq!(podman_records.len(), 1);
        assert_eq!(podman_records[0].name, "web");
        assert_eq!(podman_records[0].ports.len(), 1);
        assert_eq!(podman_records[0].ports[0].host_port, 18081);
        reset_overrides();
    }

    #[tokio::test]
    async fn collect_podman_ignores_unpublished_object_ports() {
        let _guard = test_lock().lock().expect("lock");
        reset_overrides();
        let dir = tempdir().expect("tempdir");
        let podman = write_script(
            dir.path(),
            "podman",
            "printf '[{\"Id\":\"pod-1\",\"Names\":[\"web\"],\"State\":\"running\",\"Status\":\"Up\",\"Ports\":[{\"host_ip\":\"\",\"container_port\":8080,\"protocol\":\"tcp\"}]}]'",
        );
        command_overrides()
            .lock()
            .expect("lock")
            .insert("podman".into(), podman);

        let podman_records = collect_podman().await.expect("podman");
        assert_eq!(podman_records.len(), 1);
        assert!(podman_records[0].ports.is_empty());
        reset_overrides();
    }

    #[tokio::test]
    async fn collect_aggregates_runtime_warnings() {
        let _guard = test_lock().lock().expect("lock");
        reset_overrides();
        *docker_socket_override().lock().expect("lock") =
            Some(tempdir().expect("tmp").path().join("missing.sock"));
        let dir = tempdir().expect("tempdir");
        let podman = write_script(dir.path(), "podman", "echo podman bad >&2; exit 1");
        let nerdctl = write_script(dir.path(), "nerdctl", "echo nerd bad >&2; exit 1");
        {
            let mut overrides = command_overrides().lock().expect("lock");
            overrides.insert("podman".into(), podman);
            overrides.insert("nerdctl".into(), nerdctl);
        }

        let mut config = Config::default();
        config.sources.docker = true;
        config.sources.podman = true;
        config.sources.nerdctl = true;

        let output = collect(&config).await;
        assert_eq!(output.resources.len(), 0);
        assert_eq!(output.warnings.len(), 3);
        assert!(
            output
                .warnings
                .iter()
                .any(|warning| warning.source == "docker")
        );
        assert!(
            output
                .warnings
                .iter()
                .any(|warning| warning.source == "podman")
        );
        assert!(
            output
                .warnings
                .iter()
                .any(|warning| warning.source == "nerdctl")
        );
        reset_overrides();
    }

    #[tokio::test]
    async fn collect_combines_successful_container_sources() {
        let _guard = test_lock().lock().expect("lock");
        reset_overrides();
        let dir = tempdir().expect("tempdir");
        let socket = dir.path().join("docker.sock");
        let server = spawn_docker_socket(
            &socket,
            "200 OK",
            r#"[{"Id":"abc123","Names":["/web"],"Labels":{},"State":"running","Status":"Up 5 seconds","Ports":[{"IP":"0.0.0.0","PrivatePort":80,"PublicPort":8080,"Type":"tcp"}]}]"#,
        )
        .await;
        *docker_socket_override().lock().expect("lock") = Some(socket);

        let podman = write_script(
            dir.path(),
            "podman",
            "printf '[{\"Id\":\"pod-1\",\"Names\":[\"worker\"],\"State\":\"running\",\"Status\":\"Up\",\"Ports\":[\"0.0.0.0:8081->81/tcp\"]}]'",
        );
        let nerdctl = write_script(
            dir.path(),
            "nerdctl",
            "printf '{\"ID\":\"nerd-1\",\"Names\":\"api\",\"State\":\"running\",\"Status\":\"Up\",\"Ports\":\"0.0.0.0:9000->9000/tcp\",\"Labels\":{}}\\n'",
        );
        {
            let mut overrides = command_overrides().lock().expect("lock");
            overrides.insert("podman".into(), podman);
            overrides.insert("nerdctl".into(), nerdctl);
        }

        let output = collect(&Config::default()).await;
        assert_eq!(output.resources.len(), 3);
        assert!(output.warnings.is_empty());
        server.await.expect("server");
        reset_overrides();
    }

    #[tokio::test]
    async fn collect_docker_reads_from_fake_socket() {
        let _guard = test_lock().lock().expect("lock");
        reset_overrides();
        let dir = tempdir().expect("tempdir");
        let socket = dir.path().join("docker.sock");
        let server = spawn_docker_socket(
            &socket,
            "200 OK",
            r#"[{"Id":"abc123","Names":["/web"],"Labels":{"com.docker.compose.project":"stack"},"State":"running","Status":"Up 5 seconds","Ports":[{"IP":"0.0.0.0","PrivatePort":80,"PublicPort":8080,"Type":"tcp"}]}]"#,
        )
        .await;
        *docker_socket_override().lock().expect("lock") = Some(socket);

        let records = collect_docker().await.expect("docker");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].id, "docker:abc123");
        assert_eq!(records[0].project.as_deref(), Some("stack"));
        assert_eq!(records[0].ports[0].host_port, 8080);
        server.await.expect("server");
        reset_overrides();
    }

    #[tokio::test]
    async fn collect_docker_surfaces_api_errors() {
        let _guard = test_lock().lock().expect("lock");
        reset_overrides();
        let dir = tempdir().expect("tempdir");
        let socket = dir.path().join("docker.sock");
        let server = spawn_docker_socket(
            &socket,
            "500 Internal Server Error",
            r#"{"message":"boom"}"#,
        )
        .await;
        *docker_socket_override().lock().expect("lock") = Some(socket);

        let error = collect_docker().await.expect_err("docker error");
        assert!(error.to_string().contains("boom") || error.to_string().contains("500"));
        server.await.expect("server");
        reset_overrides();
    }

    #[tokio::test]
    async fn collect_docker_uses_docker_host_env_socket() {
        let _guard = test_lock().lock().expect("lock");
        reset_overrides();
        let dir = tempdir().expect("tempdir");
        let socket = dir.path().join("docker.sock");
        let server = spawn_docker_socket(
            &socket,
            "200 OK",
            r#"[{"Id":"abc123","Names":["/web"],"Labels":{},"State":"running","Status":"Up 5 seconds","Ports":[{"IP":"0.0.0.0","PrivatePort":80,"PublicPort":8080,"Type":"tcp"}]}]"#,
        )
        .await;
        let _env = EnvVarGuard::set("DOCKER_HOST", format!("unix://{}", socket.display()));

        let records = collect_docker().await.expect("docker");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].id, "docker:abc123");
        server.await.expect("server");
        reset_overrides();
    }

    #[tokio::test]
    async fn collect_docker_uses_giggity_docker_socket_env() {
        let _guard = test_lock().lock().expect("lock");
        reset_overrides();
        let dir = tempdir().expect("tempdir");
        let socket = dir.path().join("docker.sock");
        let server = spawn_docker_socket(
            &socket,
            "200 OK",
            r#"[{"Id":"abc123","Names":["/web"],"Labels":{},"State":"running","Status":"Up 5 seconds","Ports":[]}]"#,
        )
        .await;
        let _env = EnvVarGuard::set("GIGGITY_DOCKER_SOCKET", socket.display().to_string());

        let records = collect_docker().await.expect("docker");
        assert_eq!(records.len(), 1);
        server.await.expect("server");
        reset_overrides();
    }

    #[tokio::test]
    async fn collect_docker_prefers_giggity_socket_over_docker_host() {
        let _guard = test_lock().lock().expect("lock");
        reset_overrides();
        let dir = tempdir().expect("tempdir");
        let socket = dir.path().join("docker.sock");
        let server = spawn_docker_socket(
            &socket,
            "200 OK",
            r#"[{"Id":"giggity-first","Names":["/web"],"Labels":{},"State":"running","Status":"Up 5 seconds","Ports":[]}]"#,
        )
        .await;
        let _env = EnvVarGuard::set_many([
            ("GIGGITY_DOCKER_SOCKET", Some(socket.as_os_str())),
            (
                "DOCKER_HOST",
                Some(std::ffi::OsStr::new("unix:///tmp/does-not-exist.sock")),
            ),
        ]);

        let records = collect_docker().await.expect("docker");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].id, "docker:giggity-first");
        server.await.expect("server");
        reset_overrides();
    }

    #[tokio::test]
    async fn collect_docker_covers_fallback_name_and_missing_metadata_branches() {
        let _guard = test_lock().lock().expect("lock");
        reset_overrides();
        let dir = tempdir().expect("tempdir");
        let socket = dir.path().join("docker.sock");
        let server = spawn_docker_socket(
            &socket,
            "200 OK",
            r#"[{"Id":"abcdef1234567890","Names":null,"Labels":{"io.podman.compose.project":"stack"},"State":"running","Status":null,"Ports":[{"IP":"0.0.0.0","PrivatePort":80,"Type":null},{"IP":"0.0.0.0","PrivatePort":443,"PublicPort":8443,"Type":null}]}]"#,
        )
        .await;
        *docker_socket_override().lock().expect("lock") = Some(socket);

        let records = collect_docker().await.expect("docker");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].name, "abcdef123456");
        assert_eq!(records[0].project.as_deref(), Some("stack"));
        assert_eq!(records[0].ports.len(), 1);
        assert_eq!(records[0].ports[0].host_port, 8443);
        assert_eq!(records[0].ports[0].protocol, "tcp");
        assert!(!records[0].metadata.contains_key("status"));
        server.await.expect("server");
        reset_overrides();
    }

    #[test]
    fn docker_client_uses_local_defaults_when_no_socket_env_is_set() {
        let _guard = test_lock().lock().expect("lock");
        reset_overrides();
        let _env = EnvVarGuard::set_many([
            ("GIGGITY_DOCKER_SOCKET", None::<std::ffi::OsString>),
            ("DOCKER_HOST", None::<std::ffi::OsString>),
        ]);

        assert!(docker_client().is_ok());
    }

    #[test]
    fn docker_client_ignores_empty_socket_overrides() {
        let _guard = test_lock().lock().expect("lock");
        reset_overrides();
        let _env = EnvVarGuard::set_many([
            ("GIGGITY_DOCKER_SOCKET", Some("")),
            ("DOCKER_HOST", Some("tcp://127.0.0.1:2375")),
        ]);

        assert!(docker_client().is_ok());
    }

    #[test]
    fn parse_port_specs_and_state_helpers_cover_edge_cases() {
        assert!(parse_port_specs("").is_empty());
        assert!(parse_port_specs("not-a-port").is_empty());
        assert!(
            guess_local_urls_for_ports(&parse_port_specs("0.0.0.0:8443->443/tcp"))[0]
                .as_str()
                .starts_with("https://")
        );
        assert_eq!(
            docker_state(Some("running".into()), Some("health: starting")),
            HealthState::Starting
        );
        assert_eq!(
            docker_state(Some("dead".into()), Some("Exited (2)")),
            HealthState::Crashed
        );
        assert_eq!(
            container_state_from_strings(Some("up"), Some("unhealthy")),
            HealthState::Degraded
        );
        assert_eq!(
            container_state_from_strings(Some("dead"), Some("Exited (0)")),
            HealthState::Stopped
        );
        assert_eq!(
            container_state_from_strings(None, None),
            HealthState::Unknown
        );
    }

    #[test]
    fn record_builders_cover_fallback_names_and_metadata() {
        let podman = podman_record(
            PodmanPsItem {
                id: None,
                names: Vec::new(),
                status: None,
                state: Some("created".into()),
                labels: None,
                ports: PodmanPorts::Empty,
            },
            RuntimeKind::Podman,
        );
        assert!(podman.id.starts_with("podman:"));
        assert_eq!(podman.state, HealthState::Starting);

        let nerdctl = nerdctl_record(NerdctlPsItem {
            id: "id".into(),
            names: "api".into(),
            state: Some("error".into()),
            status: None,
            ports: String::new(),
            labels: BTreeMap::new(),
            namespace: None,
        });
        assert_eq!(nerdctl.state, HealthState::Crashed);
        assert!(!nerdctl.metadata.contains_key("namespace"));
    }

    #[test]
    fn nerdctl_records_support_current_cli_json_shape_without_state_field() {
        let item: NerdctlPsItem = serde_json::from_str(
            r#"{
                "ID":"abc123",
                "Names":"web",
                "Status":"Up",
                "Ports":"0.0.0.0:18081->8080/tcp",
                "Labels":{"nerdctl/namespace":"default"}
            }"#,
        )
        .expect("nerdctl item");
        let record = nerdctl_record(item);
        assert_eq!(record.state, HealthState::Healthy);
        assert_eq!(record.metadata["namespace"], "default");
        assert_eq!(record.runtime_status.as_deref(), Some("Up"));
        assert_eq!(record.ports[0].host_port, 18081);
    }
}
