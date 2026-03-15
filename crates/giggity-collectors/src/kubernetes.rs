use std::collections::{BTreeMap, BTreeSet};

use giggity_core::config::Config;
use giggity_core::model::{
    CollectorWarning, HealthState, PortBinding, ResourceKind, ResourceRecord, RuntimeKind,
    guess_local_urls_for_ports,
};

use crate::CollectionOutput;
use crate::command::run_command;

pub async fn collect(config: &Config) -> CollectionOutput {
    if !config.sources.kubernetes {
        return CollectionOutput::default();
    }

    let mut output = CollectionOutput::default();
    match collect_pods().await {
        Ok(resources) => output.resources.extend(resources),
        Err(error) => output.warnings.push(CollectorWarning {
            source: "kubernetes".into(),
            message: error.to_string(),
        }),
    }
    output
}

async fn collect_pods() -> anyhow::Result<Vec<ResourceRecord>> {
    let context = match run_command("kubernetes", "kubectl", &["config", "current-context"]).await {
        Ok(output) => output.trim().to_string(),
        Err(error) if ignorable_kubectl_error(&error) => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    if context.is_empty() {
        return Ok(Vec::new());
    }

    let output = match run_command(
        "kubernetes",
        "kubectl",
        &["get", "pods", "--all-namespaces", "-o", "json"],
    )
    .await
    {
        Ok(output) => output,
        Err(error) if ignorable_kubectl_error(&error) => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };

    parse_pod_list(&output, &context)
}

fn ignorable_kubectl_error(error: &anyhow::Error) -> bool {
    let message = error.to_string().to_ascii_lowercase();
    message.contains("no such file or directory")
        || message.contains("command not found")
        || message.contains("current-context is not set")
        || message.contains("no context exists")
        || message.contains("no configuration has been provided")
}

fn parse_pod_list(output: &str, context: &str) -> anyhow::Result<Vec<ResourceRecord>> {
    let pod_list: KubernetesPodList = serde_json::from_str(output)?;
    Ok(pod_list
        .items
        .into_iter()
        .map(|item| pod_record(item, context))
        .collect())
}

fn pod_record(item: KubernetesPodItem, context: &str) -> ResourceRecord {
    let (state, runtime_status, restart_count, reason) = pod_state(&item);
    let namespace = item.metadata.namespace.unwrap_or_else(|| "default".into());
    let labels = item.metadata.labels.unwrap_or_default();
    let ports = pod_ports(item.spec.as_ref());
    let urls = guess_local_urls_for_ports(&ports);
    let mut metadata = BTreeMap::from([
        ("namespace".into(), namespace.clone()),
        ("cluster_context".into(), context.to_string()),
        ("pod_name".into(), item.metadata.name.clone()),
        ("restart_count".into(), restart_count.to_string()),
    ]);
    if let Some(uid) = item.metadata.uid {
        metadata.insert("uid".into(), uid);
    }
    if let Some(status) = item.status.as_ref() {
        if let Some(phase) = &status.phase {
            metadata.insert("phase".into(), phase.clone());
        }
        if let Some(pod_ip) = &status.pod_ip {
            metadata.insert("pod_ip".into(), pod_ip.clone());
        }
        if let Some(host_ip) = &status.host_ip {
            metadata.insert("host_ip".into(), host_ip.clone());
        }
    }
    if let Some(spec) = item.spec.as_ref()
        && let Some(node_name) = &spec.node_name
    {
        metadata.insert("node_name".into(), node_name.clone());
    }
    if let Some(reason) = reason {
        metadata.insert("reason".into(), reason);
    }

    ResourceRecord {
        id: format!("kubernetes:{namespace}:{}", item.metadata.name),
        kind: ResourceKind::KubernetesPod,
        runtime: RuntimeKind::Kubernetes,
        project: Some(namespace),
        name: item.metadata.name,
        state,
        runtime_status,
        ports,
        labels,
        urls,
        metadata,
        last_changed: chrono::Utc::now(),
        state_since: chrono::Utc::now(),
    }
}

fn pod_ports(spec: Option<&KubernetesPodSpec>) -> Vec<PortBinding> {
    let mut ports = Vec::new();
    let mut seen = BTreeSet::new();

    for container in spec.into_iter().flat_map(|spec| spec.containers.iter()) {
        for port in &container.ports {
            let Some(host_port) = port.host_port else {
                continue;
            };
            let key = (host_port, port.protocol.clone());
            if !seen.insert(key.clone()) {
                continue;
            }
            ports.push(PortBinding {
                host_ip: None,
                host_port,
                container_port: Some(port.container_port),
                protocol: key.1.unwrap_or_else(|| "tcp".into()).to_ascii_lowercase(),
            });
        }
    }

    ports
}

fn pod_state(item: &KubernetesPodItem) -> (HealthState, Option<String>, u64, Option<String>) {
    let phase = item
        .status
        .as_ref()
        .and_then(|status| status.phase.as_deref())
        .unwrap_or("Unknown");
    let statuses = item
        .status
        .as_ref()
        .and_then(|status| status.container_statuses.as_deref())
        .unwrap_or(&[]);
    let restart_count = statuses
        .iter()
        .map(|status| u64::try_from(status.restart_count.max(0)).unwrap_or_default())
        .sum::<u64>();

    if let Some(reason) = first_terminated_failure_reason(statuses) {
        return (
            HealthState::Crashed,
            runtime_status(phase, Some(&reason), restart_count),
            restart_count,
            Some(reason),
        );
    }

    if let Some(reason) = first_waiting_reason(statuses) {
        let state = if is_crash_reason(&reason) {
            HealthState::Crashed
        } else if is_degraded_reason(&reason) {
            HealthState::Degraded
        } else {
            HealthState::Starting
        };
        return (
            state,
            runtime_status(phase, Some(&reason), restart_count),
            restart_count,
            Some(reason),
        );
    }

    let state = match phase {
        "Running" => {
            if statuses.is_empty() || statuses.iter().all(|status| status.ready) {
                HealthState::Healthy
            } else {
                HealthState::Starting
            }
        }
        "Pending" => HealthState::Starting,
        "Succeeded" => HealthState::Stopped,
        "Failed" => HealthState::Crashed,
        "Unknown" => HealthState::Unknown,
        _ => HealthState::Unknown,
    };

    (
        state,
        runtime_status(phase, None, restart_count),
        restart_count,
        None,
    )
}

fn runtime_status(phase: &str, reason: Option<&str>, restart_count: u64) -> Option<String> {
    let mut parts = vec![phase.to_string()];
    if let Some(reason) = reason
        && !reason.is_empty()
    {
        parts.push(reason.to_string());
    }
    if restart_count > 0 {
        parts.push(format!("restarts={restart_count}"));
    }
    Some(parts.join("; "))
}

fn first_waiting_reason(statuses: &[KubernetesContainerStatus]) -> Option<String> {
    statuses.iter().find_map(|status| {
        status
            .state
            .as_ref()
            .and_then(|state| state.waiting.as_ref())
            .and_then(|detail| detail.reason.clone())
    })
}

fn first_terminated_failure_reason(statuses: &[KubernetesContainerStatus]) -> Option<String> {
    statuses.iter().find_map(|status| {
        let terminated = status
            .state
            .as_ref()
            .and_then(|state| state.terminated.as_ref())?;
        if terminated.exit_code.unwrap_or_default() == 0 {
            return None;
        }
        Some(
            terminated.reason.clone().unwrap_or_else(|| {
                format!("exit_code={}", terminated.exit_code.unwrap_or_default())
            }),
        )
    })
}

fn is_crash_reason(reason: &str) -> bool {
    matches!(
        reason,
        "CrashLoopBackOff" | "RunContainerError" | "CreateContainerError"
    )
}

fn is_degraded_reason(reason: &str) -> bool {
    matches!(
        reason,
        "ErrImagePull" | "ImagePullBackOff" | "CreateContainerConfigError"
    )
}

#[derive(Debug, serde::Deserialize)]
struct KubernetesPodList {
    #[serde(default)]
    items: Vec<KubernetesPodItem>,
}

#[derive(Debug, serde::Deserialize)]
struct KubernetesPodItem {
    metadata: KubernetesMetadata,
    #[serde(default)]
    spec: Option<KubernetesPodSpec>,
    #[serde(default)]
    status: Option<KubernetesPodStatus>,
}

#[derive(Debug, serde::Deserialize)]
struct KubernetesMetadata {
    name: String,
    #[serde(default)]
    namespace: Option<String>,
    #[serde(default)]
    uid: Option<String>,
    #[serde(default)]
    labels: Option<BTreeMap<String, String>>,
}

#[derive(Debug, serde::Deserialize)]
struct KubernetesPodSpec {
    #[serde(rename = "nodeName")]
    #[serde(default)]
    node_name: Option<String>,
    #[serde(default)]
    containers: Vec<KubernetesContainerSpec>,
}

#[derive(Debug, serde::Deserialize)]
struct KubernetesContainerSpec {
    #[serde(default)]
    ports: Vec<KubernetesContainerPort>,
}

#[derive(Debug, serde::Deserialize)]
struct KubernetesContainerPort {
    #[serde(rename = "containerPort")]
    container_port: u16,
    #[serde(rename = "hostPort")]
    #[serde(default)]
    host_port: Option<u16>,
    #[serde(default)]
    protocol: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct KubernetesPodStatus {
    #[serde(default)]
    phase: Option<String>,
    #[serde(rename = "podIP")]
    #[serde(default)]
    pod_ip: Option<String>,
    #[serde(rename = "hostIP")]
    #[serde(default)]
    host_ip: Option<String>,
    #[serde(rename = "containerStatuses")]
    #[serde(default)]
    container_statuses: Option<Vec<KubernetesContainerStatus>>,
}

#[derive(Debug, serde::Deserialize)]
struct KubernetesContainerStatus {
    #[serde(default)]
    ready: bool,
    #[serde(rename = "restartCount")]
    #[serde(default)]
    restart_count: i32,
    #[serde(default)]
    state: Option<KubernetesContainerState>,
}

#[derive(Debug, serde::Deserialize)]
struct KubernetesContainerState {
    #[serde(default)]
    waiting: Option<KubernetesStateDetail>,
    #[serde(default)]
    terminated: Option<KubernetesStateDetail>,
}

#[derive(Debug, serde::Deserialize)]
struct KubernetesStateDetail {
    #[serde(default)]
    reason: Option<String>,
    #[serde(rename = "exitCode")]
    #[serde(default)]
    exit_code: Option<i32>,
}

#[cfg(test)]
#[allow(clippy::await_holding_lock)]
mod tests {
    use super::{
        collect, first_terminated_failure_reason, first_waiting_reason, is_crash_reason,
        is_degraded_reason, parse_pod_list, pod_ports, pod_state,
    };
    use crate::command::{command_overrides, test_lock, write_script};
    use giggity_core::config::Config;
    use giggity_core::model::HealthState;
    use tempfile::tempdir;

    fn reset_overrides() {
        command_overrides().lock().expect("lock").clear();
    }

    #[test]
    fn parses_running_and_crashing_pods() {
        let resources = parse_pod_list(
            r#"{
              "items": [
                {
                  "metadata": {
                    "name": "api-7b5d9c",
                    "namespace": "dev",
                    "uid": "uid-1",
                    "labels": {"app": "api"}
                  },
                  "spec": {
                    "node_name": "minikube",
                    "containers": [
                      {"ports": [{"containerPort": 8080, "hostPort": 8080, "protocol": "TCP"}]}
                    ]
                  },
                  "status": {
                    "phase": "Running",
                    "podIP": "10.42.0.5",
                    "hostIP": "192.168.64.2",
                    "containerStatuses": [
                      {"ready": true, "restartCount": 1, "state": {}}
                    ]
                  }
                },
                {
                  "metadata": {"name": "worker-123", "namespace": "dev"},
                  "status": {
                    "phase": "Running",
                    "containerStatuses": [
                      {"ready": false, "restartCount": 3, "state": {"waiting": {"reason": "CrashLoopBackOff"}}}
                    ]
                  }
                }
              ]
            }"#,
            "kind-dev",
        )
        .expect("pods");

        assert_eq!(resources.len(), 2);
        assert_eq!(resources[0].state, HealthState::Healthy);
        assert_eq!(resources[0].project.as_deref(), Some("dev"));
        assert_eq!(resources[0].metadata["cluster_context"], "kind-dev");
        assert_eq!(resources[0].ports[0].host_port, 8080);
        assert_eq!(resources[1].state, HealthState::Crashed);
        assert_eq!(resources[1].summary_name(), "dev/worker-123");
    }

    #[test]
    fn pod_state_covers_pending_succeeded_and_image_pull_failures() {
        let pending: super::KubernetesPodItem = serde_json::from_str(
            r#"{
              "metadata": {"name": "web", "namespace": "dev"},
              "status": {"phase": "Pending", "containerStatuses": [{"ready": false, "restartCount": 0, "state": {"waiting": {"reason": "ContainerCreating"}}}]}
            }"#,
        )
        .expect("pending pod");
        let succeeded: super::KubernetesPodItem = serde_json::from_str(
            r#"{
              "metadata": {"name": "job", "namespace": "dev"},
              "status": {"phase": "Succeeded", "containerStatuses": [{"ready": false, "restartCount": 0, "state": {"terminated": {"reason": "Completed", "exitCode": 0}}}]}
            }"#,
        )
        .expect("succeeded pod");
        let degraded: super::KubernetesPodItem = serde_json::from_str(
            r#"{
              "metadata": {"name": "puller", "namespace": "dev"},
              "status": {"phase": "Pending", "containerStatuses": [{"ready": false, "restartCount": 0, "state": {"waiting": {"reason": "ImagePullBackOff"}}}]}
            }"#,
        )
        .expect("degraded pod");

        assert_eq!(pod_state(&pending).0, HealthState::Starting);
        assert_eq!(pod_state(&succeeded).0, HealthState::Stopped);
        assert_eq!(pod_state(&degraded).0, HealthState::Degraded);
    }

    #[test]
    fn pod_state_covers_terminated_failed_not_ready_failed_and_unknown_phases() {
        let terminated: super::KubernetesPodItem = serde_json::from_str(
            r#"{
              "metadata": {"name": "boom", "namespace": "dev"},
              "status": {"phase": "Running", "containerStatuses": [{"ready": false, "restartCount": 1, "state": {"terminated": {"exitCode": 42}}}]}
            }"#,
        )
        .expect("terminated pod");
        let running_not_ready: super::KubernetesPodItem = serde_json::from_str(
            r#"{
              "metadata": {"name": "warming", "namespace": "dev"},
              "status": {"phase": "Running", "containerStatuses": [{"ready": false, "restartCount": 0, "state": {}}]}
            }"#,
        )
        .expect("warming pod");
        let failed: super::KubernetesPodItem = serde_json::from_str(
            r#"{
              "metadata": {"name": "failed", "namespace": "dev"},
              "status": {"phase": "Failed"}
            }"#,
        )
        .expect("failed pod");
        let mystery: super::KubernetesPodItem = serde_json::from_str(
            r#"{
              "metadata": {"name": "mystery", "namespace": "dev"},
              "status": {"phase": "TotallyUnknown"}
            }"#,
        )
        .expect("mystery pod");

        let terminated_state = pod_state(&terminated);
        assert_eq!(terminated_state.0, HealthState::Crashed);
        assert_eq!(
            terminated_state.1.as_deref(),
            Some("Running; exit_code=42; restarts=1")
        );
        assert_eq!(pod_state(&running_not_ready).0, HealthState::Starting);
        assert_eq!(pod_state(&failed).0, HealthState::Crashed);
        assert_eq!(pod_state(&mystery).0, HealthState::Unknown);
    }

    #[test]
    fn pod_state_covers_running_without_statuses_and_successful_terminations() {
        let running_empty: super::KubernetesPodItem = serde_json::from_str(
            r#"{
              "metadata": {"name": "api", "namespace": "dev"},
              "status": {"phase": "Running", "containerStatuses": []}
            }"#,
        )
        .expect("running empty pod");
        let terminated_success: Vec<super::KubernetesContainerStatus> = serde_json::from_str(
            r#"[{"ready":false,"restartCount":0,"state":{"terminated":{"reason":"Completed","exitCode":0}}}]"#,
        )
        .expect("terminated success");

        let running_state = pod_state(&running_empty);
        assert_eq!(running_state.0, HealthState::Healthy);
        assert_eq!(running_state.1.as_deref(), Some("Running"));
        assert!(first_terminated_failure_reason(&terminated_success).is_none());
    }

    #[test]
    fn pod_ports_and_reason_helpers_cover_duplicates_and_missing_values() {
        let item: super::KubernetesPodItem = serde_json::from_str(
            r#"{
              "metadata": {"name": "api", "namespace": "dev", "uid": "uid-1"},
              "spec": {
                "nodeName": "kind-worker",
                "containers": [
                  {"ports": [{"containerPort": 8080, "hostPort": 18080, "protocol": "TCP"}]},
                  {"ports": [{"containerPort": 8081, "hostPort": 18080, "protocol": "TCP"}, {"containerPort": 8082}]}
                ]
              },
              "status": null
            }"#,
        )
        .expect("pod");

        let resources = parse_pod_list(
            r#"{
              "items": [{
                "metadata": {"name": "api", "namespace": "dev", "uid": "uid-1"},
                "spec": {
                  "nodeName": "kind-worker",
                  "containers": [
                    {"ports": [{"containerPort": 8080, "hostPort": 18080, "protocol": "TCP"}]},
                    {"ports": [{"containerPort": 8081, "hostPort": 18080, "protocol": "TCP"}, {"containerPort": 8082}]}
                  ]
                },
                "status": null
              }]
            }"#,
            "kind-dev",
        )
        .expect("pods");

        assert_eq!(pod_ports(item.spec.as_ref()).len(), 1);
        assert_eq!(resources[0].metadata["uid"], "uid-1");
        assert_eq!(resources[0].metadata["node_name"], "kind-worker");
        assert_eq!(resources[0].state, HealthState::Unknown);

        let waiting: Vec<super::KubernetesContainerStatus> = serde_json::from_str(
            r#"[{"ready":false,"restartCount":0,"state":{"waiting":{"reason":"CrashLoopBackOff"}}}]"#,
        )
        .expect("waiting statuses");
        let terminated: Vec<super::KubernetesContainerStatus> = serde_json::from_str(
            r#"[{"ready":false,"restartCount":0,"state":{"terminated":{"exitCode":5}}}]"#,
        )
        .expect("terminated statuses");
        assert_eq!(
            first_waiting_reason(&waiting).as_deref(),
            Some("CrashLoopBackOff")
        );
        assert_eq!(
            first_terminated_failure_reason(&terminated).as_deref(),
            Some("exit_code=5")
        );
        assert!(is_crash_reason("RunContainerError"));
        assert!(!is_crash_reason("ContainerCreating"));
        assert!(is_degraded_reason("CreateContainerConfigError"));
        assert!(!is_degraded_reason("ContainerCreating"));
    }

    #[tokio::test]
    async fn collect_covers_disabled_source_empty_context_and_invalid_json() {
        let mut disabled = Config::default();
        disabled.sources.kubernetes = false;
        let output = collect(&disabled).await;
        assert!(output.resources.is_empty());
        assert!(output.warnings.is_empty());

        let _guard = test_lock().lock().expect("lock");
        reset_overrides();
        let dir = tempdir().expect("tempdir");
        let kubectl = write_script(
            dir.path(),
            "kubectl",
            r#"if [ "$1" = "config" ]; then
  printf '\n'
else
  printf '{"items":[]}'
fi"#,
        );
        command_overrides()
            .lock()
            .expect("lock")
            .insert("kubectl".into(), kubectl);
        let output = collect(&Config::default()).await;
        assert!(output.resources.is_empty());
        assert!(output.warnings.is_empty());

        let dir = tempdir().expect("tempdir");
        let kubectl = write_script(
            dir.path(),
            "kubectl",
            r#"if [ "$1" = "config" ]; then
  printf 'kind-dev\n'
else
  printf 'not-json'
fi"#,
        );
        command_overrides()
            .lock()
            .expect("lock")
            .insert("kubectl".into(), kubectl);
        let output = collect(&Config::default()).await;
        assert_eq!(output.warnings.len(), 1);
        reset_overrides();
    }

    #[tokio::test]
    async fn collect_skips_missing_or_unconfigured_kubectl_without_warning() {
        let _guard = test_lock().lock().expect("lock");
        reset_overrides();
        let dir = tempdir().expect("tempdir");
        let kubectl = write_script(
            dir.path(),
            "kubectl",
            "echo 'current-context is not set' >&2; exit 1",
        );
        command_overrides()
            .lock()
            .expect("lock")
            .insert("kubectl".into(), kubectl);

        let mut config = Config::default();
        config.sources.kubernetes = true;
        let output = collect(&config).await;
        assert!(output.resources.is_empty());
        assert!(output.warnings.is_empty());
        reset_overrides();
    }

    #[tokio::test]
    async fn collect_skips_ignorable_get_pods_errors_without_warning() {
        let _guard = test_lock().lock().expect("lock");
        reset_overrides();
        let dir = tempdir().expect("tempdir");
        let kubectl = write_script(
            dir.path(),
            "kubectl",
            r#"if [ "$1" = "config" ]; then
  printf 'kind-dev\n'
else
  echo 'No configuration has been provided' >&2
  exit 1
fi"#,
        );
        command_overrides()
            .lock()
            .expect("lock")
            .insert("kubectl".into(), kubectl);

        let output = collect(&Config::default()).await;
        assert!(output.resources.is_empty());
        assert!(output.warnings.is_empty());
        reset_overrides();
    }

    #[tokio::test]
    async fn collect_uses_kubectl_context_and_pod_listing() {
        let _guard = test_lock().lock().expect("lock");
        reset_overrides();
        let dir = tempdir().expect("tempdir");
        let kubectl = write_script(
            dir.path(),
            "kubectl",
            r#"if [ "$1" = "config" ]; then
  printf 'kind-dev\n'
else
  printf '{"items":[{"metadata":{"name":"api","namespace":"dev"},"status":{"phase":"Running","containerStatuses":[{"ready":true,"restartCount":2,"state":{}}]}}]}'
fi"#,
        );
        command_overrides()
            .lock()
            .expect("lock")
            .insert("kubectl".into(), kubectl);

        let output = collect(&Config::default()).await;
        assert!(output.warnings.is_empty());
        assert_eq!(output.resources.len(), 1);
        assert_eq!(
            output.resources[0].runtime_status.as_deref(),
            Some("Running; restarts=2")
        );
        assert_eq!(output.resources[0].metadata["cluster_context"], "kind-dev");
        reset_overrides();
    }

    #[tokio::test]
    async fn collect_surfaces_real_kubectl_errors() {
        let _guard = test_lock().lock().expect("lock");
        reset_overrides();
        let dir = tempdir().expect("tempdir");
        let kubectl = write_script(
            dir.path(),
            "kubectl",
            r#"if [ "$1" = "config" ]; then
  printf 'kind-dev\n'
else
  echo 'The connection to the server 127.0.0.1 was refused' >&2
  exit 1
fi"#,
        );
        command_overrides()
            .lock()
            .expect("lock")
            .insert("kubectl".into(), kubectl);

        let mut config = Config::default();
        config.sources.kubernetes = true;
        let output = collect(&config).await;
        assert!(output.resources.is_empty());
        assert_eq!(output.warnings.len(), 1);
        assert_eq!(output.warnings[0].source, "kubernetes");
        reset_overrides();
    }

    #[tokio::test]
    async fn collect_surfaces_current_context_errors() {
        let _guard = test_lock().lock().expect("lock");
        reset_overrides();
        let dir = tempdir().expect("tempdir");
        let kubectl = write_script(
            dir.path(),
            "kubectl",
            r#"echo 'context lookup failed' >&2
exit 1"#,
        );
        command_overrides()
            .lock()
            .expect("lock")
            .insert("kubectl".into(), kubectl);

        let mut config = Config::default();
        config.sources.kubernetes = true;
        let output = collect(&config).await;
        assert!(output.resources.is_empty());
        assert_eq!(output.warnings.len(), 1);
        assert_eq!(output.warnings[0].source, "kubernetes");
        assert!(output.warnings[0].message.contains("context lookup failed"));
        reset_overrides();
    }
}
