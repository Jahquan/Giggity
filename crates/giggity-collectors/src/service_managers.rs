use std::collections::BTreeMap;

use giggity_core::config::Config;
use giggity_core::model::{
    CollectorWarning, HealthState, ResourceKind, ResourceRecord, RuntimeKind,
};

use crate::CollectionOutput;
use crate::command::run_command;

pub async fn collect(config: &Config) -> CollectionOutput {
    let mut output = CollectionOutput::default();

    #[cfg(target_os = "macos")]
    if config.sources.launchd {
        match collect_launchd().await {
            Ok(resources) => output.resources.extend(resources),
            Err(error) => output.warnings.push(CollectorWarning {
                source: "launchd".into(),
                message: error.to_string(),
            }),
        }
    }

    #[cfg(target_os = "linux")]
    if config.sources.systemd {
        match collect_systemd().await {
            Ok(resources) => output.resources.extend(resources),
            Err(error) => output.warnings.push(CollectorWarning {
                source: "systemd".into(),
                message: error.to_string(),
            }),
        }
    }

    output
}

async fn collect_launchd() -> anyhow::Result<Vec<ResourceRecord>> {
    let output = run_command("service_manager", "launchctl", &["list"]).await?;
    Ok(parse_launchctl_list(&output))
}

#[cfg(target_os = "linux")]
async fn collect_systemd() -> anyhow::Result<Vec<ResourceRecord>> {
    let mut resources = Vec::new();
    for (scope, args) in [
        (
            "user",
            vec![
                "--user",
                "--all",
                "--plain",
                "--no-legend",
                "--type=service",
                "list-units",
            ],
        ),
        (
            "system",
            vec![
                "--all",
                "--plain",
                "--no-legend",
                "--type=service",
                "list-units",
            ],
        ),
    ] {
        if let Ok(output) = run_command("service_manager", "systemctl", &args).await {
            resources.extend(parse_systemctl_list(&output, scope));
        }
    }
    Ok(resources)
}

pub fn parse_launchctl_list(output: &str) -> Vec<ResourceRecord> {
    output
        .lines()
        .skip(1)
        .filter_map(|line| {
            let columns: Vec<_> = line.split_whitespace().collect();
            if columns.len() < 3 {
                return None;
            }
            let pid = columns[0];
            let status = columns[1];
            let label = columns[2].to_string();
            let state = if pid != "-" {
                HealthState::Healthy
            } else if status == "0" {
                HealthState::Stopped
            } else {
                HealthState::Crashed
            };

            Some(ResourceRecord {
                id: format!("launchd:{label}"),
                kind: ResourceKind::LaunchdUnit,
                runtime: RuntimeKind::Launchd,
                project: None,
                name: label.clone(),
                state,
                runtime_status: Some(status.to_string()),
                ports: Vec::new(),
                labels: BTreeMap::new(),
                urls: Vec::new(),
                metadata: BTreeMap::from([
                    ("pid".into(), pid.to_string()),
                    ("domain".into(), "user".into()),
                ]),
                last_changed: chrono::Utc::now(),
                state_since: chrono::Utc::now(),
            })
        })
        .collect()
}

pub fn parse_systemctl_list(output: &str, scope: &str) -> Vec<ResourceRecord> {
    output
        .lines()
        .filter_map(|line| {
            let columns: Vec<_> = line.split_whitespace().collect();
            if columns.len() < 5 {
                return None;
            }
            let unit = columns[0].to_string();
            let active = columns[2];
            let sub = columns[3];
            let description = columns[4..].join(" ");
            let state = match (active, sub) {
                ("active", "running") => HealthState::Healthy,
                ("active", "exited") => HealthState::Stopped,
                ("activating", _) | ("reloading", _) => HealthState::Starting,
                ("failed", _) => HealthState::Crashed,
                ("inactive", _) => HealthState::Stopped,
                _ => HealthState::Unknown,
            };
            Some(ResourceRecord {
                id: format!("systemd:{scope}:{unit}"),
                kind: ResourceKind::SystemdUnit,
                runtime: RuntimeKind::Systemd,
                project: None,
                name: unit.clone(),
                state,
                runtime_status: Some(format!("{active}/{sub}")),
                ports: Vec::new(),
                labels: BTreeMap::new(),
                urls: Vec::new(),
                metadata: BTreeMap::from([
                    ("domain".into(), scope.to_string()),
                    ("description".into(), description),
                ]),
                last_changed: chrono::Utc::now(),
                state_since: chrono::Utc::now(),
            })
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::await_holding_lock)]
mod tests {
    use super::{collect, parse_launchctl_list, parse_systemctl_list};
    use crate::command::{command_overrides, run_command, test_lock, write_script};
    use giggity_core::config::Config;
    use giggity_core::model::HealthState;
    use tempfile::tempdir;

    fn reset_overrides() {
        command_overrides().lock().expect("lock").clear();
    }

    #[test]
    fn parses_launchctl_rows() {
        let parsed = parse_launchctl_list(
            "PID Status Label\n123 0 com.example.web\n- 78 com.example.worker\n",
        );
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].state, HealthState::Healthy);
        assert_eq!(parsed[1].state, HealthState::Crashed);
    }

    #[test]
    fn parses_launchctl_stopped_rows() {
        let parsed = parse_launchctl_list("PID Status Label\n- 0 com.example.idle\n");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].state, HealthState::Stopped);
        assert_eq!(parsed[0].metadata["domain"], "user");
    }

    #[test]
    fn parses_systemctl_rows() {
        let parsed = parse_systemctl_list(
            "docker.service loaded active running Docker Application Container Engine\ncron.service loaded failed failed Command Scheduler\n",
            "system",
        );
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].state, HealthState::Healthy);
        assert_eq!(parsed[1].state, HealthState::Crashed);
    }

    #[test]
    fn parses_systemctl_starting_and_stopped_rows() {
        let parsed = parse_systemctl_list(
            "api.service loaded activating start-pre API bootstrap\nworker.service loaded inactive dead Background Worker\ncache.service loaded active exited Cache Warmup\n",
            "user",
        );
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].state, HealthState::Starting);
        assert_eq!(parsed[1].state, HealthState::Stopped);
        assert_eq!(parsed[2].state, HealthState::Stopped);
        assert_eq!(parsed[0].metadata["domain"], "user");
    }

    #[test]
    fn parsers_ignore_short_rows_and_cover_unknown_systemd_states() {
        assert!(parse_launchctl_list("PID Status Label\nmalformed\n").is_empty());

        let parsed = parse_systemctl_list(
            "short\nodd.service loaded active maintenance Odd Service\n",
            "system",
        );
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].state, HealthState::Unknown);
    }

    #[tokio::test]
    async fn run_command_reports_stdout_and_failures() {
        let ok = run_command("service_manager", "sh", &["-c", "printf ok"])
            .await
            .expect("stdout");
        assert_eq!(ok, "ok");

        let error = run_command("service_manager", "sh", &["-c", "echo bad >&2; exit 4"])
            .await
            .expect_err("failure");
        assert!(error.to_string().contains("sh failed: bad"));
    }

    #[tokio::test]
    async fn collect_launchd_uses_override_command() {
        let _guard = test_lock().lock().expect("lock");
        reset_overrides();
        let dir = tempdir().expect("tempdir");
        let launchctl = write_script(
            dir.path(),
            "launchctl",
            "printf 'PID Status Label\n123 0 com.example.web\n'",
        );
        command_overrides()
            .lock()
            .expect("lock")
            .insert("launchctl".into(), launchctl);

        let mut config = Config::default();
        config.sources.launchd = true;
        config.sources.systemd = true;
        let output = collect(&config).await;
        assert!(output.warnings.is_empty());
        assert!(
            output
                .resources
                .iter()
                .any(|resource| resource.name == "com.example.web")
        );
        reset_overrides();
    }

    #[tokio::test]
    async fn collect_launchd_surfaces_warning_on_failure() {
        let _guard = test_lock().lock().expect("lock");
        reset_overrides();
        let dir = tempdir().expect("tempdir");
        let launchctl = write_script(dir.path(), "launchctl", "echo nope >&2; exit 1");
        command_overrides()
            .lock()
            .expect("lock")
            .insert("launchctl".into(), launchctl);

        let mut config = Config::default();
        config.sources.launchd = true;
        let output = collect(&config).await;
        assert!(output.resources.is_empty());
        assert!(
            output
                .warnings
                .iter()
                .any(|warning| warning.source == "launchd")
        );
        reset_overrides();
    }
}
