use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{Display, Formatter};
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use url::Url;

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum HealthState {
    Healthy,
    Starting,
    Degraded,
    Crashed,
    Stopped,
    #[default]
    Unknown,
}

impl HealthState {
    pub fn severity(self) -> u8 {
        match self {
            HealthState::Crashed => 5,
            HealthState::Degraded => 4,
            HealthState::Starting => 3,
            HealthState::Stopped => 2,
            HealthState::Unknown => 1,
            HealthState::Healthy => 0,
        }
    }

    pub fn is_issue(self) -> bool {
        matches!(
            self,
            HealthState::Crashed | HealthState::Degraded | HealthState::Starting
        )
    }
}

impl Display for HealthState {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            HealthState::Healthy => "healthy",
            HealthState::Starting => "starting",
            HealthState::Degraded => "degraded",
            HealthState::Crashed => "crashed",
            HealthState::Stopped => "stopped",
            HealthState::Unknown => "unknown",
        };
        f.write_str(text)
    }
}

impl FromStr for HealthState {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "healthy" => Ok(Self::Healthy),
            "starting" => Ok(Self::Starting),
            "degraded" => Ok(Self::Degraded),
            "crashed" => Ok(Self::Crashed),
            "stopped" => Ok(Self::Stopped),
            "unknown" => Ok(Self::Unknown),
            _ => Err(format!("unknown health state: {value}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceKind {
    Container,
    ComposeStack,
    HostProcess,
    KubernetesPod,
    LaunchdUnit,
    SystemdUnit,
}

impl Display for ResourceKind {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            ResourceKind::Container => "container",
            ResourceKind::ComposeStack => "compose_stack",
            ResourceKind::HostProcess => "host_process",
            ResourceKind::KubernetesPod => "kubernetes_pod",
            ResourceKind::LaunchdUnit => "launchd_unit",
            ResourceKind::SystemdUnit => "systemd_unit",
        };
        f.write_str(text)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeKind {
    Docker,
    Podman,
    Nerdctl,
    Kubernetes,
    Host,
    Launchd,
    Systemd,
}

impl Display for RuntimeKind {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            RuntimeKind::Docker => "docker",
            RuntimeKind::Podman => "podman",
            RuntimeKind::Nerdctl => "nerdctl",
            RuntimeKind::Kubernetes => "kubernetes",
            RuntimeKind::Host => "host",
            RuntimeKind::Launchd => "launchd",
            RuntimeKind::Systemd => "systemd",
        };
        f.write_str(text)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortBinding {
    pub host_ip: Option<String>,
    pub host_port: u16,
    pub container_port: Option<u16>,
    #[serde(default = "default_protocol")]
    pub protocol: String,
}

fn default_protocol() -> String {
    "tcp".to_string()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceRecord {
    pub id: String,
    pub kind: ResourceKind,
    pub runtime: RuntimeKind,
    pub project: Option<String>,
    pub name: String,
    pub state: HealthState,
    pub runtime_status: Option<String>,
    #[serde(default)]
    pub ports: Vec<PortBinding>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub urls: Vec<Url>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
    pub last_changed: DateTime<Utc>,
}

impl ResourceRecord {
    pub fn summary_name(&self) -> String {
        if self.kind == ResourceKind::ComposeStack {
            return match self.ports.first() {
                Some(port) => format!("{}:{}", self.name, port.host_port),
                None => self.name.clone(),
            };
        }

        if self.kind == ResourceKind::KubernetesPod {
            let namespace = self
                .namespace()
                .or(self.project.as_deref())
                .unwrap_or_default();
            return match (namespace.is_empty(), self.ports.first()) {
                (false, Some(port)) => format!("{namespace}/{}:{}", self.name, port.host_port),
                (false, None) => format!("{namespace}/{}", self.name),
                (true, Some(port)) => format!("{}:{}", self.name, port.host_port),
                (true, None) => self.name.clone(),
            };
        }

        match (&self.project, self.ports.first()) {
            (Some(project), Some(port)) => format!("{project}/{}:{}", self.name, port.host_port),
            (_, Some(port)) => format!("{}:{}", self.name, port.host_port),
            _ => self.name.clone(),
        }
    }

    pub fn namespace(&self) -> Option<&str> {
        self.metadata.get("namespace").map(String::as_str)
    }

    pub fn compose_project(&self) -> Option<&str> {
        self.labels
            .get("com.docker.compose.project")
            .or_else(|| self.labels.get("io.podman.compose.project"))
            .map(String::as_str)
            .or(self.project.as_deref())
    }
}

pub fn guess_local_url(port: u16) -> Option<Url> {
    let scheme = match port {
        443 | 8443 => "https",
        80 | 3000 | 4000 | 4200 | 5000 | 5173 | 8000 | 8080 | 9000 => "http",
        _ => return None,
    };

    Url::parse(&format!("{scheme}://127.0.0.1:{port}")).ok()
}

pub fn guess_local_urls_for_ports(ports: &[PortBinding]) -> Vec<Url> {
    let mut seen = BTreeSet::new();
    let mut urls = Vec::new();

    for port in ports {
        let Some(url) = guess_local_url(port.host_port) else {
            continue;
        };
        let key = url.to_string();
        if seen.insert(key) {
            urls.push(url);
        }
    }

    urls
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentEvent {
    pub resource_id: String,
    pub resource_name: String,
    pub from: Option<HealthState>,
    pub to: HealthState,
    pub timestamp: DateTime<Utc>,
    pub cause: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollectorWarning {
    pub source: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub api_version: u8,
    pub generated_at: DateTime<Utc>,
    #[serde(default)]
    pub resources: Vec<ResourceRecord>,
    #[serde(default)]
    pub events: Vec<RecentEvent>,
    #[serde(default)]
    pub warnings: Vec<CollectorWarning>,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            api_version: 1,
            generated_at: Utc::now(),
            resources: Vec::new(),
            events: Vec::new(),
            warnings: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use chrono::Utc;

    use super::{
        HealthState, PortBinding, ResourceKind, ResourceRecord, RuntimeKind, Snapshot,
        guess_local_url, guess_local_urls_for_ports,
    };

    #[test]
    fn health_state_severity_prioritizes_failures() {
        assert!(HealthState::Crashed.severity() > HealthState::Healthy.severity());
        assert_eq!(HealthState::Starting.severity(), 3);
        assert_eq!(HealthState::Stopped.severity(), 2);
        assert_eq!(HealthState::Unknown.severity(), 1);
        assert!(HealthState::Degraded.is_issue());
        assert!(!HealthState::Stopped.is_issue());
    }

    #[test]
    fn summary_name_prefers_project_and_primary_port() {
        let record = ResourceRecord {
            id: "docker:web".into(),
            kind: ResourceKind::Container,
            runtime: RuntimeKind::Docker,
            project: Some("stack".into()),
            name: "web".into(),
            state: HealthState::Healthy,
            runtime_status: None,
            ports: vec![PortBinding {
                host_ip: None,
                host_port: 8080,
                container_port: Some(80),
                protocol: "tcp".into(),
            }],
            labels: Default::default(),
            urls: Default::default(),
            metadata: Default::default(),
            last_changed: Utc::now(),
        };

        assert_eq!(record.summary_name(), "stack/web:8080");
        assert_eq!(
            ResourceRecord {
                project: None,
                ..record.clone()
            }
            .summary_name(),
            "web:8080"
        );
        assert_eq!(
            ResourceRecord {
                ports: Vec::new(),
                ..record
            }
            .summary_name(),
            "web"
        );
    }

    #[test]
    fn summary_name_handles_compose_stacks_and_kubernetes_pods() {
        let record = ResourceRecord {
            id: "docker:web".into(),
            kind: ResourceKind::Container,
            runtime: RuntimeKind::Docker,
            project: Some("stack".into()),
            name: "web".into(),
            state: HealthState::Healthy,
            runtime_status: None,
            ports: vec![PortBinding {
                host_ip: None,
                host_port: 8080,
                container_port: Some(80),
                protocol: "tcp".into(),
            }],
            labels: BTreeMap::new(),
            urls: Vec::new(),
            metadata: BTreeMap::new(),
            last_changed: Utc::now(),
        };

        assert_eq!(
            ResourceRecord {
                kind: ResourceKind::ComposeStack,
                name: "stack".into(),
                ..record.clone()
            }
            .summary_name(),
            "stack:8080"
        );
        assert_eq!(
            ResourceRecord {
                kind: ResourceKind::KubernetesPod,
                runtime: RuntimeKind::Kubernetes,
                project: Some("dev".into()),
                name: "api".into(),
                metadata: BTreeMap::from([("namespace".into(), "dev".into())]),
                ..record.clone()
            }
            .summary_name(),
            "dev/api:8080"
        );
        assert_eq!(
            ResourceRecord {
                kind: ResourceKind::ComposeStack,
                name: "stack".into(),
                ports: Vec::new(),
                ..record.clone()
            }
            .summary_name(),
            "stack"
        );
        assert_eq!(
            ResourceRecord {
                kind: ResourceKind::KubernetesPod,
                runtime: RuntimeKind::Kubernetes,
                project: None,
                name: "api".into(),
                metadata: BTreeMap::new(),
                ..record.clone()
            }
            .summary_name(),
            "api:8080"
        );
        assert_eq!(
            ResourceRecord {
                kind: ResourceKind::KubernetesPod,
                runtime: RuntimeKind::Kubernetes,
                project: None,
                name: "api".into(),
                ports: Vec::new(),
                metadata: BTreeMap::new(),
                ..record
            }
            .summary_name(),
            "api"
        );
    }

    #[test]
    fn health_state_round_trips_through_display_and_parse() {
        for state in [
            HealthState::Healthy,
            HealthState::Starting,
            HealthState::Degraded,
            HealthState::Crashed,
            HealthState::Stopped,
            HealthState::Unknown,
        ] {
            let parsed = state.to_string().parse::<HealthState>().expect("parse");
            assert_eq!(parsed, state);
        }
        assert!("bogus".parse::<HealthState>().is_err());
    }

    #[test]
    fn runtime_and_kind_display_match_expected_tokens() {
        assert_eq!(RuntimeKind::Docker.to_string(), "docker");
        assert_eq!(RuntimeKind::Podman.to_string(), "podman");
        assert_eq!(RuntimeKind::Nerdctl.to_string(), "nerdctl");
        assert_eq!(RuntimeKind::Kubernetes.to_string(), "kubernetes");
        assert_eq!(RuntimeKind::Host.to_string(), "host");
        assert_eq!(RuntimeKind::Launchd.to_string(), "launchd");
        assert_eq!(RuntimeKind::Systemd.to_string(), "systemd");
        assert_eq!(ResourceKind::Container.to_string(), "container");
        assert_eq!(ResourceKind::ComposeStack.to_string(), "compose_stack");
        assert_eq!(ResourceKind::HostProcess.to_string(), "host_process");
        assert_eq!(ResourceKind::KubernetesPod.to_string(), "kubernetes_pod");
        assert_eq!(ResourceKind::LaunchdUnit.to_string(), "launchd_unit");
        assert_eq!(ResourceKind::SystemdUnit.to_string(), "systemd_unit");
    }

    #[test]
    fn helpers_return_namespace_and_compose_project() {
        let record = ResourceRecord {
            id: "docker:web".into(),
            kind: ResourceKind::Container,
            runtime: RuntimeKind::Docker,
            project: Some("fallback".into()),
            name: "web".into(),
            state: HealthState::Healthy,
            runtime_status: None,
            ports: Vec::new(),
            labels: BTreeMap::from([("com.docker.compose.project".into(), "stack".into())]),
            urls: Vec::new(),
            metadata: BTreeMap::from([("namespace".into(), "dev".into())]),
            last_changed: Utc::now(),
        };

        assert_eq!(record.namespace(), Some("dev"));
        assert_eq!(record.compose_project(), Some("stack"));
    }

    #[test]
    fn snapshot_default_sets_version_and_is_empty() {
        let snapshot = Snapshot::default();
        assert_eq!(snapshot.api_version, 1);
        assert!(snapshot.resources.is_empty());
        assert!(snapshot.events.is_empty());
        assert!(snapshot.warnings.is_empty());
    }

    #[test]
    fn port_binding_defaults_protocol_to_tcp() {
        let binding: PortBinding = serde_json::from_str(r#"{"host_port":3000}"#).expect("port");
        assert_eq!(binding.protocol, "tcp");
    }

    #[test]
    fn local_url_helpers_guess_expected_ports_and_deduplicate() {
        assert_eq!(
            guess_local_url(8080).expect("http").as_str(),
            "http://127.0.0.1:8080/"
        );
        assert_eq!(
            guess_local_url(443).expect("https").as_str(),
            "https://127.0.0.1/"
        );
        assert_eq!(
            guess_local_url(8443).expect("https").as_str(),
            "https://127.0.0.1:8443/"
        );
        assert!(guess_local_url(22).is_none());

        let urls = guess_local_urls_for_ports(&[
            PortBinding {
                host_ip: None,
                host_port: 8080,
                container_port: Some(80),
                protocol: "tcp".into(),
            },
            PortBinding {
                host_ip: None,
                host_port: 8080,
                container_port: Some(8080),
                protocol: "tcp".into(),
            },
            PortBinding {
                host_ip: None,
                host_port: 8443,
                container_port: Some(443),
                protocol: "tcp".into(),
            },
            PortBinding {
                host_ip: None,
                host_port: 22,
                container_port: Some(22),
                protocol: "tcp".into(),
            },
        ]);

        assert_eq!(urls.len(), 2);
        assert_eq!(urls[0].as_str(), "http://127.0.0.1:8080/");
        assert_eq!(urls[1].as_str(), "https://127.0.0.1:8443/");
    }
}
