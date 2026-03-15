use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use chrono::Utc;
use giggity_core::config::{ProbeKind, ProbeSpec};
use giggity_core::model::{HealthState, ResourceKind, ResourceRecord, RuntimeKind};
use tracing::debug;

/// Execute all configured probes and return a `ResourceRecord` per probe.
///
/// Each probe becomes a standalone resource with `ResourceKind::Probe` and
/// `RuntimeKind::Probes`. The resulting health state depends on:
///
/// - Connection success/failure and expected status codes
/// - Latency thresholds (`warn_latency_ms` / `critical_latency_ms`)
/// - Retry policy (`retries` / `backoff_secs`)
pub async fn collect_probes(probes: &[ProbeSpec]) -> Vec<ResourceRecord> {
    if probes.is_empty() {
        return Vec::new();
    }

    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(3))
        .build()
        .ok();

    let mut records = Vec::with_capacity(probes.len());
    for probe in probes {
        let record = execute_probe(client.as_ref(), probe).await;
        records.push(record);
    }

    records
}

async fn execute_probe(client: Option<&reqwest::Client>, probe: &ProbeSpec) -> ResourceRecord {
    let max_attempts = probe.retries.saturating_add(1);
    let mut last_error: Option<String> = None;
    let mut latency_ms: u64 = 0;

    for attempt in 0..max_attempts {
        if attempt > 0 {
            let backoff = Duration::from_secs(probe.backoff_secs.saturating_mul(attempt as u64));
            tokio::time::sleep(backoff).await;
            debug!(
                probe = %probe.name,
                attempt,
                "retrying probe after backoff"
            );
        }

        let start = Instant::now();
        let result = run_probe_check(client, probe).await;
        latency_ms = start.elapsed().as_millis() as u64;

        match result {
            Ok(()) => {
                last_error = None;
                break;
            }
            Err(err) => {
                last_error = Some(err);
            }
        }
    }

    let now = Utc::now();
    let last_check = now.to_rfc3339();
    let probe_type_str = match probe.probe_type {
        giggity_core::config::ProbeType::Http => "http",
        giggity_core::config::ProbeType::Grpc => "grpc",
        giggity_core::config::ProbeType::Tcp => "tcp",
    };

    let state = match &last_error {
        Some(err) => {
            debug!(probe = %probe.name, error = %err, "probe failed");
            HealthState::Crashed
        }
        None => classify_latency(probe, latency_ms),
    };

    let mut metadata = BTreeMap::new();
    metadata.insert("latency_ms".into(), latency_ms.to_string());
    metadata.insert("last_check".into(), last_check);
    metadata.insert("probe_type".into(), probe_type_str.into());
    if let Some(err) = &last_error {
        metadata.insert("error".into(), err.clone());
    }
    if let Some(warn_ms) = probe.warn_latency_ms {
        metadata.insert("warn_latency_ms".into(), warn_ms.to_string());
    }
    if let Some(critical_ms) = probe.critical_latency_ms {
        metadata.insert("critical_latency_ms".into(), critical_ms.to_string());
    }

    let runtime_status = match &last_error {
        Some(err) => Some(format!("failed: {err}")),
        None if state == HealthState::Degraded => Some(format!("slow: {latency_ms}ms")),
        None => Some(format!("ok: {latency_ms}ms")),
    };

    ResourceRecord {
        id: format!("probe:{}", probe.name),
        kind: ResourceKind::Probe,
        runtime: RuntimeKind::Probes,
        project: None,
        name: probe.name.clone(),
        state,
        runtime_status,
        ports: Vec::new(),
        labels: BTreeMap::new(),
        urls: Vec::new(),
        metadata,
        last_changed: now,
        state_since: now,
    }
}

fn classify_latency(probe: &ProbeSpec, latency_ms: u64) -> HealthState {
    if let Some(critical_ms) = probe.critical_latency_ms
        && latency_ms > critical_ms
    {
        return HealthState::Crashed;
    }
    if let Some(warn_ms) = probe.warn_latency_ms
        && latency_ms > warn_ms
    {
        return HealthState::Degraded;
    }
    HealthState::Healthy
}

async fn run_probe_check(
    client: Option<&reqwest::Client>,
    probe: &ProbeSpec,
) -> Result<(), String> {
    match &probe.kind {
        ProbeKind::Http {
            url,
            expected_status,
        } => run_http_probe(client, url, *expected_status, probe.timeout_millis).await,
        ProbeKind::Tcp { host, port } => {
            let host = host.as_deref().unwrap_or("127.0.0.1");
            let port = port.ok_or_else(|| "tcp probe has no port configured".to_string())?;
            run_tcp_probe(host, port, probe.timeout_millis).await
        }
        ProbeKind::Command {
            program,
            args,
            contains,
        } => run_command_probe(program, args, contains.as_deref(), probe.timeout_millis).await,
    }
}

async fn run_http_probe(
    client: Option<&reqwest::Client>,
    url: &str,
    expected_status: u16,
    timeout_millis: u64,
) -> Result<(), String> {
    let client = client.ok_or_else(|| "http client unavailable".to_string())?;
    let response = tokio::time::timeout(
        Duration::from_millis(timeout_millis),
        client.get(url).send(),
    )
    .await
    .map_err(|_| "http probe timed out".to_string())?
    .map_err(|e| format!("http request failed: {e}"))?;

    let status = response.status().as_u16();
    if status == expected_status {
        Ok(())
    } else {
        Err(format!("expected {expected_status}, got {status}"))
    }
}

async fn run_tcp_probe(host: &str, port: u16, timeout_millis: u64) -> Result<(), String> {
    tokio::time::timeout(
        Duration::from_millis(timeout_millis),
        tokio::net::TcpStream::connect((host, port)),
    )
    .await
    .map_err(|_| "tcp probe timed out".to_string())?
    .map_err(|e| format!("tcp connect failed: {e}"))?;
    Ok(())
}

async fn run_command_probe(
    program: &str,
    args: &[String],
    contains: Option<&str>,
    timeout_millis: u64,
) -> Result<(), String> {
    let output = tokio::time::timeout(
        Duration::from_millis(timeout_millis),
        tokio::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output(),
    )
    .await
    .map_err(|_| "command probe timed out".to_string())?
    .map_err(|e| format!("command spawn failed: {e}"))?;

    if !output.status.success() {
        return Err(format!("command exited with {}", output.status));
    }
    if let Some(expected) = contains {
        let stdout = String::from_utf8_lossy(&output.stdout);
        if !stdout.contains(expected) {
            return Err(format!("output missing '{expected}'"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use giggity_core::config::{ProbeKind, ProbeSpec, ProbeType};
    use giggity_core::model::HealthState;

    use super::{classify_latency, collect_probes};

    fn http_probe(name: &str, url: &str) -> ProbeSpec {
        ProbeSpec {
            name: name.into(),
            matcher: Default::default(),
            kind: ProbeKind::Http {
                url: url.into(),
                expected_status: 200,
            },
            timeout_millis: 500,
            retries: 0,
            backoff_secs: 1,
            warn_latency_ms: None,
            critical_latency_ms: None,
            interval_secs: 30,
            probe_type: ProbeType::Http,
        }
    }

    fn tcp_probe(name: &str, host: &str, port: u16) -> ProbeSpec {
        ProbeSpec {
            name: name.into(),
            matcher: Default::default(),
            kind: ProbeKind::Tcp {
                host: Some(host.into()),
                port: Some(port),
            },
            timeout_millis: 500,
            retries: 0,
            backoff_secs: 1,
            warn_latency_ms: None,
            critical_latency_ms: None,
            interval_secs: 30,
            probe_type: ProbeType::Tcp,
        }
    }

    #[test]
    fn classify_latency_returns_healthy_when_no_thresholds() {
        let probe = http_probe("test", "http://localhost");
        assert_eq!(classify_latency(&probe, 500), HealthState::Healthy);
    }

    #[test]
    fn classify_latency_returns_degraded_above_warn_threshold() {
        let mut probe = http_probe("test", "http://localhost");
        probe.warn_latency_ms = Some(100);
        assert_eq!(classify_latency(&probe, 50), HealthState::Healthy);
        assert_eq!(classify_latency(&probe, 150), HealthState::Degraded);
    }

    #[test]
    fn classify_latency_returns_crashed_above_critical_threshold() {
        let mut probe = http_probe("test", "http://localhost");
        probe.warn_latency_ms = Some(100);
        probe.critical_latency_ms = Some(500);
        assert_eq!(classify_latency(&probe, 600), HealthState::Crashed);
        assert_eq!(classify_latency(&probe, 150), HealthState::Degraded);
        assert_eq!(classify_latency(&probe, 50), HealthState::Healthy);
    }

    #[test]
    fn classify_latency_critical_without_warn() {
        let mut probe = http_probe("test", "http://localhost");
        probe.critical_latency_ms = Some(200);
        assert_eq!(classify_latency(&probe, 100), HealthState::Healthy);
        assert_eq!(classify_latency(&probe, 250), HealthState::Crashed);
    }

    #[tokio::test]
    async fn collect_probes_returns_empty_for_empty_input() {
        let result = collect_probes(&[]).await;
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn collect_probes_http_unreachable_returns_crashed() {
        let probe = http_probe("bad-http", "http://127.0.0.1:19999/nonexistent");
        let records = collect_probes(&[probe]).await;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].id, "probe:bad-http");
        assert_eq!(records[0].kind, giggity_core::model::ResourceKind::Probe);
        assert_eq!(records[0].runtime, giggity_core::model::RuntimeKind::Probes);
        assert_eq!(records[0].state, HealthState::Crashed);
        assert!(records[0].metadata.contains_key("latency_ms"));
        assert!(records[0].metadata.contains_key("last_check"));
        assert_eq!(records[0].metadata["probe_type"], "http");
        assert!(records[0].metadata.contains_key("error"));
    }

    #[tokio::test]
    async fn collect_probes_tcp_unreachable_returns_crashed() {
        let probe = tcp_probe("bad-tcp", "127.0.0.1", 19998);
        let records = collect_probes(&[probe]).await;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].state, HealthState::Crashed);
        assert_eq!(records[0].metadata["probe_type"], "tcp");
    }

    #[tokio::test]
    async fn collect_probes_tcp_missing_port_returns_crashed() {
        let probe = ProbeSpec {
            name: "no-port".into(),
            matcher: Default::default(),
            kind: ProbeKind::Tcp {
                host: Some("127.0.0.1".into()),
                port: None,
            },
            timeout_millis: 500,
            retries: 0,
            backoff_secs: 1,
            warn_latency_ms: None,
            critical_latency_ms: None,
            interval_secs: 30,
            probe_type: ProbeType::Tcp,
        };
        let records = collect_probes(&[probe]).await;
        assert_eq!(records[0].state, HealthState::Crashed);
        assert!(records[0].metadata["error"].contains("no port"));
    }

    #[tokio::test]
    async fn collect_probes_retries_on_failure() {
        let mut probe = http_probe("retry-test", "http://127.0.0.1:19997/nope");
        probe.retries = 1;
        probe.backoff_secs = 0;
        let records = collect_probes(&[probe]).await;
        // Should still fail after retries (unreachable host)
        assert_eq!(records[0].state, HealthState::Crashed);
    }

    #[tokio::test]
    async fn collect_probes_sets_metadata_fields() {
        let probe = tcp_probe("meta-test", "127.0.0.1", 19996);
        let records = collect_probes(&[probe]).await;
        let r = &records[0];
        assert!(r.metadata.contains_key("latency_ms"));
        assert!(r.metadata.contains_key("last_check"));
        assert!(r.metadata.contains_key("probe_type"));
        assert!(r.runtime_status.is_some());
    }

    #[tokio::test]
    async fn collect_probes_multiple_probes() {
        let probes = vec![
            tcp_probe("tcp-a", "127.0.0.1", 19995),
            http_probe("http-b", "http://127.0.0.1:19994/nope"),
        ];
        let records = collect_probes(&probes).await;
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].name, "tcp-a");
        assert_eq!(records[1].name, "http-b");
    }

    #[tokio::test]
    async fn collect_probes_command_probe_success() {
        let probe = ProbeSpec {
            name: "echo-test".into(),
            matcher: Default::default(),
            kind: ProbeKind::Command {
                program: "echo".into(),
                args: vec!["hello".into()],
                contains: Some("hello".into()),
            },
            timeout_millis: 5_000,
            retries: 0,
            backoff_secs: 1,
            warn_latency_ms: None,
            critical_latency_ms: None,
            interval_secs: 30,
            probe_type: ProbeType::Http, // probe_type field is informational
        };
        let records = collect_probes(&[probe]).await;
        assert_eq!(records[0].state, HealthState::Healthy);
    }

    #[tokio::test]
    async fn collect_probes_command_probe_missing_output() {
        let probe = ProbeSpec {
            name: "echo-miss".into(),
            matcher: Default::default(),
            kind: ProbeKind::Command {
                program: "echo".into(),
                args: vec!["hello".into()],
                contains: Some("world".into()),
            },
            timeout_millis: 5_000,
            retries: 0,
            backoff_secs: 1,
            warn_latency_ms: None,
            critical_latency_ms: None,
            interval_secs: 30,
            probe_type: ProbeType::Http,
        };
        let records = collect_probes(&[probe]).await;
        assert_eq!(records[0].state, HealthState::Crashed);
        assert!(records[0].metadata["error"].contains("missing"));
    }

    #[tokio::test]
    async fn collect_probes_command_probe_failure() {
        let probe = ProbeSpec {
            name: "false-test".into(),
            matcher: Default::default(),
            kind: ProbeKind::Command {
                program: "false".into(),
                args: vec![],
                contains: None,
            },
            timeout_millis: 5_000,
            retries: 0,
            backoff_secs: 1,
            warn_latency_ms: None,
            critical_latency_ms: None,
            interval_secs: 30,
            probe_type: ProbeType::Http,
        };
        let records = collect_probes(&[probe]).await;
        assert_eq!(records[0].state, HealthState::Crashed);
    }

    #[tokio::test]
    async fn collect_probes_grpc_falls_back_to_tcp() {
        // gRPC probes use TCP connect check via the Tcp variant
        let probe = ProbeSpec {
            name: "grpc-test".into(),
            matcher: Default::default(),
            kind: ProbeKind::Tcp {
                host: Some("127.0.0.1".into()),
                port: Some(19993),
            },
            timeout_millis: 500,
            retries: 0,
            backoff_secs: 1,
            warn_latency_ms: None,
            critical_latency_ms: None,
            interval_secs: 30,
            probe_type: ProbeType::Grpc,
        };
        let records = collect_probes(&[probe]).await;
        assert_eq!(records[0].state, HealthState::Crashed);
        assert_eq!(records[0].metadata["probe_type"], "grpc");
    }
}
