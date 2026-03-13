use std::collections::BTreeMap;

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
        collect_nerdctl, collect_podman, container_state_from_strings, docker_client,
        docker_socket_override, docker_state, nerdctl_record, parse_port_specs, podman_record,
    };
    use crate::command::{EnvVarGuard, command_overrides, run_command, test_lock, write_script};
    use giggity_core::config::Config;
    use giggity_core::model::{HealthState, RuntimeKind, guess_local_urls_for_ports};
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
