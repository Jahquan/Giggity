use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use chrono::{DateTime, Utc};
use giggity_collectors::{CollectionOutput, CollectorProvider, SystemCollector};
use giggity_core::config::{Config, ProbeKind, ProbeSpec};
use giggity_core::model::{
    CollectorWarning, HealthState, RecentEvent, ResourceKind, ResourceRecord, Snapshot,
};
use giggity_core::protocol::{ActionKind, ClientRequest, RenderFormat, ServerResponse};
use giggity_core::state::StateEngine;
use giggity_core::view::{
    compile_match_rule, matches_compiled_rule, render_status_line, render_tmux_status_line,
    resolve_view,
};
use tokio::fs;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::Command;
use tokio::sync::{RwLock, broadcast, oneshot};
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

#[cfg(test)]
use std::collections::BTreeMap;
#[cfg(test)]
use std::sync::{Mutex, OnceLock};

#[cfg(test)]
static COMMAND_OVERRIDES: OnceLock<Mutex<BTreeMap<String, PathBuf>>> = OnceLock::new();

/// Streaming events broadcast to connected streaming clients.
#[derive(Debug, Clone)]
pub(crate) enum StreamEvent {
    StateChanged(ServerResponse),
    ConfigReloaded,
}

#[derive(Debug, Clone)]
struct Store {
    config: Config,
    snapshot: Snapshot,
    muted_until: Option<DateTime<Utc>>,
    last_notified: HashMap<String, DateTime<Utc>>,
}

pub async fn run_daemon(config_path: Option<PathBuf>) -> anyhow::Result<()> {
    let collector: Arc<dyn CollectorProvider> = Arc::new(SystemCollector);
    run_daemon_with_collector(config_path, collector, None).await
}

pub async fn run_daemon_with_collector(
    config_path: Option<PathBuf>,
    collector: Arc<dyn CollectorProvider>,
    mut shutdown: Option<oneshot::Receiver<()>>,
) -> anyhow::Result<()> {
    let config_path = config_path.unwrap_or_else(Config::default_path);
    let initial_config = Config::load_from(&config_path)?;
    let socket_path = initial_config.socket_path.clone();
    fs::create_dir_all(&initial_config.cache_dir)
        .await
        .with_context(|| format!("creating cache dir {}", initial_config.cache_dir.display()))?;

    if fs::try_exists(&socket_path).await.unwrap_or(false) {
        let _ = fs::remove_file(&socket_path).await;
    }

    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("binding socket {}", socket_path.display()))?;
    let store = Arc::new(RwLock::new(Store {
        config: initial_config.clone(),
        snapshot: Snapshot::default(),
        muted_until: None,
        last_notified: HashMap::new(),
    }));

    let (event_tx, _) = broadcast::channel::<StreamEvent>(256);

    let poll_store = store.clone();
    let poll_collector = collector.clone();
    let poll_path = config_path.clone();
    let poll_event_tx = event_tx.clone();
    let collector_initial_config = initial_config.clone();
    let collector_task = tokio::spawn(async move {
        let mut engine = StateEngine::new(Duration::from_secs(
            collector_initial_config.host_event_ttl_seconds,
        ));
        let mut previous_config = collector_initial_config.clone();
        loop {
            let config = match Config::load_from(&poll_path) {
                Ok(config) => config,
                Err(error) => {
                    warn!(?error, "failed to reload config; keeping previous config");
                    poll_store.read().await.config.clone()
                }
            };
            if config != previous_config {
                log_config_reload(&poll_path, &config);
                previous_config = config.clone();
                let _ = poll_event_tx.send(StreamEvent::ConfigReloaded);
            }
            let output = match poll_collector.collect(&config).await {
                Ok(output) => output,
                Err(error) => CollectionOutput {
                    resources: Vec::new(),
                    warnings: vec![CollectorWarning {
                        source: "collector".into(),
                        message: error.to_string(),
                    }],
                },
            };
            let mut resources = output.resources;
            apply_probes(&config, &mut resources).await;
            let probe_resources = giggity_collectors::probes::collect_probes(&config.probes).await;
            resources.extend(probe_resources);

            let snapshot = engine.ingest(Utc::now(), resources, output.warnings);
            broadcast_new_events(&poll_event_tx, &snapshot);
            let mut guard = poll_store.write().await;
            dispatch_notifications(&mut guard, &snapshot).await;
            guard.config = config;
            guard.snapshot = snapshot;
            let wait_seconds = guard.config.refresh_seconds.max(1);
            drop(guard);
            sleep(Duration::from_secs(wait_seconds)).await;
        }
    });

    info!(socket = %socket_path.display(), "giggity daemon started");
    let mut ctrl_c = Box::pin(tokio::signal::ctrl_c());
    #[cfg(unix)]
    let mut sigterm = Box::pin(async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?
            .recv()
            .await;
        anyhow::Ok(())
    });
    #[cfg(not(unix))]
    let mut sigterm = Box::pin(async {
        std::future::pending::<()>().await;
        anyhow::Ok(())
    });

    loop {
        tokio::select! {
            accept_result = listener.accept() => {
                let (stream, _) = accept_result?;
                let connection_store = store.clone();
                let connection_event_tx = event_tx.clone();
                tokio::spawn(async move {
                    if let Err(error) = handle_connection(stream, connection_store, connection_event_tx).await {
                        error!(?error, "connection handling failed");
                    }
                });
            }
            _ = &mut ctrl_c => {
                info!("received SIGINT; shutting down giggity daemon");
                break;
            }
            sigterm_result = &mut sigterm => {
                sigterm_result?;
                info!("received SIGTERM; shutting down giggity daemon");
                break;
            }
            _ = wait_for_shutdown(&mut shutdown), if shutdown.is_some() => {
                info!("received internal shutdown request; shutting down giggity daemon");
                break;
            }
        }
    }

    collector_task.abort();
    let _ = collector_task.await;
    let _ = fs::remove_file(&socket_path).await;
    info!(socket = %socket_path.display(), "giggity daemon stopped");
    Ok(())
}

#[derive(Debug, Clone)]
pub struct DaemonClient {
    socket_path: PathBuf,
}

impl DaemonClient {
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
        }
    }

    pub async fn request(&self, request: &ClientRequest) -> anyhow::Result<ServerResponse> {
        let mut stream = UnixStream::connect(&self.socket_path)
            .await
            .with_context(|| format!("connecting to {}", self.socket_path.display()))?;
        let payload = serde_json::to_string(request)?;
        stream.write_all(payload.as_bytes()).await?;
        stream.write_all(b"\n").await?;
        stream.shutdown().await?;

        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).await?;
        Ok(serde_json::from_str(line.trim())?)
    }

    /// Open a streaming connection. Returns the underlying stream split into a
    /// reader (for receiving `ServerResponse` lines) and writer (for sending
    /// `CloseStream`). The caller is responsible for reading lines and
    /// deserializing them.
    pub async fn open_stream(&self, request: &ClientRequest) -> anyhow::Result<StreamHandle> {
        let stream = UnixStream::connect(&self.socket_path)
            .await
            .with_context(|| format!("connecting to {}", self.socket_path.display()))?;
        let (reader, mut writer) = stream.into_split();
        let payload = serde_json::to_string(request)?;
        writer.write_all(payload.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        // Do NOT shutdown -- keep the connection open for streaming
        Ok(StreamHandle {
            reader: BufReader::new(reader),
            writer,
        })
    }
}

/// Handle to an open streaming connection.
pub struct StreamHandle {
    pub reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    pub writer: tokio::net::unix::OwnedWriteHalf,
}

impl StreamHandle {
    /// Read the next streamed response. Returns `None` on EOF.
    pub async fn next_response(&mut self) -> anyhow::Result<Option<ServerResponse>> {
        let mut line = String::new();
        let n = self.reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(None);
        }
        Ok(Some(serde_json::from_str(line.trim())?))
    }

    /// Send a close request to gracefully end the stream.
    pub async fn close(mut self) -> anyhow::Result<()> {
        let payload = serde_json::to_string(&ClientRequest::CloseStream)?;
        self.writer.write_all(payload.as_bytes()).await?;
        self.writer.write_all(b"\n").await?;
        Ok(())
    }
}

async fn handle_connection(
    stream: UnixStream,
    store: Arc<RwLock<Store>>,
    event_tx: broadcast::Sender<StreamEvent>,
) -> anyhow::Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    let request: ClientRequest = serde_json::from_str(line.trim())?;

    match request {
        ClientRequest::StreamLogs { resource_id, lines } => {
            handle_stream_logs(reader, writer, store, &resource_id, lines).await
        }
        ClientRequest::StreamEvents { view } => {
            handle_stream_events(reader, writer, event_tx, view).await
        }
        _ => {
            let response = handle_request(request, store).await;
            let payload = serde_json::to_string(&response)?;
            writer.write_all(payload.as_bytes()).await?;
            writer.write_all(b"\n").await?;
            Ok(())
        }
    }
}

fn broadcast_new_events(tx: &broadcast::Sender<StreamEvent>, snapshot: &Snapshot) {
    for event in &snapshot.events {
        let response = ServerResponse::Event {
            event: event.clone(),
        };
        let _ = tx.send(StreamEvent::StateChanged(response));
    }
}

async fn dispatch_notifications(store: &mut Store, snapshot: &Snapshot) {
    let now = Utc::now();
    let is_muted = store.muted_until.map(|until| now < until).unwrap_or(false);

    let cooldown_secs = store.config.integrations.cooldown_secs as i64;

    for event in &snapshot.events {
        let is_crash = event.to == HealthState::Crashed || event.to == HealthState::Degraded;
        let is_recovery = event.from.is_some()
            && matches!(
                event.from,
                Some(HealthState::Crashed) | Some(HealthState::Degraded)
            )
            && matches!(event.to, HealthState::Healthy);

        if !is_crash && !is_recovery {
            continue;
        }

        if let Some(last) = store.last_notified.get(&event.resource_id) {
            let elapsed = now.signed_duration_since(*last).num_seconds();
            if elapsed < cooldown_secs {
                debug!(
                    resource = %event.resource_id,
                    elapsed_secs = elapsed,
                    cooldown_secs,
                    "skipping notification: within cooldown"
                );
                continue;
            }
        }

        store.last_notified.insert(event.resource_id.clone(), now);

        if !is_muted {
            if is_crash && store.config.notifications.enabled && store.config.notifications.on_crash
            {
                send_desktop_notification(&format!("{} crashed", event.resource_name)).await;
            }
            if is_recovery
                && store.config.notifications.enabled
                && store.config.notifications.on_recovery
            {
                send_desktop_notification(&format!("{} recovered", event.resource_name)).await;
            }
        }

        if is_crash {
            send_integration_crash(&store.config, &event.resource_name).await;
        }
        if is_recovery {
            send_integration_recovery(&store.config, &event.resource_name).await;
        }
    }
}

#[cfg(target_os = "macos")]
async fn send_desktop_notification(message: &str) {
    let script = format!(
        "display notification \"{}\" with title \"Giggity\"",
        message.replace('\\', "\\\\").replace('"', "\\\"")
    );
    let _ = Command::new("osascript")
        .args(["-e", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
}

#[cfg(not(target_os = "macos"))]
async fn send_desktop_notification(_message: &str) {
    // Desktop notifications only supported on macOS
}

async fn send_integration_crash(config: &Config, resource_name: &str) {
    if let Some(slack) = &config.integrations.slack {
        if slack.on_crash {
            let payload = serde_json::json!({
                "text": format!("\u{1f6a8} Giggity: {resource_name} crashed")
            });
            send_webhook(&slack.webhook_url, &payload).await;
        }
    }
    if let Some(telegram) = &config.integrations.telegram {
        if telegram.on_crash {
            let text = format!("\u{1f6a8} Giggity: {resource_name} crashed");
            send_telegram(&telegram.bot_token, &telegram.chat_id, &text).await;
        }
    }
}

async fn send_integration_recovery(config: &Config, resource_name: &str) {
    if let Some(slack) = &config.integrations.slack {
        if slack.on_recovery {
            let payload = serde_json::json!({
                "text": format!("\u{2705} Giggity: {resource_name} recovered")
            });
            send_webhook(&slack.webhook_url, &payload).await;
        }
    }
    if let Some(telegram) = &config.integrations.telegram {
        if telegram.on_recovery {
            let text = format!("\u{2705} Giggity: {resource_name} recovered");
            send_telegram(&telegram.bot_token, &telegram.chat_id, &text).await;
        }
    }
}

async fn send_webhook(url: &str, payload: &serde_json::Value) {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            warn!(?error, "failed to build HTTP client for webhook");
            return;
        }
    };
    match client.post(url).json(payload).send().await {
        Ok(response) if !response.status().is_success() => {
            warn!(
                status = %response.status(),
                url,
                "webhook request failed"
            );
        }
        Err(error) => {
            warn!(?error, url, "webhook request error");
        }
        Ok(_) => {
            debug!(url, "webhook sent successfully");
        }
    }
}

async fn send_telegram(bot_token: &str, chat_id: &str, text: &str) {
    let url = format!("https://api.telegram.org/bot{bot_token}/sendMessage");
    let payload = serde_json::json!({
        "chat_id": chat_id,
        "text": text,
    });
    send_webhook(&url, &payload).await;
}

async fn handle_request(request: ClientRequest, store: Arc<RwLock<Store>>) -> ServerResponse {
    match request {
        ClientRequest::Ping => ServerResponse::Pong { api_version: 1 },
        ClientRequest::Query { .. } => {
            let guard = store.read().await;
            ServerResponse::Query {
                snapshot: guard.snapshot.clone(),
            }
        }
        ClientRequest::Render { view, format } => {
            let guard = store.read().await;
            let resolved = resolve_view(&guard.config, view.as_deref(), &guard.snapshot);
            let output = match format {
                RenderFormat::Plain => render_status_line(&resolved),
                RenderFormat::Tmux => render_tmux_status_line(&resolved),
            };
            ServerResponse::Rendered { output }
        }
        ClientRequest::ValidateConfig => {
            let guard = store.read().await;
            ServerResponse::Validation {
                warnings: guard.config.validate(),
            }
        }
        ClientRequest::ExportConfig => {
            let guard = store.read().await;
            match toml::to_string_pretty(&guard.config) {
                Ok(toml) => ServerResponse::ExportedConfig { toml },
                Err(error) => ServerResponse::Error {
                    message: format!("failed to serialize config: {error}"),
                },
            }
        }
        ClientRequest::MuteNotifications { duration_secs } => {
            let mut guard = store.write().await;
            let until = Utc::now() + chrono::Duration::seconds(duration_secs as i64);
            guard.muted_until = Some(until);
            ServerResponse::MuteResult {
                message: format!("notifications muted for {duration_secs}s"),
            }
        }
        ClientRequest::UnmuteNotifications => {
            let mut guard = store.write().await;
            guard.muted_until = None;
            ServerResponse::MuteResult {
                message: "notifications unmuted".into(),
            }
        }
        ClientRequest::Logs { resource_id, lines } => {
            let guard = store.read().await;
            match guard
                .snapshot
                .resources
                .iter()
                .find(|resource| resource.id == resource_id)
            {
                Some(resource) => match fetch_logs(resource, lines).await {
                    Ok(content) => ServerResponse::Logs { content },
                    Err(error) => ServerResponse::Error {
                        message: error.to_string(),
                    },
                },
                None => ServerResponse::Error {
                    message: format!("unknown resource {resource_id}"),
                },
            }
        }
        ClientRequest::Action {
            action,
            resource_id,
            confirm,
        } => {
            let guard = store.read().await;
            match guard
                .snapshot
                .resources
                .iter()
                .find(|resource| resource.id == resource_id)
            {
                Some(resource) => {
                    if matches!(
                        action,
                        ActionKind::Restart | ActionKind::Stop | ActionKind::ForceKill
                    ) && !confirm
                    {
                        return ServerResponse::Error {
                            message: "mutating actions require confirmation".into(),
                        };
                    }
                    match run_action(&action, resource).await {
                        Ok(message) => ServerResponse::ActionResult { message },
                        Err(error) => ServerResponse::Error {
                            message: error.to_string(),
                        },
                    }
                }
                None => ServerResponse::Error {
                    message: format!("unknown resource {resource_id}"),
                },
            }
        }
        ClientRequest::BulkRestart { resource_ids } => {
            let guard = store.read().await;
            let mut successes = 0_usize;
            let mut failures = Vec::new();
            for rid in &resource_ids {
                match guard.snapshot.resources.iter().find(|r| r.id == *rid) {
                    Some(resource) => match restart_resource(resource).await {
                        Ok(_) => successes += 1,
                        Err(e) => failures.push(format!("{rid}: {e}")),
                    },
                    None => failures.push(format!("{rid}: unknown resource")),
                }
            }
            let mut parts = vec![format!("{successes}/{} restarted", resource_ids.len())];
            if !failures.is_empty() {
                parts.push(format!("failures: {}", failures.join("; ")));
            }
            ServerResponse::ActionResult {
                message: parts.join(", "),
            }
        }
        ClientRequest::StreamLogs { .. }
        | ClientRequest::StreamEvents { .. }
        | ClientRequest::CloseStream => ServerResponse::Error {
            message: "streaming requests are handled at the connection level".into(),
        },
    }
}

async fn send_response(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    response: &ServerResponse,
) -> anyhow::Result<()> {
    let payload = serde_json::to_string(response)?;
    writer.write_all(payload.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

async fn handle_stream_logs(
    mut reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    mut writer: tokio::net::unix::OwnedWriteHalf,
    store: Arc<RwLock<Store>>,
    resource_id: &str,
    lines: u32,
) -> anyhow::Result<()> {
    let (resource, runtime) = {
        let guard = store.read().await;
        match guard
            .snapshot
            .resources
            .iter()
            .find(|r| r.id == resource_id)
        {
            Some(r) => (r.clone(), r.runtime),
            None => {
                send_response(
                    &mut writer,
                    &ServerResponse::Error {
                        message: format!("unknown resource {resource_id}"),
                    },
                )
                .await?;
                return Ok(());
            }
        }
    };

    let args = match build_log_follow_args(&resource, lines) {
        Ok(args) => args,
        Err(error) => {
            send_response(
                &mut writer,
                &ServerResponse::Error {
                    message: error.to_string(),
                },
            )
            .await?;
            return Ok(());
        }
    };

    let program = runtime_log_program(runtime);
    let mut child = match Command::new(resolve_program(program))
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            send_response(
                &mut writer,
                &ServerResponse::Error {
                    message: format!("failed to spawn {program}: {error}"),
                },
            )
            .await?;
            return Ok(());
        }
    };

    let stdout = child.stdout.take().expect("stdout was piped");
    let mut log_reader = BufReader::new(stdout);
    let mut close_line = String::new();

    loop {
        let mut log_line = String::new();
        tokio::select! {
            result = log_reader.read_line(&mut log_line) => {
                match result {
                    Ok(0) => break,
                    Ok(_) => {
                        let trimmed = log_line.trim_end().to_string();
                        if send_response(&mut writer, &ServerResponse::LogLine { line: trimmed }).await.is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            result = reader.read_line(&mut close_line) => {
                match result {
                    Ok(0) => break,
                    Ok(_) => {
                        if let Ok(ClientRequest::CloseStream) = serde_json::from_str(close_line.trim()) {
                            break;
                        }
                        close_line.clear();
                    }
                    Err(_) => break,
                }
            }
        }
    }

    child.kill().await.ok();
    child.wait().await.ok();
    let _ = send_response(
        &mut writer,
        &ServerResponse::StreamEnd {
            reason: "log stream ended".into(),
        },
    )
    .await;
    debug!(resource_id, "log stream closed");
    Ok(())
}

fn runtime_log_program(runtime: giggity_core::model::RuntimeKind) -> &'static str {
    match runtime {
        giggity_core::model::RuntimeKind::Docker => "docker",
        giggity_core::model::RuntimeKind::Podman => "podman",
        giggity_core::model::RuntimeKind::Nerdctl => "nerdctl",
        giggity_core::model::RuntimeKind::Kubernetes => "kubectl",
        giggity_core::model::RuntimeKind::Systemd => "journalctl",
        _ => "echo",
    }
}

fn build_log_follow_args(resource: &ResourceRecord, lines: u32) -> anyhow::Result<Vec<String>> {
    let tail = lines.max(10).to_string();
    if resource.kind == ResourceKind::ComposeStack {
        anyhow::bail!("log streaming unavailable for compose stack resources");
    }
    match resource.runtime {
        giggity_core::model::RuntimeKind::Docker => {
            let id = metadata(resource, "container_id")?;
            Ok(vec![
                "logs".into(),
                "--tail".into(),
                tail,
                "--follow".into(),
                id.into(),
            ])
        }
        giggity_core::model::RuntimeKind::Podman => {
            let id = metadata(resource, "container_id").unwrap_or(&resource.name);
            Ok(vec![
                "logs".into(),
                "--tail".into(),
                tail,
                "--follow".into(),
                id.into(),
            ])
        }
        giggity_core::model::RuntimeKind::Nerdctl => {
            let id = metadata(resource, "container_id").unwrap_or(&resource.name);
            Ok(vec![
                "logs".into(),
                "--tail".into(),
                tail,
                "--follow".into(),
                id.into(),
            ])
        }
        giggity_core::model::RuntimeKind::Kubernetes => {
            let ns = resource.namespace().unwrap_or("default");
            Ok(vec![
                "logs".into(),
                "-n".into(),
                ns.into(),
                resource.name.clone(),
                "--all-containers=true".into(),
                "--tail".into(),
                tail,
                "-f".into(),
            ])
        }
        giggity_core::model::RuntimeKind::Systemd => {
            let mut args = vec!["-n".into(), tail, "--no-pager".into(), "-f".into()];
            if resource.metadata.get("domain").map(String::as_str) == Some("user") {
                args.insert(0, "--user".into());
            }
            args.push("-u".into());
            args.push(resource.name.clone());
            Ok(args)
        }
        _ => anyhow::bail!("log streaming unavailable for this resource type"),
    }
}

async fn handle_stream_events(
    mut reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    mut writer: tokio::net::unix::OwnedWriteHalf,
    event_tx: broadcast::Sender<StreamEvent>,
    _view: Option<String>,
) -> anyhow::Result<()> {
    let mut rx = event_tx.subscribe();
    let mut close_line = String::new();

    loop {
        tokio::select! {
            result = rx.recv() => {
                match result {
                    Ok(StreamEvent::StateChanged(response)) => {
                        if send_response(&mut writer, &response).await.is_err() {
                            break;
                        }
                    }
                    Ok(StreamEvent::ConfigReloaded) => {
                        if send_response(&mut writer, &ServerResponse::ConfigReloaded).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        debug!(skipped = n, "streaming client lagged behind");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            result = reader.read_line(&mut close_line) => {
                match result {
                    Ok(0) => break,
                    Ok(_) => {
                        if let Ok(ClientRequest::CloseStream) = serde_json::from_str(close_line.trim()) {
                            break;
                        }
                        close_line.clear();
                    }
                    Err(_) => break,
                }
            }
        }
    }

    let _ = send_response(
        &mut writer,
        &ServerResponse::StreamEnd {
            reason: "event stream ended".into(),
        },
    )
    .await;
    debug!("event stream closed");
    Ok(())
}

async fn fetch_logs(resource: &ResourceRecord, lines: usize) -> anyhow::Result<String> {
    let lines = lines.max(10).to_string();
    if resource.kind == ResourceKind::ComposeStack {
        return Ok("logs unavailable for compose stack resources".into());
    }
    match resource.runtime {
        giggity_core::model::RuntimeKind::Docker => {
            run_output(
                "docker",
                &[
                    "logs",
                    "--tail",
                    &lines,
                    metadata(resource, "container_id")?,
                ],
            )
            .await
        }
        giggity_core::model::RuntimeKind::Podman => {
            run_output(
                "podman",
                &[
                    "logs",
                    "--tail",
                    &lines,
                    metadata(resource, "container_id").unwrap_or(&resource.name),
                ],
            )
            .await
        }
        giggity_core::model::RuntimeKind::Nerdctl => {
            run_output(
                "nerdctl",
                &[
                    "logs",
                    "--tail",
                    &lines,
                    metadata(resource, "container_id").unwrap_or(&resource.name),
                ],
            )
            .await
        }
        giggity_core::model::RuntimeKind::Systemd => {
            let mut args = vec!["-n", &lines, "--no-pager", "-u", &resource.name];
            if resource.metadata.get("domain").map(String::as_str) == Some("user") {
                args.insert(0, "--user");
            }
            run_output("journalctl", &args).await
        }
        giggity_core::model::RuntimeKind::Kubernetes => {
            let namespace = resource.namespace().unwrap_or("default");
            run_output(
                "kubectl",
                &[
                    "logs",
                    "-n",
                    namespace,
                    &resource.name,
                    "--all-containers=true",
                    "--tail",
                    &lines,
                ],
            )
            .await
        }
        _ => Ok("logs unavailable for this resource".into()),
    }
}

async fn apply_probes(config: &Config, resources: &mut [ResourceRecord]) {
    if config.probes.is_empty() {
        return;
    }

    let compiled = config
        .probes
        .iter()
        .map(|probe| (probe, compile_match_rule(&probe.matcher)))
        .collect::<Vec<_>>();
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(3))
        .build()
        .ok();

    for resource in resources {
        let matching: Vec<_> = compiled
            .iter()
            .filter(|(_, matcher)| matches_compiled_rule(matcher, resource))
            .map(|(probe, _)| *probe)
            .collect();
        if matching.is_empty() {
            continue;
        }

        let mut failures = Vec::new();
        for probe in matching {
            match evaluate_probe(client.as_ref(), probe, resource).await {
                Ok(()) => {
                    resource
                        .metadata
                        .insert(format!("probe:{}", probe.name), "ok".into());
                }
                Err(error) => {
                    resource
                        .metadata
                        .insert(format!("probe:{}", probe.name), format!("failed: {error}"));
                    failures.push(format!("{}={error}", probe.name));
                }
            }
        }

        if failures.is_empty() {
            if resource.state == HealthState::Unknown {
                resource.state = HealthState::Healthy;
            }
        } else if !matches!(resource.state, HealthState::Crashed | HealthState::Stopped) {
            resource.state = HealthState::Degraded;
            let status = resource
                .runtime_status
                .clone()
                .unwrap_or_else(|| "probe".into());
            resource.runtime_status = Some(format!("{status}; {}", failures.join(", ")));
        }
    }
}

async fn evaluate_probe(
    client: Option<&reqwest::Client>,
    probe: &ProbeSpec,
    resource: &ResourceRecord,
) -> anyhow::Result<()> {
    match &probe.kind {
        ProbeKind::Tcp { host, port } => {
            let host = expand_template(host.as_deref().unwrap_or("127.0.0.1"), resource);
            let port = port
                .as_ref()
                .copied()
                .or_else(|| resource.ports.first().map(|binding| binding.host_port))
                .context("tcp probe has no port to target")?;
            tokio::time::timeout(
                Duration::from_millis(probe.timeout_millis),
                tokio::net::TcpStream::connect((host.as_str(), port)),
            )
            .await
            .context("tcp probe timed out")??;
            Ok(())
        }
        ProbeKind::Http {
            url,
            expected_status,
        } => {
            let client = client.context("http probe requires reqwest client")?;
            let url = expand_template(url, resource);
            let response = tokio::time::timeout(
                Duration::from_millis(probe.timeout_millis),
                client.get(url).send(),
            )
            .await
            .context("http probe timed out")??;
            let status = response.status().as_u16();
            if status == *expected_status {
                Ok(())
            } else {
                anyhow::bail!("expected {expected_status}, got {status}")
            }
        }
        ProbeKind::Command {
            program,
            args,
            contains,
        } => {
            let expanded_program = expand_template(program, resource);
            let expanded_args = args
                .iter()
                .map(|arg| expand_template(arg, resource))
                .collect::<Vec<_>>();
            let output = tokio::time::timeout(
                Duration::from_millis(probe.timeout_millis),
                Command::new(&expanded_program)
                    .args(&expanded_args)
                    .stdin(Stdio::null())
                    .output(),
            )
            .await
            .context("command probe timed out")??;
            if !output.status.success() {
                anyhow::bail!("command exited with {}", output.status);
            }
            if let Some(contains) = contains {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let expected = expand_template(contains, resource);
                if !stdout.contains(&expected) {
                    anyhow::bail!("output missing '{expected}'");
                }
            }
            Ok(())
        }
    }
}

fn expand_template(template: &str, resource: &ResourceRecord) -> String {
    let primary_port = resource
        .ports
        .first()
        .map(|binding| binding.host_port.to_string())
        .unwrap_or_default();
    template
        .replace("{id}", &resource.id)
        .replace("{name}", &resource.name)
        .replace("{project}", resource.project.as_deref().unwrap_or(""))
        .replace("{port}", &primary_port)
        .replace(
            "{runtime}",
            match resource.runtime {
                giggity_core::model::RuntimeKind::Docker => "docker",
                giggity_core::model::RuntimeKind::Podman => "podman",
                giggity_core::model::RuntimeKind::Nerdctl => "nerdctl",
                giggity_core::model::RuntimeKind::Kubernetes => "kubernetes",
                giggity_core::model::RuntimeKind::Host => "host",
                giggity_core::model::RuntimeKind::Launchd => "launchd",
                giggity_core::model::RuntimeKind::Probes => "probes",
                giggity_core::model::RuntimeKind::Systemd => "systemd",
            },
        )
}

async fn run_action(action: &ActionKind, resource: &ResourceRecord) -> anyhow::Result<String> {
    match action {
        ActionKind::Logs => fetch_logs(resource, 50).await,
        ActionKind::OpenUrl => {
            let url = resource
                .urls
                .first()
                .context("resource has no URLs to open")?
                .to_string();
            run_status(opener_program(), &[&url]).await?;
            Ok(format!("opened {url}"))
        }
        ActionKind::CopyPort => {
            let port = resource
                .ports
                .first()
                .map(|port| port.host_port.to_string())
                .context("resource has no ports to copy")?;
            if try_copy_to_clipboard(&port).await? {
                Ok(format!("copied {port}"))
            } else {
                Ok(port)
            }
        }
        ActionKind::Restart => restart_resource(resource).await,
        ActionKind::Stop => stop_resource(resource).await,
        ActionKind::ForceKill => force_kill_resource(resource).await,
    }
}

#[cfg(target_os = "macos")]
fn opener_program() -> &'static str {
    "open"
}

#[cfg(not(target_os = "macos"))]
fn opener_program() -> &'static str {
    "xdg-open"
}

async fn restart_resource(resource: &ResourceRecord) -> anyhow::Result<String> {
    if resource.kind == ResourceKind::ComposeStack {
        let project = resource
            .compose_project()
            .context("compose stack missing project name")?
            .to_owned();
        let program =
            compose_program(resource.runtime).context("unsupported runtime for compose stack")?;
        run_status(program, &["compose", "-p", &project, "restart"]).await?;
        return Ok(format!("restarted {}", resource.name));
    }
    match resource.runtime {
        giggity_core::model::RuntimeKind::Docker => {
            run_status("docker", &["restart", metadata(resource, "container_id")?]).await?;
        }
        giggity_core::model::RuntimeKind::Podman => {
            run_status(
                "podman",
                &[
                    "restart",
                    metadata(resource, "container_id").unwrap_or(&resource.name),
                ],
            )
            .await?;
        }
        giggity_core::model::RuntimeKind::Nerdctl => {
            run_status(
                "nerdctl",
                &[
                    "restart",
                    metadata(resource, "container_id").unwrap_or(&resource.name),
                ],
            )
            .await?;
        }
        giggity_core::model::RuntimeKind::Systemd => {
            let domain = resource
                .metadata
                .get("domain")
                .map(String::as_str)
                .unwrap_or("system");
            let args = if domain == "user" {
                vec!["--user", "restart", &resource.name]
            } else {
                vec!["restart", &resource.name]
            };
            run_status("systemctl", &args).await?;
        }
        giggity_core::model::RuntimeKind::Launchd => {
            let target = format!("gui/{}/{}", current_uid()?, resource.name);
            run_status("launchctl", &["kickstart", "-k", &target]).await?;
        }
        giggity_core::model::RuntimeKind::Kubernetes => {
            let namespace = resource.namespace().unwrap_or("default").to_string();
            run_status(
                "kubectl",
                &[
                    "delete",
                    "pod",
                    "-n",
                    &namespace,
                    &resource.name,
                    "--wait=false",
                ],
            )
            .await?;
        }
        giggity_core::model::RuntimeKind::Host => {
            anyhow::bail!("restart is unavailable for ad-hoc host processes");
        }
        giggity_core::model::RuntimeKind::Probes => {
            anyhow::bail!("actions are not supported for probe resources");
        }
    }
    Ok(format!("restarted {}", resource.name))
}

async fn stop_resource(resource: &ResourceRecord) -> anyhow::Result<String> {
    if resource.kind == ResourceKind::ComposeStack {
        let project = resource
            .compose_project()
            .context("compose stack missing project name")?
            .to_owned();
        let program =
            compose_program(resource.runtime).context("unsupported runtime for compose stack")?;
        run_status(program, &["compose", "-p", &project, "stop"]).await?;
        return Ok(format!("stopped {}", resource.name));
    }
    match resource.runtime {
        giggity_core::model::RuntimeKind::Docker => {
            run_status("docker", &["stop", metadata(resource, "container_id")?]).await?;
        }
        giggity_core::model::RuntimeKind::Podman => {
            run_status(
                "podman",
                &[
                    "stop",
                    metadata(resource, "container_id").unwrap_or(&resource.name),
                ],
            )
            .await?;
        }
        giggity_core::model::RuntimeKind::Nerdctl => {
            run_status(
                "nerdctl",
                &[
                    "stop",
                    metadata(resource, "container_id").unwrap_or(&resource.name),
                ],
            )
            .await?;
        }
        giggity_core::model::RuntimeKind::Systemd => {
            let domain = resource
                .metadata
                .get("domain")
                .map(String::as_str)
                .unwrap_or("system");
            let args = if domain == "user" {
                vec!["--user", "stop", &resource.name]
            } else {
                vec!["stop", &resource.name]
            };
            run_status("systemctl", &args).await?;
        }
        giggity_core::model::RuntimeKind::Launchd => {
            let target = format!("gui/{}/{}", current_uid()?, resource.name);
            run_status("launchctl", &["bootout", &target]).await?;
        }
        giggity_core::model::RuntimeKind::Kubernetes => {
            anyhow::bail!(
                "stop is not supported for kubernetes pods; use force kill to delete the pod"
            );
        }
        giggity_core::model::RuntimeKind::Host => {
            run_status("kill", &["-TERM", metadata(resource, "pid")?]).await?;
        }
        giggity_core::model::RuntimeKind::Probes => {
            anyhow::bail!("actions are not supported for probe resources");
        }
    }
    Ok(format!("stopped {}", resource.name))
}

async fn force_kill_resource(resource: &ResourceRecord) -> anyhow::Result<String> {
    match resource.kind {
        ResourceKind::ComposeStack => {
            let project = resource
                .compose_project()
                .context("compose stack missing project name")?
                .to_owned();
            let program = compose_program(resource.runtime)
                .context("unsupported runtime for compose stack")?;
            run_status(program, &["compose", "-p", &project, "kill"]).await?;
        }
        _ => match resource.runtime {
            giggity_core::model::RuntimeKind::Docker => {
                run_status("docker", &["kill", metadata(resource, "container_id")?]).await?;
            }
            giggity_core::model::RuntimeKind::Podman => {
                run_status(
                    "podman",
                    &[
                        "kill",
                        metadata(resource, "container_id").unwrap_or(&resource.name),
                    ],
                )
                .await?;
            }
            giggity_core::model::RuntimeKind::Nerdctl => {
                run_status(
                    "nerdctl",
                    &[
                        "kill",
                        metadata(resource, "container_id").unwrap_or(&resource.name),
                    ],
                )
                .await?;
            }
            giggity_core::model::RuntimeKind::Host => {
                run_status("kill", &["-9", metadata(resource, "pid")?]).await?;
            }
            giggity_core::model::RuntimeKind::Launchd => {
                let target = format!("gui/{}/{}", current_uid()?, resource.name);
                run_status("launchctl", &["kill", "9", &target]).await?;
            }
            giggity_core::model::RuntimeKind::Systemd => {
                let domain = resource
                    .metadata
                    .get("domain")
                    .map(String::as_str)
                    .unwrap_or("system");
                let args = if domain == "user" {
                    vec!["--user", "kill", "--signal=SIGKILL", &resource.name]
                } else {
                    vec!["kill", "--signal=SIGKILL", &resource.name]
                };
                run_status("systemctl", &args).await?;
            }
            giggity_core::model::RuntimeKind::Kubernetes => {
                let namespace = resource.namespace().unwrap_or("default").to_string();
                run_status(
                    "kubectl",
                    &[
                        "delete",
                        "pod",
                        "-n",
                        &namespace,
                        &resource.name,
                        "--grace-period=0",
                        "--force",
                    ],
                )
                .await?;
            }
            giggity_core::model::RuntimeKind::Probes => {
                anyhow::bail!("actions are not supported for probe resources");
            }
        },
    }
    Ok(format!("force killed {}", resource.name))
}

fn compose_program(runtime: giggity_core::model::RuntimeKind) -> Option<&'static str> {
    match runtime {
        giggity_core::model::RuntimeKind::Docker => Some("docker"),
        giggity_core::model::RuntimeKind::Podman => Some("podman"),
        giggity_core::model::RuntimeKind::Nerdctl => Some("nerdctl"),
        _ => None,
    }
}

#[rustfmt::skip]
async fn try_copy_to_clipboard(value: &str) -> anyhow::Result<bool> {
    for program in clipboard_commands() {
        let mut command = Command::new(resolve_program(program.0));
        command.args(program.1);
        command.stdin(Stdio::piped());
        let Ok(mut child) = command.spawn() else { continue; };
        if let Some(mut stdin) = child.stdin.take() { stdin.write_all(value.as_bytes()).await?; }
        let status = child.wait().await?;
        if status.success() { return Ok(true); }
    }
    Ok(false)
}

fn log_config_reload(path: &Path, config: &Config) {
    info!(path = %path.display(), views = config.views.len(), probes = config.probes.len(), "reloaded giggity config");
}

async fn wait_for_shutdown(shutdown: &mut Option<oneshot::Receiver<()>>) {
    if let Some(receiver) = shutdown.as_mut() {
        let _ = receiver.await;
    }
}

#[cfg(target_os = "macos")]
fn clipboard_commands() -> Vec<(&'static str, Vec<&'static str>)> {
    vec![("pbcopy", Vec::new())]
}

#[cfg(not(target_os = "macos"))]
fn clipboard_commands() -> Vec<(&'static str, Vec<&'static str>)> {
    vec![
        ("wl-copy", Vec::new()),
        ("xclip", vec!["-selection", "clipboard"]),
        ("xsel", vec!["--clipboard", "--input"]),
    ]
}

fn metadata<'a>(resource: &'a ResourceRecord, key: &str) -> anyhow::Result<&'a str> {
    resource
        .metadata
        .get(key)
        .map(String::as_str)
        .with_context(|| format!("resource missing metadata key '{key}'"))
}

async fn run_output(program: &str, args: &[&str]) -> anyhow::Result<String> {
    let output = Command::new(resolve_program(program))
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await?;
    if !output.status.success() {
        anyhow::bail!(
            "{} failed: {}",
            program,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

async fn run_status(program: &str, args: &[&str]) -> anyhow::Result<()> {
    let status = Command::new(resolve_program(program))
        .args(args)
        .status()
        .await?;
    if !status.success() {
        anyhow::bail!("{program} exited with {status}");
    }
    Ok(())
}

fn current_uid() -> anyhow::Result<String> {
    match std::env::var("UID") {
        Ok(uid) if !uid.trim().is_empty() => Ok(uid),
        _ => nix_like_id(),
    }
}

fn nix_like_id() -> anyhow::Result<String> {
    let output = std::process::Command::new(resolve_program("id"))
        .arg("-u")
        .output()
        .context("failed to execute 'id -u'")?;
    if !output.status.success() {
        anyhow::bail!("id -u exited with {}", output.status);
    }
    let uid = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if uid.is_empty() {
        anyhow::bail!("id -u returned an empty uid");
    }
    Ok(uid)
}

fn resolve_program(program: &str) -> PathBuf {
    #[cfg(test)]
    if let Some(path) = command_overrides()
        .lock()
        .expect("lock")
        .get(program)
        .cloned()
    {
        return path;
    }

    PathBuf::from(program)
}

#[cfg(test)]
fn command_overrides() -> &'static Mutex<BTreeMap<String, PathBuf>> {
    COMMAND_OVERRIDES.get_or_init(|| Mutex::new(BTreeMap::new()))
}

pub async fn ensure_daemon_running(
    socket_path: impl AsRef<Path>,
    config_path: Option<&Path>,
) -> anyhow::Result<()> {
    let socket_path = socket_path.as_ref();
    if fs::try_exists(socket_path).await.unwrap_or(false) {
        if socket_is_live(socket_path).await {
            return Ok(());
        }
        warn!(socket = %socket_path.display(), "removing stale giggity socket");
        let _ = fs::remove_file(socket_path).await;
    }
    let current_exe = std::env::current_exe()?;
    let mut command = Command::new(current_exe);
    command.arg("daemon");
    command.arg("--background");
    if let Some(path) = config_path {
        command.arg("--config").arg(path);
    }
    command.stdin(Stdio::null());
    command.stdout(Stdio::null());
    command.stderr(Stdio::null());
    let _child = command.spawn()?;

    for _ in 0..20 {
        if fs::try_exists(socket_path).await.unwrap_or(false) {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    anyhow::bail!("daemon did not start in time")
}

async fn socket_is_live(socket_path: &Path) -> bool {
    let client = DaemonClient::new(socket_path.to_path_buf());
    matches!(
        client.request(&ClientRequest::Ping).await,
        Ok(ServerResponse::Pong { .. })
    )
}

#[cfg(test)]
#[allow(clippy::await_holding_lock)]
mod tests {
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::path::Path;
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::Duration;

    use anyhow::Result;
    use async_trait::async_trait;
    use chrono::{TimeZone, Utc};
    use giggity_collectors::{CollectionOutput, CollectorProvider};
    use giggity_core::config::{Config, MatchRule, ProbeKind, ProbeSpec, ProbeType};
    use giggity_core::model::{
        HealthState, PortBinding, ResourceKind, ResourceRecord, RuntimeKind,
    };
    use giggity_core::protocol::{ActionKind, ClientRequest, RenderFormat, ServerResponse};
    use tempfile::tempdir;
    use tokio::io::AsyncWriteExt;
    use tokio::sync::{RwLock, oneshot};

    use crate::daemon::{
        DaemonClient, Store, apply_probes, clipboard_commands, command_overrides, current_uid,
        dispatch_notifications, ensure_daemon_running, expand_template, fetch_logs, handle_request,
        log_config_reload, metadata, nix_like_id, run_action, run_daemon,
        run_daemon_with_collector, run_output, run_status, socket_is_live, try_copy_to_clipboard,
        wait_for_shutdown,
    };

    static TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    #[derive(Debug)]
    struct FakeCollector;

    #[derive(Debug)]
    struct ErrorCollector;

    #[async_trait]
    impl CollectorProvider for FakeCollector {
        async fn collect(&self, _config: &Config) -> Result<CollectionOutput> {
            Ok(CollectionOutput {
                resources: vec![docker_resource()],
                warnings: Vec::new(),
            })
        }
    }

    #[async_trait]
    impl CollectorProvider for ErrorCollector {
        async fn collect(&self, _config: &Config) -> Result<CollectionOutput> {
            anyhow::bail!("collector boom")
        }
    }

    fn docker_resource() -> ResourceRecord {
        ResourceRecord {
            id: "docker:web".into(),
            kind: ResourceKind::Container,
            runtime: RuntimeKind::Docker,
            project: Some("stack".into()),
            name: "web".into(),
            state: HealthState::Crashed,
            runtime_status: Some("Exited (1)".into()),
            ports: Vec::new(),
            labels: BTreeMap::new(),
            urls: Vec::new(),
            metadata: BTreeMap::new(),
            last_changed: Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap(),
            state_since: Utc::now(),
        }
    }

    fn host_resource() -> ResourceRecord {
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
            last_changed: Utc::now(),
            state_since: Utc::now(),
        }
    }

    fn store_with(resource: ResourceRecord) -> Arc<RwLock<Store>> {
        Arc::new(RwLock::new(Store {
            config: Config::default(),
            snapshot: giggity_core::model::Snapshot {
                resources: vec![resource],
                ..Default::default()
            },
            last_notified: Default::default(),
            muted_until: None,
        }))
    }

    fn write_script(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path).expect("metadata").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).expect("chmod");
        }
        path
    }

    fn reset_overrides() {
        command_overrides().lock().expect("lock").clear();
    }

    fn expect_query_response(response: ServerResponse) -> giggity_core::model::Snapshot {
        match response {
            ServerResponse::Query { snapshot } => snapshot,
            other => panic!("unexpected response: {other:?}"),
        }
    }

    fn expect_rendered_output(response: ServerResponse) -> String {
        match response {
            ServerResponse::Rendered { output } => output,
            other => panic!("unexpected response: {other:?}"),
        }
    }

    fn expect_pong_version(response: ServerResponse) -> u8 {
        match response {
            ServerResponse::Pong { api_version } => api_version,
            other => panic!("unexpected response: {other:?}"),
        }
    }

    fn expect_error_message(response: ServerResponse) -> String {
        match response {
            ServerResponse::Error { message } => message,
            other => panic!("unexpected response: {other:?}"),
        }
    }

    fn podman_resource() -> ResourceRecord {
        ResourceRecord {
            runtime: RuntimeKind::Podman,
            metadata: BTreeMap::from([("container_id".into(), "pod-1".into())]),
            ..docker_resource()
        }
    }

    fn nerdctl_resource() -> ResourceRecord {
        ResourceRecord {
            runtime: RuntimeKind::Nerdctl,
            metadata: BTreeMap::from([("container_id".into(), "nerd-1".into())]),
            ..docker_resource()
        }
    }

    fn kubernetes_resource() -> ResourceRecord {
        ResourceRecord {
            id: "kubernetes:dev:api-123".into(),
            kind: ResourceKind::KubernetesPod,
            runtime: RuntimeKind::Kubernetes,
            project: Some("dev".into()),
            name: "api-123".into(),
            state: HealthState::Healthy,
            runtime_status: Some("Running".into()),
            ports: Vec::new(),
            labels: BTreeMap::from([("app".into(), "api".into())]),
            urls: Vec::new(),
            metadata: BTreeMap::from([("namespace".into(), "dev".into())]),
            last_changed: Utc::now(),
            state_since: Utc::now(),
        }
    }

    fn compose_stack_resource() -> ResourceRecord {
        ResourceRecord {
            id: "compose:docker:stack".into(),
            kind: ResourceKind::ComposeStack,
            runtime: RuntimeKind::Docker,
            project: Some("stack".into()),
            name: "stack stack".into(),
            state: HealthState::Degraded,
            runtime_status: Some("1/2 healthy".into()),
            ports: vec![PortBinding {
                host_ip: None,
                host_port: 8080,
                container_port: Some(80),
                protocol: "tcp".into(),
            }],
            labels: BTreeMap::from([("com.docker.compose.project".into(), "stack".into())]),
            urls: vec!["http://127.0.0.1:8080".parse().expect("url")],
            metadata: BTreeMap::from([("compose_project".into(), "stack".into())]),
            last_changed: Utc::now(),
            state_since: Utc::now(),
        }
    }

    fn systemd_resource(domain: &str) -> ResourceRecord {
        ResourceRecord {
            id: format!("systemd:{domain}:svc"),
            kind: ResourceKind::SystemdUnit,
            runtime: RuntimeKind::Systemd,
            project: None,
            name: "svc.service".into(),
            state: HealthState::Healthy,
            runtime_status: Some("active/running".into()),
            ports: Vec::new(),
            labels: BTreeMap::new(),
            urls: Vec::new(),
            metadata: BTreeMap::from([("domain".into(), domain.into())]),
            last_changed: Utc::now(),
            state_since: Utc::now(),
        }
    }

    fn launchd_resource() -> ResourceRecord {
        ResourceRecord {
            id: "launchd:com.example.web".into(),
            kind: ResourceKind::LaunchdUnit,
            runtime: RuntimeKind::Launchd,
            project: None,
            name: "com.example.web".into(),
            state: HealthState::Healthy,
            runtime_status: Some("0".into()),
            ports: Vec::new(),
            labels: BTreeMap::new(),
            urls: Vec::new(),
            metadata: BTreeMap::from([("pid".into(), "99".into())]),
            last_changed: Utc::now(),
            state_since: Utc::now(),
        }
    }

    #[tokio::test]
    async fn daemon_serves_query_and_render_requests() {
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
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let task = tokio::spawn(run_daemon_with_collector(
            Some(config_path.clone()),
            Arc::new(FakeCollector),
            Some(shutdown_rx),
        ));

        tokio::time::sleep(Duration::from_millis(250)).await;
        let client = DaemonClient::new(dir.path().join("giggity.sock"));
        let snapshot = expect_query_response(
            client
                .request(&ClientRequest::Query { view: None })
                .await
                .expect("query"),
        );
        assert_eq!(snapshot.resources.len(), 1);

        let output = expect_rendered_output(
            client
                .request(&ClientRequest::Render {
                    view: None,
                    format: RenderFormat::Plain,
                })
                .await
                .expect("render"),
        );
        assert!(output.contains("down 1"));

        let _ = shutdown_tx.send(());
        let _ = task.await.expect("daemon result");
    }

    #[tokio::test]
    async fn daemon_handles_bad_requests_reloads_valid_config_and_renders_tmux() {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        let socket_path = dir.path().join("giggity.sock");
        std::fs::write(
            &config_path,
            format!(
                "cache_dir = '{}'\nsocket_path = '{}'\nrefresh_seconds = 1\n",
                dir.path().display(),
                socket_path.display()
            ),
        )
        .expect("config");
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let task = tokio::spawn(run_daemon_with_collector(
            Some(config_path.clone()),
            Arc::new(FakeCollector),
            Some(shutdown_rx),
        ));
        tokio::time::sleep(Duration::from_millis(250)).await;

        let mut stream = tokio::net::UnixStream::connect(&socket_path)
            .await
            .expect("connect");
        stream.write_all(b"{bad json}\n").await.expect("write");
        stream.shutdown().await.expect("shutdown");
        tokio::time::sleep(Duration::from_millis(100)).await;

        let client = DaemonClient::new(socket_path.clone());
        let output = expect_rendered_output(
            client
                .request(&ClientRequest::Render {
                    view: None,
                    format: RenderFormat::Tmux,
                })
                .await
                .expect("tmux render"),
        );
        assert!(output.contains("#[fg="));

        std::fs::write(
            &config_path,
            format!(
                "cache_dir = '{}'\nsocket_path = '{}'\nrefresh_seconds = 2\n[views.ops]\n",
                dir.path().display(),
                socket_path.display()
            ),
        )
        .expect("update config");
        tokio::time::sleep(Duration::from_millis(1_100)).await;

        assert_eq!(
            expect_pong_version(
                client
                    .request(&ClientRequest::Ping)
                    .await
                    .expect("ping after reload")
            ),
            1
        );

        let _ = shutdown_tx.send(());
        let _ = task.await.expect("daemon result");
    }

    #[test]
    fn daemon_response_helpers_cover_success_and_panic_paths() {
        assert_eq!(
            expect_query_response(ServerResponse::Query {
                snapshot: giggity_core::model::Snapshot::default(),
            })
            .resources
            .len(),
            0
        );
        assert_eq!(
            expect_rendered_output(ServerResponse::Rendered {
                output: "ok".into(),
            }),
            "ok"
        );
        assert_eq!(
            expect_pong_version(ServerResponse::Pong { api_version: 1 }),
            1
        );
        assert_eq!(
            expect_error_message(ServerResponse::Error {
                message: "nope".into(),
            }),
            "nope"
        );

        assert!(
            std::panic::catch_unwind(|| expect_query_response(ServerResponse::Pong {
                api_version: 1
            }))
            .is_err()
        );
        assert!(
            std::panic::catch_unwind(|| {
                expect_rendered_output(ServerResponse::Validation {
                    warnings: Vec::new(),
                })
            })
            .is_err()
        );
        assert!(
            std::panic::catch_unwind(|| {
                expect_pong_version(ServerResponse::Rendered {
                    output: String::new(),
                })
            })
            .is_err()
        );
        assert!(
            std::panic::catch_unwind(|| {
                expect_error_message(ServerResponse::Logs {
                    content: String::new(),
                })
            })
            .is_err()
        );
    }

    #[test]
    fn log_config_reload_executes_without_panicking() {
        log_config_reload(Path::new("/tmp/giggity.toml"), &Config::default());
    }

    #[tokio::test]
    async fn wait_for_shutdown_covers_missing_and_present_receivers() {
        let mut missing = None;
        wait_for_shutdown(&mut missing).await;

        let (tx, rx) = oneshot::channel();
        let mut shutdown = Some(rx);
        let _ = tx.send(());
        wait_for_shutdown(&mut shutdown).await;
    }

    #[tokio::test]
    async fn try_copy_to_clipboard_covers_success_and_failure_statuses() {
        let _guard = TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("lock");
        reset_overrides();
        let dir = tempdir().expect("tempdir");
        {
            let mut overrides = command_overrides().lock().expect("lock");
            for (program, _) in clipboard_commands() {
                overrides.insert(
                    program.into(),
                    write_script(dir.path(), program, "cat >/dev/null"),
                );
            }
        }
        assert!(try_copy_to_clipboard("3000").await.expect("copy success"));

        {
            let mut overrides = command_overrides().lock().expect("lock");
            for (program, _) in clipboard_commands() {
                overrides.insert(program.into(), write_script(dir.path(), program, "exit 1"));
            }
        }
        assert!(!try_copy_to_clipboard("3000").await.expect("copy failure"));
        reset_overrides();
    }

    #[tokio::test]
    async fn tcp_probe_preserves_healthy_resource() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let port = listener.local_addr().expect("addr").port();
        let mut resources = vec![ResourceRecord {
            id: "host:api".into(),
            kind: ResourceKind::HostProcess,
            runtime: RuntimeKind::Host,
            project: None,
            name: "api".into(),
            state: HealthState::Healthy,
            runtime_status: Some("listening".into()),
            ports: vec![PortBinding {
                host_ip: None,
                host_port: port,
                container_port: None,
                protocol: "tcp".into(),
            }],
            labels: BTreeMap::new(),
            urls: Vec::new(),
            metadata: BTreeMap::new(),
            last_changed: Utc::now(),
            state_since: Utc::now(),
        }];
        let mut config = Config::default();
        config.probes.push(ProbeSpec {
            name: "tcp".into(),
            matcher: MatchRule {
                name_regex: Some("^api$".into()),
                ..MatchRule::default()
            },
            kind: ProbeKind::Tcp {
                host: None,
                port: None,
            },
            timeout_millis: 500,
            retries: 0,
            backoff_secs: 2,
            warn_latency_ms: None,
            critical_latency_ms: None,
            interval_secs: 30,
            probe_type: ProbeType::default(),
        });

        apply_probes(&config, &mut resources).await;
        assert_eq!(resources[0].state, HealthState::Healthy);
        assert_eq!(resources[0].metadata["probe:tcp"], "ok");
    }

    #[tokio::test]
    async fn command_probe_degrades_on_failed_output_match() {
        let mut resources = vec![ResourceRecord {
            id: "host:web".into(),
            kind: ResourceKind::HostProcess,
            runtime: RuntimeKind::Host,
            project: None,
            name: "web".into(),
            state: HealthState::Healthy,
            runtime_status: Some("listening".into()),
            ports: Vec::new(),
            labels: BTreeMap::new(),
            urls: Vec::new(),
            metadata: BTreeMap::new(),
            last_changed: Utc::now(),
            state_since: Utc::now(),
        }];
        let mut config = Config::default();
        config.probes.push(ProbeSpec {
            name: "cmd".into(),
            matcher: MatchRule {
                name_regex: Some("^web$".into()),
                ..MatchRule::default()
            },
            kind: ProbeKind::Command {
                program: "sh".into(),
                args: vec!["-c".into(), "echo nope".into()],
                contains: Some("ok".into()),
            },
            timeout_millis: 500,
            retries: 0,
            backoff_secs: 2,
            warn_latency_ms: None,
            critical_latency_ms: None,
            interval_secs: 30,
            probe_type: ProbeType::default(),
        });

        apply_probes(&config, &mut resources).await;
        assert_eq!(resources[0].state, HealthState::Degraded);
        assert!(resources[0].metadata["probe:cmd"].contains("failed"));
    }

    #[tokio::test]
    async fn failing_probes_set_runtime_status_when_missing() {
        let mut resources = vec![ResourceRecord {
            state: HealthState::Healthy,
            runtime_status: None,
            ..host_resource()
        }];
        let mut config = Config::default();
        config.probes.push(ProbeSpec {
            name: "cmd-miss".into(),
            matcher: MatchRule {
                name_regex: Some("^api$".into()),
                ..MatchRule::default()
            },
            kind: ProbeKind::Command {
                program: "sh".into(),
                args: vec!["-c".into(), "echo nope".into()],
                contains: Some("ok".into()),
            },
            timeout_millis: 500,
            retries: 0,
            backoff_secs: 2,
            warn_latency_ms: None,
            critical_latency_ms: None,
            interval_secs: 30,
            probe_type: ProbeType::default(),
        });

        apply_probes(&config, &mut resources).await;
        assert_eq!(resources[0].state, HealthState::Degraded);
        assert!(
            resources[0]
                .runtime_status
                .as_deref()
                .unwrap_or("")
                .starts_with("probe;")
        );
    }

    #[tokio::test]
    async fn probes_cover_no_match_http_failure_and_command_success() {
        let mut resources = vec![ResourceRecord {
            state: HealthState::Stopped,
            runtime_status: None,
            ..host_resource()
        }];
        let mut config = Config::default();
        config.probes.push(ProbeSpec {
            name: "no-match".into(),
            matcher: MatchRule {
                name_regex: Some("^other$".into()),
                ..MatchRule::default()
            },
            kind: ProbeKind::Tcp {
                host: None,
                port: Some(1),
            },
            timeout_millis: 10,
            retries: 0,
            backoff_secs: 2,
            warn_latency_ms: None,
            critical_latency_ms: None,
            interval_secs: 30,
            probe_type: ProbeType::default(),
        });
        apply_probes(&config, &mut resources).await;
        assert_eq!(resources[0].state, HealthState::Stopped);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let port = listener.local_addr().expect("addr").port();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let _ = stream.readable().await;
            let _ = stream
                .write_all(b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\n\r\n")
                .await;
        });
        let mut resources = vec![ResourceRecord {
            state: HealthState::Unknown,
            ports: vec![PortBinding {
                host_ip: None,
                host_port: port,
                container_port: None,
                protocol: "tcp".into(),
            }],
            ..host_resource()
        }];
        let mut config = Config::default();
        config.probes.push(ProbeSpec {
            name: "http-bad".into(),
            matcher: MatchRule {
                name_regex: Some("^api$".into()),
                ..MatchRule::default()
            },
            kind: ProbeKind::Http {
                url: format!("http://127.0.0.1:{port}"),
                expected_status: 200,
            },
            timeout_millis: 1_000,
            retries: 0,
            backoff_secs: 2,
            warn_latency_ms: None,
            critical_latency_ms: None,
            interval_secs: 30,
            probe_type: ProbeType::default(),
        });
        apply_probes(&config, &mut resources).await;
        assert_eq!(resources[0].state, HealthState::Degraded);
        tokio::time::timeout(Duration::from_secs(1), server)
            .await
            .expect("server timed out")
            .expect("server");

        let mut resources = vec![ResourceRecord {
            state: HealthState::Unknown,
            ..host_resource()
        }];
        let mut config = Config::default();
        config.probes.push(ProbeSpec {
            name: "cmd-ok".into(),
            matcher: MatchRule {
                name_regex: Some("^api$".into()),
                ..MatchRule::default()
            },
            kind: ProbeKind::Command {
                program: "sh".into(),
                args: vec!["-c".into(), "echo ok".into()],
                contains: None,
            },
            timeout_millis: 500,
            retries: 0,
            backoff_secs: 2,
            warn_latency_ms: None,
            critical_latency_ms: None,
            interval_secs: 30,
            probe_type: ProbeType::default(),
        });
        apply_probes(&config, &mut resources).await;
        assert_eq!(resources[0].state, HealthState::Healthy);
    }

    #[tokio::test]
    async fn http_probe_can_promote_unknown_resource_to_healthy() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let port = listener.local_addr().expect("addr").port();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let _ = stream.readable().await;
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok")
                .await;
        });

        let mut resources = vec![ResourceRecord {
            state: HealthState::Unknown,
            ports: vec![PortBinding {
                host_ip: None,
                host_port: port,
                container_port: None,
                protocol: "tcp".into(),
            }],
            ..host_resource()
        }];
        let mut config = Config::default();
        config.probes.push(ProbeSpec {
            name: "http".into(),
            matcher: MatchRule {
                name_regex: Some("^api$".into()),
                ..MatchRule::default()
            },
            kind: ProbeKind::Http {
                url: format!("http://127.0.0.1:{port}"),
                expected_status: 200,
            },
            timeout_millis: 1_000,
            retries: 0,
            backoff_secs: 2,
            warn_latency_ms: None,
            critical_latency_ms: None,
            interval_secs: 30,
            probe_type: ProbeType::default(),
        });

        apply_probes(&config, &mut resources).await;
        assert_eq!(resources[0].state, HealthState::Healthy);
        assert_eq!(resources[0].metadata["probe:http"], "ok");
        tokio::time::timeout(Duration::from_secs(1), server)
            .await
            .expect("server timed out")
            .expect("server join");
    }

    #[tokio::test]
    async fn command_probe_with_contains_can_promote_unknown_resource() {
        let mut resources = vec![ResourceRecord {
            state: HealthState::Unknown,
            ..host_resource()
        }];
        let mut config = Config::default();
        config.probes.push(ProbeSpec {
            name: "cmd-contains".into(),
            matcher: MatchRule {
                name_regex: Some("^api$".into()),
                ..MatchRule::default()
            },
            kind: ProbeKind::Command {
                program: "sh".into(),
                args: vec!["-c".into(), "printf 'ready api 3000'".into()],
                contains: Some("{name} {port}".into()),
            },
            timeout_millis: 500,
            retries: 0,
            backoff_secs: 2,
            warn_latency_ms: None,
            critical_latency_ms: None,
            interval_secs: 30,
            probe_type: ProbeType::default(),
        });

        apply_probes(&config, &mut resources).await;
        assert_eq!(resources[0].state, HealthState::Healthy);
        assert_eq!(resources[0].metadata["probe:cmd-contains"], "ok");
    }

    #[tokio::test]
    async fn failing_probe_does_not_override_crashed_resource_state() {
        let mut resources = vec![ResourceRecord {
            state: HealthState::Crashed,
            runtime_status: Some("Exited (1)".into()),
            ..host_resource()
        }];
        let mut config = Config::default();
        config.probes.push(ProbeSpec {
            name: "cmd-fail".into(),
            matcher: MatchRule {
                name_regex: Some("^api$".into()),
                ..MatchRule::default()
            },
            kind: ProbeKind::Command {
                program: "sh".into(),
                args: vec!["-c".into(), "echo nope".into()],
                contains: Some("ok".into()),
            },
            timeout_millis: 500,
            retries: 0,
            backoff_secs: 2,
            warn_latency_ms: None,
            critical_latency_ms: None,
            interval_secs: 30,
            probe_type: ProbeType::default(),
        });

        apply_probes(&config, &mut resources).await;
        assert_eq!(resources[0].state, HealthState::Crashed);
        assert_eq!(resources[0].runtime_status.as_deref(), Some("Exited (1)"));
        assert!(resources[0].metadata["probe:cmd-fail"].contains("failed"));
    }

    #[tokio::test]
    async fn handle_request_requires_confirmation_for_mutations() {
        let response = handle_request(
            ClientRequest::Action {
                action: ActionKind::Restart,
                resource_id: docker_resource().id,
                confirm: false,
            },
            store_with(docker_resource()),
        )
        .await;

        match response {
            ServerResponse::Error { message } => {
                assert!(message.contains("require confirmation"));
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[tokio::test]
    async fn handle_request_returns_unknown_resource_error() {
        let response = handle_request(
            ClientRequest::Logs {
                resource_id: "missing".into(),
                lines: 25,
            },
            store_with(host_resource()),
        )
        .await;
        assert!(expect_error_message(response).contains("unknown resource"));

        let response = handle_request(
            ClientRequest::Action {
                action: ActionKind::Logs,
                resource_id: "missing".into(),
                confirm: false,
            },
            store_with(host_resource()),
        )
        .await;
        assert!(expect_error_message(response).contains("unknown resource"));
    }

    #[tokio::test]
    async fn logs_action_for_host_resource_is_non_destructive() {
        let message = run_action(&ActionKind::Logs, &host_resource())
            .await
            .expect("logs action");
        assert_eq!(message, "logs unavailable for this resource");
    }

    #[tokio::test]
    async fn fetch_logs_uses_runtime_specific_commands() {
        let _guard = TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("lock");
        reset_overrides();
        let dir = tempdir().expect("tempdir");
        let docker = write_script(dir.path(), "docker", "printf 'docker-log'");
        let podman = write_script(dir.path(), "podman", "printf 'podman-log'");
        let nerdctl = write_script(dir.path(), "nerdctl", "printf 'nerdctl-log'");
        let kubectl = write_script(dir.path(), "kubectl", "printf 'kube-log'");
        let journalctl = write_script(dir.path(), "journalctl", "printf 'journal-log'");
        {
            let mut overrides = command_overrides().lock().expect("lock");
            overrides.insert("docker".into(), docker);
            overrides.insert("podman".into(), podman);
            overrides.insert("nerdctl".into(), nerdctl);
            overrides.insert("kubectl".into(), kubectl);
            overrides.insert("journalctl".into(), journalctl);
        }

        let mut docker_resource = docker_resource();
        docker_resource
            .metadata
            .insert("container_id".into(), "abc".into());
        assert_eq!(
            fetch_logs(&docker_resource, 5).await.expect("docker"),
            "docker-log"
        );
        assert_eq!(
            fetch_logs(&podman_resource(), 5).await.expect("podman"),
            "podman-log"
        );
        assert_eq!(
            fetch_logs(&nerdctl_resource(), 5).await.expect("nerdctl"),
            "nerdctl-log"
        );
        assert_eq!(
            fetch_logs(&kubernetes_resource(), 5)
                .await
                .expect("kubernetes"),
            "kube-log"
        );
        assert_eq!(
            fetch_logs(&systemd_resource("user"), 5)
                .await
                .expect("systemd"),
            "journal-log"
        );
        assert_eq!(
            fetch_logs(&systemd_resource("system"), 5)
                .await
                .expect("systemd system"),
            "journal-log"
        );
        assert_eq!(
            fetch_logs(&compose_stack_resource(), 5)
                .await
                .expect("compose stack"),
            "logs unavailable for compose stack resources"
        );
        reset_overrides();
    }

    #[tokio::test]
    async fn copy_port_action_requires_port() {
        let error = run_action(
            &ActionKind::CopyPort,
            &ResourceRecord {
                ports: Vec::new(),
                ..host_resource()
            },
        )
        .await
        .expect_err("missing port");
        assert!(error.to_string().contains("no ports"));
    }

    #[tokio::test]
    async fn copy_port_returns_plain_value_when_clipboard_copy_fails() {
        let _guard = TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("lock");
        reset_overrides();
        let dir = tempdir().expect("tempdir");
        let pbcopy = write_script(dir.path(), "pbcopy", "exit 1");
        command_overrides()
            .lock()
            .expect("lock")
            .insert("pbcopy".into(), pbcopy);

        let message = run_action(&ActionKind::CopyPort, &host_resource())
            .await
            .expect("copy fallback");
        assert_eq!(message, "3000");
        reset_overrides();
    }

    #[tokio::test]
    async fn metadata_reports_missing_key() {
        let error = metadata(&docker_resource(), "container_id").expect_err("missing key");
        assert!(error.to_string().contains("missing metadata key"));
    }

    #[tokio::test]
    async fn runtime_actions_cover_restart_stop_open_and_copy() {
        let _guard = TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("lock");
        reset_overrides();
        let dir = tempdir().expect("tempdir");
        let open = write_script(
            dir.path(),
            "open",
            "printf \"$@\" > \"$TMPDIR/giggity-open.log\"",
        );
        let pbcopy = write_script(dir.path(), "pbcopy", "cat > \"$TMPDIR/giggity-copy.log\"");
        let docker = write_script(dir.path(), "docker", "exit 0");
        let podman = write_script(dir.path(), "podman", "exit 0");
        let nerdctl = write_script(dir.path(), "nerdctl", "exit 0");
        let kubectl = write_script(dir.path(), "kubectl", "exit 0");
        let systemctl = write_script(
            dir.path(),
            "systemctl",
            "printf '%s\\n' \"$*\" >> \"$TMPDIR/giggity-systemctl.log\"",
        );
        let launchctl = write_script(dir.path(), "launchctl", "exit 0");
        let kill = write_script(dir.path(), "kill", "exit 0");
        let id = write_script(dir.path(), "id", "printf '501\\n'");
        {
            let mut overrides = command_overrides().lock().expect("lock");
            overrides.insert("open".into(), open);
            overrides.insert("pbcopy".into(), pbcopy);
            overrides.insert("docker".into(), docker);
            overrides.insert("podman".into(), podman);
            overrides.insert("nerdctl".into(), nerdctl);
            overrides.insert("kubectl".into(), kubectl);
            overrides.insert("systemctl".into(), systemctl);
            overrides.insert("launchctl".into(), launchctl);
            overrides.insert("kill".into(), kill);
            overrides.insert("id".into(), id);
        }

        let tmpdir = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into());
        let _ = std::fs::remove_file(format!("{tmpdir}/giggity-systemctl.log"));
        let open_result = run_action(&ActionKind::OpenUrl, &host_resource())
            .await
            .expect("open url");
        assert!(open_result.contains("opened"));
        let copy_result = run_action(&ActionKind::CopyPort, &host_resource())
            .await
            .expect("copy port");
        assert!(copy_result.contains("copied"));

        let mut docker_resource = docker_resource();
        docker_resource
            .metadata
            .insert("container_id".into(), "abc".into());
        assert!(
            run_action(&ActionKind::Restart, &docker_resource)
                .await
                .expect("docker restart")
                .contains("restarted")
        );
        assert!(
            run_action(&ActionKind::Restart, &podman_resource())
                .await
                .expect("podman restart")
                .contains("restarted")
        );
        assert!(
            run_action(&ActionKind::Restart, &nerdctl_resource())
                .await
                .expect("nerdctl restart")
                .contains("restarted")
        );
        assert!(
            run_action(&ActionKind::Restart, &systemd_resource("user"))
                .await
                .expect("systemd restart")
                .contains("restarted")
        );
        assert!(
            run_action(&ActionKind::Restart, &systemd_resource("system"))
                .await
                .expect("systemd system restart")
                .contains("restarted")
        );
        assert!(
            run_action(&ActionKind::Restart, &launchd_resource())
                .await
                .expect("launchd restart")
                .contains("restarted")
        );
        assert!(
            run_action(&ActionKind::Restart, &host_resource())
                .await
                .expect_err("host restart")
                .to_string()
                .contains("unavailable")
        );
        assert!(
            run_action(&ActionKind::Restart, &kubernetes_resource())
                .await
                .expect("kubernetes restart")
                .contains("restarted")
        );
        assert!(
            run_action(&ActionKind::Restart, &compose_stack_resource())
                .await
                .expect("compose restart")
                .contains("restarted")
        );

        assert!(
            run_action(&ActionKind::Stop, &docker_resource)
                .await
                .expect("docker stop")
                .contains("stopped")
        );
        assert!(
            run_action(&ActionKind::Stop, &podman_resource())
                .await
                .expect("podman stop")
                .contains("stopped")
        );
        assert!(
            run_action(&ActionKind::Stop, &nerdctl_resource())
                .await
                .expect("nerdctl stop")
                .contains("stopped")
        );
        assert!(
            run_action(&ActionKind::Stop, &systemd_resource("user"))
                .await
                .expect("systemd user stop")
                .contains("stopped")
        );
        assert!(
            run_action(&ActionKind::Stop, &systemd_resource("system"))
                .await
                .expect("systemd stop")
                .contains("stopped")
        );
        assert!(
            run_action(&ActionKind::Stop, &launchd_resource())
                .await
                .expect("launchd stop")
                .contains("stopped")
        );
        assert!(
            run_action(&ActionKind::Stop, &host_resource())
                .await
                .expect("host stop")
                .contains("stopped")
        );
        assert!(
            run_action(&ActionKind::Stop, &kubernetes_resource())
                .await
                .expect_err("kubernetes stop")
                .to_string()
                .contains("not supported")
        );
        assert!(
            run_action(&ActionKind::Stop, &compose_stack_resource())
                .await
                .expect("compose stop")
                .contains("stopped")
        );

        assert!(
            std::fs::read_to_string(format!("{tmpdir}/giggity-open.log"))
                .expect("open log")
                .contains("http://127.0.0.1:3000/")
        );
        assert_eq!(
            std::fs::read_to_string(format!("{tmpdir}/giggity-copy.log")).expect("copy log"),
            "3000"
        );
        let systemctl_log = std::fs::read_to_string(format!("{tmpdir}/giggity-systemctl.log"))
            .expect("systemctl log");
        let systemctl_lines = systemctl_log.lines().collect::<Vec<_>>();
        assert_eq!(
            &systemctl_lines[systemctl_lines.len().saturating_sub(4)..],
            vec![
                "--user restart svc.service",
                "restart svc.service",
                "--user stop svc.service",
                "stop svc.service",
            ]
        );
        reset_overrides();
    }

    #[tokio::test]
    async fn handle_request_surfaces_action_and_log_failures() {
        let response = handle_request(
            ClientRequest::Logs {
                resource_id: docker_resource().id,
                lines: 10,
            },
            store_with(docker_resource()),
        )
        .await;
        assert!(expect_error_message(response).contains("missing metadata key"));

        let response = handle_request(
            ClientRequest::Action {
                action: ActionKind::OpenUrl,
                resource_id: "host:api".into(),
                confirm: false,
            },
            store_with(ResourceRecord {
                urls: Vec::new(),
                ..host_resource()
            }),
        )
        .await;
        assert!(expect_error_message(response).contains("no URLs"));
    }

    #[tokio::test]
    async fn socket_liveness_checks_distinguish_live_and_stale_paths() {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        let socket_path = dir.path().join("giggity.sock");
        std::fs::write(
            &config_path,
            format!(
                "cache_dir = '{}'\nsocket_path = '{}'\n",
                dir.path().display(),
                socket_path.display()
            ),
        )
        .expect("config");

        tokio::fs::write(&socket_path, b"stale")
            .await
            .expect("stale socket placeholder");
        assert!(!socket_is_live(&socket_path).await);

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let task = tokio::spawn(run_daemon_with_collector(
            Some(config_path.clone()),
            Arc::new(FakeCollector),
            Some(shutdown_rx),
        ));
        tokio::time::sleep(Duration::from_millis(250)).await;

        assert!(socket_is_live(&socket_path).await);
        ensure_daemon_running(&socket_path, Some(&config_path))
            .await
            .expect("ensure running");

        let _ = shutdown_tx.send(());
        let _ = task.await.expect("daemon result");
    }

    #[tokio::test]
    async fn ensure_daemon_running_removes_stale_socket_before_waiting() {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        let stale_path = dir.path().join("stale.sock");
        std::fs::write(
            &config_path,
            format!(
                "cache_dir = '{}'\nsocket_path = '{}'\nrefresh_seconds = 1\n[sources]\ndocker = false\npodman = false\nnerdctl = false\nhost_listeners = false\nlaunchd = false\nsystemd = false\n",
                dir.path().display(),
                stale_path.display()
            ),
        )
        .expect("config");
        tokio::fs::write(&stale_path, b"stale")
            .await
            .expect("stale file");

        let delayed = stale_path.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let _ = tokio::fs::write(delayed, b"socket").await;
        });

        ensure_daemon_running(&stale_path, Some(&config_path))
            .await
            .expect("stale startup");
        assert!(stale_path.exists());
    }

    #[tokio::test]
    async fn ensure_daemon_running_uses_default_config_path_when_not_provided() {
        let _guard = TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("lock");
        let dir = tempdir().expect("tempdir");
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).expect("home dir");
        let delayed_path = dir.path().join("default.sock");
        let _env = giggity_core::test_support::EnvVarGuard::set_many([
            ("HOME", Some(home.as_os_str().to_os_string())),
            ("XDG_CONFIG_HOME", None::<OsString>),
        ]);
        let default_config_path = Config::default_path();
        std::fs::create_dir_all(default_config_path.parent().expect("config parent"))
            .expect("config parent");
        std::fs::write(
            &default_config_path,
            format!(
                "cache_dir = '{}'\nsocket_path = '{}'\nrefresh_seconds = 1\n[sources]\ndocker = false\npodman = false\nnerdctl = false\nhost_listeners = false\nlaunchd = false\nsystemd = false\n",
                dir.path().display(),
                delayed_path.display()
            ),
        )
        .expect("default config");

        let delayed_clone = delayed_path.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let _ = tokio::fs::write(delayed_clone, b"socket").await;
        });

        ensure_daemon_running(&delayed_path, None)
            .await
            .expect("default-config startup");
        assert!(delayed_path.exists());
    }

    #[tokio::test]
    async fn ensure_daemon_running_handles_initial_try_exists_permission_errors() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let dir = tempdir().expect("tempdir");
            let config_path = dir.path().join("config.toml");
            let sealed_dir = dir.path().join("sealed");
            std::fs::create_dir_all(&sealed_dir).expect("sealed dir");
            let socket_path = sealed_dir.join("giggity.sock");
            std::fs::write(
                &config_path,
                format!(
                    "cache_dir = '{}'\nsocket_path = '{}'\nrefresh_seconds = 1\n[sources]\ndocker = false\npodman = false\nnerdctl = false\nhost_listeners = false\nlaunchd = false\nsystemd = false\n",
                    dir.path().display(),
                    socket_path.display()
                ),
            )
            .expect("config");

            let mut perms = std::fs::metadata(&sealed_dir)
                .expect("metadata")
                .permissions();
            perms.set_mode(0o000);
            std::fs::set_permissions(&sealed_dir, perms).expect("seal dir");

            let sealed_dir_clone = sealed_dir.clone();
            let socket_clone = socket_path.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(200)).await;
                let mut perms = std::fs::metadata(&sealed_dir_clone)
                    .expect("metadata")
                    .permissions();
                perms.set_mode(0o755);
                std::fs::set_permissions(&sealed_dir_clone, perms).expect("unseal dir");
                let _ = tokio::fs::write(socket_clone, b"socket").await;
            });

            ensure_daemon_running(&socket_path, Some(&config_path))
                .await
                .expect("permission recovery startup");
            assert!(socket_path.exists());
        }
    }

    #[tokio::test]
    async fn daemon_wrapper_and_startup_paths_are_exercised() {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        let socket_path = dir.path().join("giggity.sock");
        std::fs::write(
            &config_path,
            format!(
                "cache_dir = '{}'\nsocket_path = '{}'\nrefresh_seconds = 1\n[sources]\ndocker = false\npodman = false\nnerdctl = false\nhost_listeners = false\nlaunchd = false\nsystemd = false\n",
                dir.path().display(),
                socket_path.display()
            ),
        )
        .expect("config");

        let task = tokio::spawn(run_daemon(Some(config_path.clone())));
        for _ in 0..10 {
            if socket_path.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(socket_path.exists());
        task.abort();

        let delayed_path = dir.path().join("delayed.sock");
        let delayed_clone = delayed_path.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let _ = tokio::fs::write(delayed_clone, b"socket").await;
        });
        ensure_daemon_running(&delayed_path, Some(&config_path))
            .await
            .expect("delayed startup");

        let timeout_path = dir.path().join("timeout.sock");
        let error = ensure_daemon_running(&timeout_path, Some(&config_path))
            .await
            .expect_err("timeout");
        assert!(error.to_string().contains("did not start in time"));
    }

    #[tokio::test]
    async fn daemon_collector_failures_and_reload_errors_are_preserved_in_snapshot() {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        let socket_path = dir.path().join("giggity.sock");
        std::fs::write(
            &config_path,
            format!(
                "cache_dir = '{}'\nsocket_path = '{}'\nrefresh_seconds = 1\n",
                dir.path().display(),
                socket_path.display()
            ),
        )
        .expect("config");
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let task = tokio::spawn(run_daemon_with_collector(
            Some(config_path.clone()),
            Arc::new(ErrorCollector),
            Some(shutdown_rx),
        ));
        tokio::time::sleep(Duration::from_millis(250)).await;
        std::fs::write(&config_path, "not = [valid").expect("break config");
        tokio::time::sleep(Duration::from_millis(1_100)).await;

        let client = DaemonClient::new(socket_path);
        let snapshot = expect_query_response(
            client
                .request(&ClientRequest::Query { view: None })
                .await
                .expect("query"),
        );
        assert!(
            snapshot
                .warnings
                .iter()
                .any(|warning| warning.source == "collector")
        );
        let _ = shutdown_tx.send(());
        let _ = task.await.expect("daemon result");
    }

    #[tokio::test]
    async fn handle_request_supports_ping_validate_and_logs() {
        let config = Config {
            refresh_seconds: 0,
            ..Config::default()
        };
        let store = Arc::new(RwLock::new(Store {
            config,
            snapshot: giggity_core::model::Snapshot {
                resources: vec![host_resource()],
                ..Default::default()
            },
            last_notified: Default::default(),
            muted_until: None,
        }));

        match handle_request(ClientRequest::Ping, store.clone()).await {
            ServerResponse::Pong { api_version } => assert_eq!(api_version, 1),
            other => panic!("unexpected ping response: {other:?}"),
        }

        match handle_request(ClientRequest::ValidateConfig, store.clone()).await {
            ServerResponse::Validation { warnings } => {
                assert!(
                    warnings
                        .iter()
                        .any(|warning| warning.contains("refresh_seconds"))
                );
            }
            other => panic!("unexpected validation response: {other:?}"),
        }

        match handle_request(
            ClientRequest::Logs {
                resource_id: "host:api".into(),
                lines: 25,
            },
            store,
        )
        .await
        {
            ServerResponse::Logs { content } => {
                assert_eq!(content, "logs unavailable for this resource");
            }
            other => panic!("unexpected logs response: {other:?}"),
        }
    }

    #[test]
    fn expand_template_substitutes_resource_fields() {
        let rendered = expand_template("{runtime}:{name}:{project}:{port}:{id}", &host_resource());
        assert_eq!(rendered, "host:api:dev:3000:host:api");
    }

    #[test]
    fn expand_template_covers_all_runtime_tokens() {
        assert!(expand_template("{runtime}", &docker_resource()).contains("docker"));
        assert!(expand_template("{runtime}", &podman_resource()).contains("podman"));
        assert!(expand_template("{runtime}", &nerdctl_resource()).contains("nerdctl"));
        let mut kubernetes = host_resource();
        kubernetes.runtime = RuntimeKind::Kubernetes;
        kubernetes.kind = ResourceKind::KubernetesPod;
        kubernetes.project = Some("dev".into());
        kubernetes.metadata.insert("namespace".into(), "dev".into());
        assert!(expand_template("{runtime}", &kubernetes).contains("kubernetes"));
        assert!(expand_template("{runtime}", &launchd_resource()).contains("launchd"));
        assert!(expand_template("{runtime}", &systemd_resource("system")).contains("systemd"));
    }

    #[test]
    fn current_uid_and_nix_like_id_cover_both_paths() {
        let _guard = TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("lock");
        reset_overrides();
        let dir = tempdir().expect("tempdir");
        let id = write_script(dir.path(), "id", "printf '777\\n'");
        command_overrides()
            .lock()
            .expect("lock")
            .insert("id".into(), id);
        assert_eq!(nix_like_id().expect("nix-like uid"), "777");
        assert!(!current_uid().expect("uid").is_empty());
        reset_overrides();
    }

    #[test]
    fn current_uid_and_nix_like_id_surface_empty_and_failed_id_commands() {
        let _guard = TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("lock");
        reset_overrides();
        let dir = tempdir().expect("tempdir");
        let empty = write_script(dir.path(), "id", "printf ''");
        command_overrides()
            .lock()
            .expect("lock")
            .insert("id".into(), empty);
        assert!(
            nix_like_id()
                .expect_err("empty uid")
                .to_string()
                .contains("empty uid")
        );

        let dir = tempdir().expect("tempdir");
        let failing = write_script(dir.path(), "id", "exit 7");
        command_overrides()
            .lock()
            .expect("lock")
            .insert("id".into(), failing);
        assert!(
            current_uid()
                .expect_err("failed id")
                .to_string()
                .contains("id -u exited")
        );

        let _env = giggity_core::test_support::EnvVarGuard::set("UID", "");
        let dir = tempdir().expect("tempdir");
        let id = write_script(dir.path(), "id", "printf '901\\n'");
        command_overrides()
            .lock()
            .expect("lock")
            .insert("id".into(), id);
        assert_eq!(current_uid().expect("fallback uid"), "901");
        reset_overrides();
    }

    #[tokio::test]
    async fn run_output_and_status_report_success_and_failure() {
        let output = run_output("sh", &["-c", "printf ok"])
            .await
            .expect("stdout");
        assert_eq!(output, "ok");
        run_status("sh", &["-c", "exit 0"])
            .await
            .expect("successful status");

        let output_error = run_output("sh", &["-c", "echo bad >&2; exit 9"])
            .await
            .expect_err("stderr failure");
        assert!(output_error.to_string().contains("sh failed: bad"));

        let status_error = run_status("sh", &["-c", "exit 5"])
            .await
            .expect_err("non-zero status");
        assert!(status_error.to_string().contains("exited with"));
    }

    #[tokio::test]
    async fn probes_cover_command_exit_and_runtime_status_defaulting() {
        let mut resources = vec![ResourceRecord {
            state: HealthState::Healthy,
            runtime_status: None,
            ..host_resource()
        }];
        let mut config = Config::default();
        config.probes.push(ProbeSpec {
            name: "cmd-exit".into(),
            matcher: MatchRule {
                name_regex: Some("^api$".into()),
                ..MatchRule::default()
            },
            kind: ProbeKind::Command {
                program: "sh".into(),
                args: vec!["-c".into(), "exit 4".into()],
                contains: None,
            },
            timeout_millis: 500,
            retries: 0,
            backoff_secs: 2,
            warn_latency_ms: None,
            critical_latency_ms: None,
            interval_secs: 30,
            probe_type: ProbeType::default(),
        });
        apply_probes(&config, &mut resources).await;
        assert_eq!(resources[0].state, HealthState::Degraded);
        assert!(
            resources[0]
                .runtime_status
                .as_deref()
                .unwrap_or("")
                .contains("probe;")
        );
    }

    #[tokio::test]
    async fn dispatch_notifications_skips_within_cooldown() {
        use giggity_core::model::{RecentEvent, Snapshot};
        use std::collections::HashMap;

        let mut config = Config::default();
        config.integrations.cooldown_secs = 60;
        config.notifications.enabled = true;
        config.notifications.on_crash = true;

        let now = Utc::now();
        let event = RecentEvent {
            resource_id: "docker:web".into(),
            resource_name: "web".into(),
            from: Some(HealthState::Healthy),
            to: HealthState::Crashed,
            timestamp: now,
            cause: Some("state transition".into()),
        };

        let snapshot = Snapshot {
            api_version: 1,
            generated_at: now,
            resources: Vec::new(),
            events: vec![event.clone()],
            warnings: Vec::new(),
            last_crash_at: None,
        };

        // First dispatch should record the notification
        let mut store = Store {
            config: config.clone(),
            snapshot: Snapshot::default(),
            muted_until: None,
            last_notified: HashMap::new(),
        };
        dispatch_notifications(&mut store, &snapshot).await;
        assert!(store.last_notified.contains_key("docker:web"));

        // Second dispatch within cooldown should not update the timestamp
        let first_notified = store.last_notified["docker:web"];
        dispatch_notifications(&mut store, &snapshot).await;
        assert_eq!(store.last_notified["docker:web"], first_notified);
    }

    #[tokio::test]
    async fn dispatch_notifications_allows_after_cooldown_expires() {
        use giggity_core::model::{RecentEvent, Snapshot};
        use std::collections::HashMap;

        let mut config = Config::default();
        config.integrations.cooldown_secs = 1;
        config.notifications.enabled = true;
        config.notifications.on_crash = true;

        let now = Utc::now();
        let event = RecentEvent {
            resource_id: "docker:api".into(),
            resource_name: "api".into(),
            from: Some(HealthState::Healthy),
            to: HealthState::Crashed,
            timestamp: now,
            cause: Some("state transition".into()),
        };

        let snapshot = Snapshot {
            api_version: 1,
            generated_at: now,
            resources: Vec::new(),
            events: vec![event],
            warnings: Vec::new(),
            last_crash_at: None,
        };

        // Pre-populate last_notified as if it was notified 2 seconds ago
        let mut last_notified = HashMap::new();
        last_notified.insert("docker:api".into(), now - chrono::Duration::seconds(2));

        let mut store = Store {
            config,
            snapshot: Snapshot::default(),
            muted_until: None,
            last_notified,
        };

        dispatch_notifications(&mut store, &snapshot).await;
        // The timestamp should have been updated since cooldown (1s) has passed
        let updated = store.last_notified["docker:api"];
        assert!(updated > now - chrono::Duration::seconds(1));
    }

    #[tokio::test]
    async fn dispatch_notifications_respects_custom_cooldown_from_config() {
        use giggity_core::model::{RecentEvent, Snapshot};
        use std::collections::HashMap;

        let mut config = Config::default();
        config.integrations.cooldown_secs = 600; // 10 minutes

        let now = Utc::now();
        let event = RecentEvent {
            resource_id: "docker:db".into(),
            resource_name: "db".into(),
            from: Some(HealthState::Healthy),
            to: HealthState::Crashed,
            timestamp: now,
            cause: None,
        };

        let snapshot = Snapshot {
            api_version: 1,
            generated_at: now,
            resources: Vec::new(),
            events: vec![event],
            warnings: Vec::new(),
            last_crash_at: None,
        };

        // Notified 5 minutes ago -- within the 10-minute cooldown
        let mut last_notified = HashMap::new();
        last_notified.insert("docker:db".into(), now - chrono::Duration::seconds(300));

        let mut store = Store {
            config,
            snapshot: Snapshot::default(),
            muted_until: None,
            last_notified,
        };

        let before = store.last_notified["docker:db"];
        dispatch_notifications(&mut store, &snapshot).await;
        // Should NOT have updated because 300s < 600s cooldown
        assert_eq!(store.last_notified["docker:db"], before);
    }

    #[test]
    fn cooldown_secs_defaults_to_300() {
        let config = Config::default();
        assert_eq!(config.integrations.cooldown_secs, 300);
    }

    #[test]
    fn cooldown_secs_is_configurable_via_toml() {
        let config: Config = toml::from_str(
            r#"
[integrations]
cooldown_secs = 120
"#,
        )
        .expect("parse");
        assert_eq!(config.integrations.cooldown_secs, 120);
    }

    #[test]
    fn cooldown_secs_is_configurable_via_tmux_overrides() {
        let mut config = Config::default();
        config.merge_tmux_overrides(&std::collections::BTreeMap::from([(
            "cooldown_secs".into(),
            "60".into(),
        )]));
        assert_eq!(config.integrations.cooldown_secs, 60);
    }
}
