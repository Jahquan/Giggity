mod install;
mod popup;

use std::collections::BTreeMap;
use std::future::Future;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;

use clap::{Args, Parser, Subcommand};
use giggity_core::config::{Config, TmuxOverrides};
use giggity_core::model::Snapshot;
use giggity_core::protocol::{ActionKind, ClientRequest, RenderFormat, ServerResponse};
use giggity_core::view::{render_status_line, render_tmux_status_line, resolve_view};
use giggity_daemon::{DaemonClient, ensure_daemon_running, run_daemon};
use tokio::process::Command;
use tracing_subscriber::EnvFilter;

#[cfg(test)]
mod test_support;

#[derive(Debug, Parser)]
#[command(
    name = "giggity",
    version,
    about = "Developer service dashboard for tmux"
)]
struct Cli {
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    Daemon(DaemonArgs),
    Render(RenderArgs),
    Popup(RenderArgs),
    Query(QueryArgs),
    Action(ActionArgs),
    Config(ConfigCommand),
    InstallService(InstallServiceArgs),
}

#[derive(Debug, Args)]
struct DaemonArgs {
    #[arg(long)]
    background: bool,
}

#[derive(Debug, Args, Clone)]
struct RenderArgs {
    #[arg(long)]
    view: Option<String>,
    #[arg(long, default_value = "tmux")]
    format: String,
    #[arg(long = "tmux-option")]
    tmux_option: Vec<String>,
}

#[derive(Debug, Args)]
struct QueryArgs {
    #[arg(long)]
    json: bool,
    #[arg(long)]
    view: Option<String>,
}

#[derive(Debug, Args)]
struct ActionArgs {
    action: String,
    #[arg(long)]
    resource: String,
    #[arg(long)]
    confirm: bool,
}

#[derive(Debug, Subcommand)]
enum ConfigSubcommand {
    Validate,
}

#[derive(Debug, Args)]
struct ConfigCommand {
    #[command(subcommand)]
    command: ConfigSubcommand,
}

#[derive(Debug, Args)]
struct InstallServiceArgs {
    #[arg(long)]
    activate: bool,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    init_tracing();
    run_cli(Cli::parse()).await
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(false)
        .compact()
        .init();
}

async fn run_cli(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Commands::Daemon(args) => daemon_command(cli.config, args).await,
        Commands::Render(args) => render_command(cli.config, args).await,
        Commands::Popup(args) => popup_command(cli.config, args).await,
        Commands::Query(args) => query_command(cli.config, args).await,
        Commands::Action(args) => action_command(cli.config, args).await,
        Commands::Config(command) => config_command(cli.config, command).await,
        Commands::InstallService(args) => install_service_command(cli.config, args).await,
    }
}

async fn daemon_command(config_path: Option<PathBuf>, args: DaemonArgs) -> anyhow::Result<()> {
    if args.background {
        let current_exe = std::env::current_exe()?;
        let mut command = Command::new(current_exe);
        command.arg("daemon");
        if let Some(path) = config_path {
            command.arg("--config").arg(path);
        }
        command.stdin(Stdio::null());
        command.stdout(Stdio::null());
        command.stderr(Stdio::null());
        command.spawn()?;
        return Ok(());
    }
    run_daemon(config_path).await
}

async fn render_command(config_path: Option<PathBuf>, args: RenderArgs) -> anyhow::Result<()> {
    let overrides = parse_tmux_overrides(&args.tmux_option);
    let config = load_config(config_path.clone(), &overrides)?;
    let snapshot = request_snapshot(&config, config_path.as_deref(), args.view.clone()).await?;
    println!(
        "{}",
        render_snapshot(
            &config,
            args.view.as_deref(),
            parse_render_format(&args.format)?,
            &snapshot
        )
    );
    Ok(())
}

async fn popup_command(config_path: Option<PathBuf>, args: RenderArgs) -> anyhow::Result<()> {
    let overrides = parse_tmux_overrides(&args.tmux_option);
    let config = load_config(config_path.clone(), &overrides)?;
    ensure_daemon_running(&config.socket_path, config_path.as_deref()).await?;
    let client = DaemonClient::new(config.socket_path.clone());
    launch_popup(client, config, args.view).await
}

async fn launch_popup(
    client: DaemonClient,
    config: Config,
    view: Option<String>,
) -> anyhow::Result<()> {
    launch_popup_with(client, config, view, popup::run_popup).await
}

async fn launch_popup_with<F, Fut>(
    client: DaemonClient,
    config: Config,
    view: Option<String>,
    runner: F,
) -> anyhow::Result<()>
where
    F: FnOnce(DaemonClient, Config, Option<String>) -> Fut,
    Fut: Future<Output = anyhow::Result<()>>,
{
    runner(client, config, view).await
}

async fn query_command(config_path: Option<PathBuf>, args: QueryArgs) -> anyhow::Result<()> {
    let config = load_config(config_path.clone(), &TmuxOverrides::new())?;
    let snapshot = request_snapshot(&config, config_path.as_deref(), args.view.clone()).await?;
    print!(
        "{}",
        format_query_output(&config, args.view.as_deref(), &snapshot, args.json)?
    );
    Ok(())
}

async fn action_command(config_path: Option<PathBuf>, args: ActionArgs) -> anyhow::Result<()> {
    let config = load_config(config_path.clone(), &TmuxOverrides::new())?;
    println!(
        "{}",
        execute_action(
            &config,
            config_path.as_deref(),
            parse_action(&args.action)?,
            args.resource,
            args.confirm
        )
        .await?
    );
    Ok(())
}

async fn config_command(
    config_path: Option<PathBuf>,
    command: ConfigCommand,
) -> anyhow::Result<()> {
    let config_path = config_path.unwrap_or_else(Config::default_path);
    let config = Config::load_from(&config_path)?;
    match command.command {
        ConfigSubcommand::Validate => print!("{}", format_config_validation(&config)),
    }
    Ok(())
}

async fn install_service_command(
    config_path: Option<PathBuf>,
    args: InstallServiceArgs,
) -> anyhow::Result<()> {
    let config_path = config_path.unwrap_or_else(Config::default_path);
    let path = install::install_service(&config_path, args.activate).await?;
    println!("{}", path.display());
    Ok(())
}

fn load_config(config_path: Option<PathBuf>, overrides: &TmuxOverrides) -> anyhow::Result<Config> {
    let path = config_path.unwrap_or_else(Config::default_path);
    Config::load_with_tmux_overrides(path, overrides).map_err(anyhow::Error::from)
}

async fn request_snapshot(
    config: &Config,
    config_path: Option<&Path>,
    view: Option<String>,
) -> anyhow::Result<Snapshot> {
    ensure_daemon_running(&config.socket_path, config_path).await?;
    let client = DaemonClient::new(config.socket_path.clone());
    match client.request(&ClientRequest::Query { view }).await? {
        ServerResponse::Query { snapshot } => Ok(snapshot),
        ServerResponse::Error { message } => anyhow::bail!("{message}"),
        _ => anyhow::bail!("unexpected response"),
    }
}

fn render_snapshot(
    config: &Config,
    view: Option<&str>,
    format: RenderFormat,
    snapshot: &Snapshot,
) -> String {
    let resolved = resolve_view(config, view, snapshot);
    match format {
        RenderFormat::Plain => render_status_line(&resolved),
        RenderFormat::Tmux => render_tmux_status_line(&resolved),
    }
}

fn format_query_output(
    config: &Config,
    view: Option<&str>,
    snapshot: &Snapshot,
    json: bool,
) -> anyhow::Result<String> {
    if json {
        return Ok(format!("{}\n", serde_json::to_string_pretty(snapshot)?));
    }

    let resolved = resolve_view(config, view, snapshot);
    let mut output = String::new();
    for resource in &resolved.resources {
        output.push_str(&format!(
            "{:<24} {:<10} {:<10} {:<10} {}\n",
            resource.name,
            resource.state,
            resource.runtime,
            resource.project.clone().unwrap_or_else(|| "-".into()),
            resource
                .ports
                .iter()
                .map(|port| port.host_port.to_string())
                .collect::<Vec<_>>()
                .join(",")
        ));
    }
    Ok(output)
}

async fn execute_action(
    config: &Config,
    config_path: Option<&Path>,
    action: ActionKind,
    resource_id: String,
    confirm: bool,
) -> anyhow::Result<String> {
    ensure_daemon_running(&config.socket_path, config_path).await?;
    let client = DaemonClient::new(config.socket_path.clone());
    match client
        .request(&ClientRequest::Action {
            action,
            resource_id,
            confirm,
        })
        .await?
    {
        ServerResponse::ActionResult { message } | ServerResponse::Logs { content: message } => {
            Ok(message)
        }
        ServerResponse::Error { message } => anyhow::bail!("{message}"),
        _ => anyhow::bail!("unexpected response"),
    }
}

fn format_config_validation(config: &Config) -> String {
    let warnings = config.validate();
    if warnings.is_empty() {
        "config is valid\n".into()
    } else {
        format!("{}\n", warnings.join("\n"))
    }
}

fn parse_tmux_overrides(values: &[String]) -> TmuxOverrides {
    values
        .iter()
        .filter_map(|pair| pair.split_once('='))
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect::<BTreeMap<_, _>>()
}

fn parse_render_format(value: &str) -> anyhow::Result<RenderFormat> {
    match value {
        "tmux" => Ok(RenderFormat::Tmux),
        "plain" => Ok(RenderFormat::Plain),
        _ => anyhow::bail!("unsupported render format: {value}"),
    }
}

fn parse_action(value: &str) -> anyhow::Result<ActionKind> {
    match value {
        "restart" => Ok(ActionKind::Restart),
        "stop" => Ok(ActionKind::Stop),
        "logs" => Ok(ActionKind::Logs),
        "open-url" => Ok(ActionKind::OpenUrl),
        "copy-port" => Ok(ActionKind::CopyPort),
        _ => anyhow::bail!("unsupported action: {value}"),
    }
}

#[cfg(test)]
#[allow(clippy::await_holding_lock)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::Duration;

    use super::{parse_action, parse_tmux_overrides};
    use async_trait::async_trait;
    use chrono::{TimeZone, Utc};
    use giggity_collectors::{CollectionOutput, CollectorProvider};
    use giggity_core::config::{Config, MatchRule, ProbeKind, ProbeSpec};
    use giggity_core::model::{
        HealthState, PortBinding, ResourceKind, ResourceRecord, RuntimeKind, Snapshot,
    };
    use giggity_core::protocol::{ActionKind, RenderFormat, ServerResponse};
    use giggity_daemon::run_daemon_with_collector;
    use tempfile::tempdir;
    use tokio::sync::oneshot;

    use super::{
        ActionArgs, Cli, Commands, ConfigCommand, ConfigSubcommand, DaemonArgs, InstallServiceArgs,
        QueryArgs, RenderArgs, action_command, config_command, daemon_command, execute_action,
        format_config_validation, format_query_output, install_service_command, launch_popup_with,
        load_config, parse_render_format, query_command, render_command, render_snapshot,
        request_snapshot, run_cli,
    };
    use crate::test_support::EnvVarGuard;

    #[derive(Debug)]
    struct FakeCollector;

    static TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    #[async_trait]
    impl CollectorProvider for FakeCollector {
        async fn collect(&self, _config: &Config) -> anyhow::Result<CollectionOutput> {
            Ok(CollectionOutput {
                resources: vec![resource()],
                warnings: Vec::new(),
            })
        }
    }

    fn resource() -> ResourceRecord {
        ResourceRecord {
            id: "host:api".into(),
            kind: ResourceKind::HostProcess,
            runtime: RuntimeKind::Host,
            project: Some("dev".into()),
            name: "api".into(),
            state: HealthState::Healthy,
            runtime_status: Some("listening".into()),
            ports: vec![PortBinding {
                host_ip: None,
                host_port: 3000,
                container_port: None,
                protocol: "tcp".into(),
            }],
            labels: BTreeMap::new(),
            urls: vec!["http://127.0.0.1:3000".parse().expect("url")],
            metadata: BTreeMap::from([("pid".into(), "123".into())]),
            last_changed: Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap(),
        }
    }

    async fn spawn_fake_daemon() -> (
        Config,
        std::path::PathBuf,
        oneshot::Sender<()>,
        tempfile::TempDir,
    ) {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "cache_dir = '{}'\nsocket_path = '{}'\n",
                dir.path().display(),
                dir.path().join("giggity.sock").display()
            ),
        )
        .expect("config");
        let config = Config::load_from(&config_path).expect("config");
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        tokio::spawn(run_daemon_with_collector(
            Some(config_path.clone()),
            Arc::new(FakeCollector),
            Some(shutdown_rx),
        ));
        tokio::time::sleep(Duration::from_millis(250)).await;
        (config, config_path, shutdown_tx, dir)
    }

    #[test]
    fn parses_tmux_overrides() {
        let overrides = parse_tmux_overrides(&["view=ops".into(), "template=abc".into()]);
        assert_eq!(
            overrides,
            BTreeMap::from([
                ("view".to_string(), "ops".to_string()),
                ("template".to_string(), "abc".to_string())
            ])
        );
    }

    #[test]
    fn parses_actions() {
        assert!(matches!(
            parse_action("restart").unwrap(),
            ActionKind::Restart
        ));
    }

    #[test]
    fn rejects_unknown_action_and_format() {
        assert!(parse_action("bogus").is_err());
        assert!(parse_render_format("bogus").is_err());
    }

    #[test]
    fn load_config_applies_tmux_overrides() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "refresh_seconds = 5\n").expect("config");
        let config = load_config(
            Some(path),
            &BTreeMap::from([
                ("refresh_seconds".into(), "9".into()),
                ("template".into(), "ok {total}".into()),
            ]),
        )
        .expect("config");
        assert_eq!(config.refresh_seconds, 9);
        assert_eq!(config.active_view(None).status_bar.template, "ok {total}");
    }

    #[test]
    fn render_and_query_helpers_format_snapshot() {
        let config = Config::default();
        let snapshot = Snapshot {
            resources: vec![resource()],
            ..Snapshot::default()
        };

        let rendered = render_snapshot(&config, None, RenderFormat::Plain, &snapshot);
        assert!(rendered.contains("ok 1"));
        let tmux_rendered = render_snapshot(&config, None, RenderFormat::Tmux, &snapshot);
        assert!(tmux_rendered.contains("#[fg="));

        let text = format_query_output(&config, None, &snapshot, false).expect("text output");
        assert!(text.contains("api"));
        assert!(text.contains("3000"));

        let json = format_query_output(&config, None, &snapshot, true).expect("json output");
        assert!(json.contains("\"resources\""));
        assert!(json.contains("\"api\""));
    }

    #[test]
    fn config_validation_output_renders_success_and_warnings() {
        assert_eq!(
            format_config_validation(&Config::default()),
            "config is valid\n"
        );

        let mut config = Config {
            refresh_seconds: 0,
            ..Config::default()
        };
        config.probes.push(ProbeSpec {
            name: "broken".into(),
            matcher: MatchRule::default(),
            kind: ProbeKind::Tcp {
                host: None,
                port: None,
            },
            timeout_millis: 0,
        });
        let output = format_config_validation(&config);
        assert!(output.contains("refresh_seconds should be at least 1"));
        assert!(output.contains("timeout_millis should be at least 1"));
    }

    #[tokio::test]
    async fn request_snapshot_and_logs_action_use_running_daemon() {
        let (config, config_path, shutdown_tx, _dir) = spawn_fake_daemon().await;

        let snapshot = request_snapshot(&config, Some(&config_path), None)
            .await
            .expect("snapshot");
        assert_eq!(snapshot.resources.len(), 1);

        let message = execute_action(
            &config,
            Some(&config_path),
            ActionKind::Logs,
            "host:api".into(),
            false,
        )
        .await
        .expect("logs action");
        assert_eq!(message, "logs unavailable for this resource");

        let error = execute_action(
            &config,
            Some(&config_path),
            ActionKind::Restart,
            "host:api".into(),
            false,
        )
        .await
        .expect_err("restart without confirm");
        assert!(error.to_string().contains("require confirmation"));

        let _ = shutdown_tx.send(());
    }

    #[tokio::test]
    async fn command_entrypoints_execute_against_fake_daemon() {
        let (_config, config_path, shutdown_tx, _dir) = spawn_fake_daemon().await;

        render_command(
            Some(config_path.clone()),
            RenderArgs {
                view: None,
                format: "plain".into(),
                tmux_option: Vec::new(),
            },
        )
        .await
        .expect("render command");

        query_command(
            Some(config_path.clone()),
            QueryArgs {
                json: false,
                view: None,
            },
        )
        .await
        .expect("query command");

        action_command(
            Some(config_path.clone()),
            ActionArgs {
                action: "logs".into(),
                resource: "host:api".into(),
                confirm: false,
            },
        )
        .await
        .expect("action command");

        config_command(
            Some(config_path.clone()),
            ConfigCommand {
                command: ConfigSubcommand::Validate,
            },
        )
        .await
        .expect("config command");

        let _ = shutdown_tx.send(());
    }

    #[tokio::test]
    async fn daemon_popup_and_install_commands_are_exercised() {
        let _guard = TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("lock");
        let (_config, config_path, shutdown_tx, dir) = spawn_fake_daemon().await;

        launch_popup_with(
            giggity_daemon::DaemonClient::new(dir.path().join("giggity.sock")),
            Config::load_from(&config_path).expect("config"),
            Some("default".into()),
            |_client, _config, _view| async { Ok(()) },
        )
        .await
        .expect("launch popup");

        let daemon_config = dir.path().join("daemon-config.toml");
        std::fs::write(
            &daemon_config,
            format!(
                "cache_dir = '{}'\nsocket_path = '{}'\nrefresh_seconds = 1\n[sources]\ndocker = false\npodman = false\nnerdctl = false\nhost_listeners = false\nlaunchd = false\nsystemd = false\n",
                dir.path().display(),
                dir.path().join("daemon-test.sock").display()
            ),
        )
        .expect("daemon config");

        daemon_command(Some(daemon_config.clone()), DaemonArgs { background: true })
            .await
            .expect("background daemon");

        let task = tokio::spawn(daemon_command(
            Some(daemon_config.clone()),
            DaemonArgs { background: false },
        ));
        for _ in 0..10 {
            if dir.path().join("daemon-test.sock").exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(dir.path().join("daemon-test.sock").exists());
        task.abort();

        let _env = EnvVarGuard::set("HOME", dir.path().as_os_str().to_os_string());
        install_service_command(
            Some(config_path.clone()),
            InstallServiceArgs { activate: false },
        )
        .await
        .expect("install service");

        let _ = shutdown_tx.send(());
    }

    #[tokio::test]
    async fn request_snapshot_and_execute_action_reject_unexpected_responses() {
        let dir = tempdir().expect("tempdir");
        let socket_path = dir.path().join("giggity.sock");
        let config = Config {
            socket_path: socket_path.clone(),
            ..Config::default()
        };
        let listener = tokio::net::UnixListener::bind(&socket_path).expect("bind");
        let server = tokio::spawn(async move {
            for response in [
                serde_json::to_string(&giggity_core::protocol::ServerResponse::Pong {
                    api_version: 1,
                })
                .expect("pong"),
                serde_json::to_string(&giggity_core::protocol::ServerResponse::Rendered {
                    output: "nope".into(),
                })
                .expect("rendered"),
                serde_json::to_string(&giggity_core::protocol::ServerResponse::Pong {
                    api_version: 1,
                })
                .expect("pong"),
                serde_json::to_string(&giggity_core::protocol::ServerResponse::Validation {
                    warnings: Vec::new(),
                })
                .expect("validation"),
            ] {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut reader = tokio::io::BufReader::new(&mut stream);
                let mut line = String::new();
                use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
                reader.read_line(&mut line).await.expect("read");
                stream.write_all(response.as_bytes()).await.expect("write");
                stream.write_all(b"\n").await.expect("newline");
            }
        });

        let snapshot_error = request_snapshot(&config, None, None)
            .await
            .expect_err("unexpected query");
        assert!(snapshot_error.to_string().contains("unexpected response"));

        let action_error =
            execute_action(&config, None, ActionKind::Logs, "host:api".into(), false)
                .await
                .expect_err("unexpected action");
        assert!(action_error.to_string().contains("unexpected response"));

        server.await.expect("server");
    }

    #[tokio::test]
    async fn request_snapshot_surfaces_server_errors() {
        let dir = tempdir().expect("tempdir");
        let socket_path = dir.path().join("giggity.sock");
        let config = Config {
            socket_path: socket_path.clone(),
            ..Config::default()
        };
        let listener = tokio::net::UnixListener::bind(&socket_path).expect("bind");
        let server = tokio::spawn(async move {
            for response in [
                ServerResponse::Pong { api_version: 1 },
                ServerResponse::Error {
                    message: "snapshot failed".into(),
                },
            ] {
                let (stream, _) = listener.accept().await.expect("accept");
                let (reader, mut writer) = stream.into_split();
                let mut reader = tokio::io::BufReader::new(reader);
                let mut line = String::new();
                use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
                reader.read_line(&mut line).await.expect("read");
                writer
                    .write_all(
                        serde_json::to_string(&response)
                            .expect("response")
                            .as_bytes(),
                    )
                    .await
                    .expect("write");
                writer.write_all(b"\n").await.expect("newline");
            }
        });

        let error = request_snapshot(&config, None, None)
            .await
            .expect_err("server error");
        assert!(error.to_string().contains("snapshot failed"));
        server.await.expect("server");
    }

    #[tokio::test]
    async fn run_cli_dispatches_commands() {
        let _guard = TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("lock");
        let (_config, config_path, shutdown_tx, dir) = spawn_fake_daemon().await;

        run_cli(Cli {
            config: Some(config_path.clone()),
            command: Commands::Render(RenderArgs {
                view: None,
                format: "plain".into(),
                tmux_option: Vec::new(),
            }),
        })
        .await
        .expect("render");

        run_cli(Cli {
            config: Some(config_path.clone()),
            command: Commands::Query(QueryArgs {
                json: false,
                view: None,
            }),
        })
        .await
        .expect("query");

        run_cli(Cli {
            config: Some(config_path.clone()),
            command: Commands::Action(ActionArgs {
                action: "logs".into(),
                resource: "host:api".into(),
                confirm: false,
            }),
        })
        .await
        .expect("action");

        run_cli(Cli {
            config: Some(config_path.clone()),
            command: Commands::Config(ConfigCommand {
                command: ConfigSubcommand::Validate,
            }),
        })
        .await
        .expect("config");

        run_cli(Cli {
            config: Some(config_path.clone()),
            command: Commands::Daemon(DaemonArgs { background: true }),
        })
        .await
        .expect("daemon");

        run_cli(Cli {
            config: Some(config_path.clone()),
            command: Commands::Popup(RenderArgs {
                view: None,
                format: "plain".into(),
                tmux_option: Vec::new(),
            }),
        })
        .await
        .expect_err("real popup requires terminal");

        let _env = EnvVarGuard::set("HOME", dir.path().as_os_str().to_os_string());
        run_cli(Cli {
            config: Some(config_path.clone()),
            command: Commands::InstallService(InstallServiceArgs { activate: false }),
        })
        .await
        .expect("install");

        let _ = shutdown_tx.send(());
    }
}
