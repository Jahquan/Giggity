use std::collections::BTreeMap;
use std::path::Path;
use std::sync::LazyLock;

use giggity_core::config::Config;
use giggity_core::model::{
    CollectorWarning, HealthState, PortBinding, ResourceKind, ResourceRecord, RuntimeKind,
    guess_local_url,
};
use regex::Regex;

use crate::CollectionOutput;
use crate::command::run_command;

static SS_LISTEN_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"LISTEN.+?(?P<local>\S+:\d+)\s+\S+\s+users:\(\("(?P<command>[^"]+)",pid=(?P<pid>\d+),fd=\d+\)\)"#,
    )
    .expect("valid ss regex")
});

static NETSTAT_LISTEN_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^tcp\d?\s+\d+\s+\d+\s+\S+\.(?P<port>\d+)\s+\S+\s+LISTEN")
        .expect("valid netstat regex")
});

pub async fn collect(config: &Config) -> CollectionOutput {
    if !config.sources.host_listeners {
        return CollectionOutput::default();
    }

    let mut output = CollectionOutput::default();
    match collect_host_processes().await {
        Ok(resources) => output.resources.extend(resources),
        Err(error) => output.warnings.push(CollectorWarning {
            source: "host".into(),
            message: error.to_string(),
        }),
    }
    output
}

async fn collect_host_processes() -> anyhow::Result<Vec<ResourceRecord>> {
    if let Ok(output) = run_command("host", "lsof", &["-nP", "-iTCP", "-sTCP:LISTEN"]).await {
        let mut resources = parse_lsof_listeners(&output);
        enrich_host_processes(&mut resources).await;
        return Ok(resources);
    }
    #[cfg(target_os = "linux")]
    if let Some(output) = collect_from_ss().await? {
        let mut resources = parse_ss_listeners(&output);
        enrich_host_processes(&mut resources).await;
        return Ok(resources);
    }
    let output = run_command("host", "netstat", &["-anv", "-p", "tcp"]).await?;
    let mut resources = parse_netstat_listeners(&output);
    enrich_host_processes(&mut resources).await;
    Ok(resources)
}

#[cfg(target_os = "linux")]
async fn collect_from_ss() -> anyhow::Result<Option<String>> {
    Ok(run_command("host", "ss", &["-ltnp"]).await.ok())
}

pub fn parse_lsof_listeners(output: &str) -> Vec<ResourceRecord> {
    let mut grouped: BTreeMap<String, ResourceRecord> = BTreeMap::new();

    for line in output.lines().skip(1) {
        let columns: Vec<_> = line.split_whitespace().collect();
        if columns.len() < 10 || columns[7] != "TCP" || columns.last() != Some(&"(LISTEN)") {
            continue;
        }
        let command = columns[0].to_string();
        let pid = columns[1].to_string();
        let user = columns[2].to_string();
        let address = columns[columns.len() - 2];
        let Some(port) = capture_port(address) else {
            continue;
        };
        let entry = grouped
            .entry(pid.clone())
            .or_insert_with(|| ResourceRecord {
                id: format!("host:{pid}"),
                kind: ResourceKind::HostProcess,
                runtime: RuntimeKind::Host,
                project: None,
                name: command.clone(),
                state: HealthState::Healthy,
                runtime_status: Some("listening".into()),
                ports: Vec::new(),
                labels: BTreeMap::new(),
                urls: Vec::new(),
                metadata: BTreeMap::from([
                    ("pid".into(), pid.clone()),
                    ("user".into(), user.clone()),
                ]),
                last_changed: chrono::Utc::now(),
            });
        entry.ports.push(PortBinding {
            host_ip: None,
            host_port: port,
            container_port: None,
            protocol: "tcp".into(),
        });
        if let Some(url) = guess_local_url(port) {
            entry.urls.push(url);
        }
    }

    grouped.into_values().collect()
}

pub fn parse_ss_listeners(output: &str) -> Vec<ResourceRecord> {
    let mut grouped: BTreeMap<String, ResourceRecord> = BTreeMap::new();
    for line in output.lines() {
        let Some(captures) = SS_LISTEN_REGEX.captures(line) else {
            continue;
        };
        let pid = captures["pid"].to_string();
        let command = captures["command"].to_string();
        let port = capture_port(&captures["local"]).expect("ss regex captures a numeric port");
        let entry = grouped
            .entry(pid.clone())
            .or_insert_with(|| ResourceRecord {
                id: format!("host:{pid}"),
                kind: ResourceKind::HostProcess,
                runtime: RuntimeKind::Host,
                project: None,
                name: command,
                state: HealthState::Healthy,
                runtime_status: Some("listening".into()),
                ports: Vec::new(),
                labels: BTreeMap::new(),
                urls: Vec::new(),
                metadata: BTreeMap::from([("pid".into(), pid.clone())]),
                last_changed: chrono::Utc::now(),
            });
        entry.ports.push(PortBinding {
            host_ip: None,
            host_port: port,
            container_port: None,
            protocol: "tcp".into(),
        });
        if let Some(url) = guess_local_url(port) {
            entry.urls.push(url);
        }
    }
    grouped.into_values().collect()
}

pub fn parse_netstat_listeners(output: &str) -> Vec<ResourceRecord> {
    output
        .lines()
        .enumerate()
        .filter_map(|(idx, line)| {
            let captures = NETSTAT_LISTEN_REGEX.captures(line)?;
            let port = captures["port"].parse().ok()?;
            let urls = guess_local_url(port).into_iter().collect();
            Some(ResourceRecord {
                id: format!("host:port:{port}:{idx}"),
                kind: ResourceKind::HostProcess,
                runtime: RuntimeKind::Host,
                project: None,
                name: format!("port-{port}"),
                state: HealthState::Healthy,
                runtime_status: Some("listening".into()),
                ports: vec![PortBinding {
                    host_ip: None,
                    host_port: port,
                    container_port: None,
                    protocol: "tcp".into(),
                }],
                labels: BTreeMap::new(),
                urls,
                metadata: BTreeMap::new(),
                last_changed: chrono::Utc::now(),
            })
        })
        .collect()
}

async fn enrich_host_processes(resources: &mut [ResourceRecord]) {
    let pids = resources
        .iter()
        .filter_map(|resource| resource.metadata.get("pid").cloned())
        .collect::<Vec<_>>();
    if pids.is_empty() {
        return;
    }

    let Ok(commands) = process_commands(&pids).await else {
        return;
    };

    for resource in resources {
        let Some(pid) = resource.metadata.get("pid") else {
            continue;
        };
        let Some(command) = commands.get(pid) else {
            continue;
        };
        resource.metadata.insert("command".into(), command.clone());
        if let Some(display) = display_host_command(command) {
            resource.name = display;
        }
    }
}

async fn process_commands(pids: &[String]) -> anyhow::Result<BTreeMap<String, String>> {
    let selection = pids.join(",");
    let output = run_command(
        "host",
        "ps",
        &["-ww", "-o", "pid=", "-o", "command=", "-p", &selection],
    )
    .await?;
    Ok(parse_ps_commands(&output))
}

pub fn parse_ps_commands(output: &str) -> BTreeMap<String, String> {
    output
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim_start();
            let split_at = trimmed.find(char::is_whitespace)?;
            let pid = trimmed[..split_at].trim();
            let command = trimmed[split_at..].trim();
            if pid.is_empty() || command.is_empty() {
                return None;
            }
            Some((pid.to_string(), command.to_string()))
        })
        .collect()
}

pub fn display_host_command(command: &str) -> Option<String> {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return None;
    }

    let mut parts = trimmed.split_whitespace();
    let program = parts.next()?;
    let executable = Path::new(program)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(program);
    let args = parts.collect::<Vec<_>>().join(" ");
    Some(if args.is_empty() {
        executable.to_string()
    } else {
        format!("{executable} {args}")
    })
}

fn capture_port(address: &str) -> Option<u16> {
    address.rsplit(':').next()?.parse().ok()
}

#[cfg(test)]
#[allow(clippy::await_holding_lock)]
mod tests {
    use super::{
        collect, collect_host_processes, display_host_command, enrich_host_processes,
        parse_lsof_listeners, parse_netstat_listeners, parse_ps_commands, parse_ss_listeners,
    };
    use crate::command::{command_overrides, run_command, test_lock, write_script};
    use giggity_core::config::Config;
    use tempfile::tempdir;

    fn reset_overrides() {
        command_overrides().lock().expect("lock").clear();
    }

    #[test]
    fn parses_lsof_listener_rows() {
        let parsed = parse_lsof_listeners(
            "COMMAND PID USER FD TYPE DEVICE SIZE/OFF NODE NAME\npython 123 q 4u IPv4 0x 0t0 TCP *:8080 (LISTEN)\n",
        );
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].name, "python");
        assert_eq!(parsed[0].ports[0].host_port, 8080);
    }

    #[test]
    fn lsof_and_ss_parsers_ignore_invalid_rows_and_unknown_ports() {
        let lsof = parse_lsof_listeners(
            "COMMAND PID USER FD TYPE DEVICE SIZE/OFF NODE NAME\nbad row\npython 123 q 4u IPv4 0x 0t0 TCP *:2222 (LISTEN)\npython 124 q 4u IPv4 0x 0t0 TCP *:oops (LISTEN)\n",
        );
        assert_eq!(lsof.len(), 1);
        assert!(lsof[0].urls.is_empty());

        let ss = parse_ss_listeners(
            "garbage\nLISTEN 0 4096 127.0.0.1:1234 0.0.0.0:* users:((\"worker\",pid=4,fd=5))",
        );
        assert_eq!(ss.len(), 1);
        assert!(ss[0].urls.is_empty());
    }

    #[test]
    fn lsof_parser_ignores_non_listening_and_non_tcp_rows() {
        let parsed = parse_lsof_listeners(
            "COMMAND PID USER FD TYPE DEVICE SIZE/OFF NODE NAME\npython 123 q 4u IPv4 0x 0t0 UDP *:8080\npython 124 q 4u IPv4 0x 0t0 TCP *:8080 (ESTABLISHED)\npython 125 q 4u IPv4 0x 0t0 TCP *:8080 (LISTEN)\n",
        );
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].metadata["pid"], "125");
    }

    #[test]
    fn parses_ss_listener_rows() {
        let parsed = parse_ss_listeners(
            r#"LISTEN 0 4096 127.0.0.1:5432 0.0.0.0:* users:(("postgres",pid=777,fd=5))"#,
        );
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].metadata["pid"], "777");
    }

    #[test]
    fn ss_parser_adds_urls_for_known_ports() {
        let parsed = parse_ss_listeners(
            r#"LISTEN 0 4096 127.0.0.1:8080 0.0.0.0:* users:(("python",pid=123,fd=5))"#,
        );
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].urls[0].as_str(), "http://127.0.0.1:8080/");
    }

    #[test]
    fn parses_netstat_listener_rows() {
        let parsed = parse_netstat_listeners("tcp4 0 0 *.3000 *.* LISTEN\n");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].ports[0].host_port, 3000);
    }

    #[test]
    fn parsing_helpers_cover_ignored_lines_and_https_urls() {
        assert!(parse_lsof_listeners("COMMAND PID USER\njunk\n").is_empty());
        assert!(parse_ss_listeners("garbage").is_empty());
        let parsed = parse_netstat_listeners("tcp4 0 0 *.8443 *.* LISTEN\n");
        assert_eq!(parsed[0].urls[0].as_str(), "https://127.0.0.1:8443/");
    }

    #[test]
    fn parses_ps_output_and_normalizes_host_command_names() {
        let parsed = parse_ps_commands(
            "  123 /nix/store/node/bin/node server.js --port 3000\n  777 python3 -m http.server\n",
        );
        assert_eq!(
            parsed["123"],
            "/nix/store/node/bin/node server.js --port 3000"
        );
        assert_eq!(
            display_host_command(parsed["123"].as_str()).as_deref(),
            Some("node server.js --port 3000")
        );
        assert_eq!(
            display_host_command(parsed["777"].as_str()).as_deref(),
            Some("python3 -m http.server")
        );
        assert_eq!(
            display_host_command("/usr/local/bin/postgres").as_deref(),
            Some("postgres")
        );
        assert!(display_host_command("   ").is_none());
        assert!(parse_ps_commands("  123\n  456   \n  onlytext\n").is_empty());
    }

    #[tokio::test]
    async fn enrich_host_processes_handles_failed_and_partial_ps_output() {
        let _guard = test_lock().lock().expect("lock");
        reset_overrides();
        let dir = tempdir().expect("tempdir");
        let ps_fail = write_script(dir.path(), "ps", "echo nope >&2; exit 1");
        command_overrides()
            .lock()
            .expect("lock")
            .insert("ps".into(), ps_fail);

        let mut resources = parse_lsof_listeners(
            "COMMAND PID USER FD TYPE DEVICE SIZE/OFF NODE NAME\npython 123 q 4u IPv4 0x 0t0 TCP *:8080 (LISTEN)\n",
        );
        enrich_host_processes(&mut resources).await;
        assert_eq!(resources[0].name, "python");
        assert!(!resources[0].metadata.contains_key("command"));

        let dir = tempdir().expect("tempdir");
        let ps_partial = write_script(dir.path(), "ps", "printf '  999 /usr/bin/ignored\\n'");
        command_overrides()
            .lock()
            .expect("lock")
            .insert("ps".into(), ps_partial);
        let mut resources = parse_lsof_listeners(
            "COMMAND PID USER FD TYPE DEVICE SIZE/OFF NODE NAME\npython 123 q 4u IPv4 0x 0t0 TCP *:8080 (LISTEN)\n",
        );
        resources.push(giggity_core::model::ResourceRecord {
            id: "host:port:3000:1".into(),
            kind: giggity_core::model::ResourceKind::HostProcess,
            runtime: giggity_core::model::RuntimeKind::Host,
            project: None,
            name: "port-3000".into(),
            state: giggity_core::model::HealthState::Healthy,
            runtime_status: Some("listening".into()),
            ports: vec![giggity_core::model::PortBinding {
                host_ip: None,
                host_port: 3000,
                container_port: None,
                protocol: "tcp".into(),
            }],
            labels: std::collections::BTreeMap::new(),
            urls: Vec::new(),
            metadata: std::collections::BTreeMap::new(),
            last_changed: chrono::Utc::now(),
        });
        enrich_host_processes(&mut resources).await;
        assert_eq!(resources[0].name, "python");
        assert_eq!(resources[1].name, "port-3000");
        reset_overrides();
    }

    #[tokio::test]
    async fn run_command_and_collection_use_overrides() {
        let _guard = test_lock().lock().expect("lock");
        reset_overrides();
        let dir = tempdir().expect("tempdir");
        let lsof = write_script(
            dir.path(),
            "lsof",
            "printf 'COMMAND PID USER FD TYPE DEVICE SIZE/OFF NODE NAME\npython 123 q 4u IPv4 0x 0t0 TCP *:8080 (LISTEN)\n'",
        );
        let ps = write_script(
            dir.path(),
            "ps",
            "printf '  123 /usr/bin/python3 manage.py runserver 0.0.0.0:8080\n'",
        );
        command_overrides()
            .lock()
            .expect("lock")
            .insert("lsof".into(), lsof);
        command_overrides()
            .lock()
            .expect("lock")
            .insert("ps".into(), ps);

        let ok = run_command("host", "lsof", &["-nP"]).await.expect("stdout");
        assert!(ok.contains("python"));

        let mut config = Config::default();
        config.sources.host_listeners = true;
        let collected = collect(&config).await;
        assert!(collected.warnings.is_empty());
        assert_eq!(
            collected.resources[0].name,
            "python3 manage.py runserver 0.0.0.0:8080"
        );
        assert_eq!(
            collected.resources[0].metadata["command"],
            "/usr/bin/python3 manage.py runserver 0.0.0.0:8080"
        );
        reset_overrides();
    }

    #[tokio::test]
    async fn host_collection_falls_back_to_netstat_and_surfaces_warning() {
        let _guard = test_lock().lock().expect("lock");
        reset_overrides();
        let dir = tempdir().expect("tempdir");
        let lsof = write_script(dir.path(), "lsof", "echo bad >&2; exit 1");
        let netstat = write_script(
            dir.path(),
            "netstat",
            "printf 'tcp4 0 0 *.3000 *.* LISTEN\n'",
        );
        let ps = write_script(dir.path(), "ps", "echo nope >&2; exit 1");
        {
            let mut overrides = command_overrides().lock().expect("lock");
            overrides.insert("lsof".into(), lsof);
            overrides.insert("netstat".into(), netstat);
            overrides.insert("ps".into(), ps);
        }

        let resources = collect_host_processes().await.expect("fallback");
        assert_eq!(resources[0].ports[0].host_port, 3000);

        let dir = tempdir().expect("tempdir");
        let lsof = write_script(dir.path(), "lsof", "echo bad >&2; exit 1");
        let netstat = write_script(dir.path(), "netstat", "echo worse >&2; exit 2");
        {
            let mut overrides = command_overrides().lock().expect("lock");
            overrides.insert("lsof".into(), lsof);
            overrides.insert("netstat".into(), netstat);
        }

        let mut config = Config::default();
        config.sources.host_listeners = true;
        let collected = collect(&config).await;
        assert!(collected.resources.is_empty());
        assert_eq!(collected.warnings[0].source, "host");
        reset_overrides();
    }
}
