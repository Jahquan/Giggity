use std::io::Write;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use giggity_core::config::{Config, GroupBy};
use giggity_core::model::{RecentEvent, ResourceRecord, Snapshot};
use giggity_core::protocol::{ActionKind, ClientRequest, ServerResponse};
use giggity_core::view::{ResolvedView, render_status_line, resolve_view};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Text};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{Terminal, TerminalOptions, Viewport};

use giggity_daemon::DaemonClient;

trait PopupEvents {
    fn poll(&mut self, timeout: Duration) -> std::io::Result<bool>;
    fn read(&mut self) -> std::io::Result<Event>;
}

#[derive(Clone, Copy)]
struct CrosstermEvents {
    poll_fn: fn(Duration) -> std::io::Result<bool>,
    read_fn: fn() -> std::io::Result<Event>,
}

#[cfg(test)]
impl CrosstermEvents {
    fn with_io(
        poll_fn: fn(Duration) -> std::io::Result<bool>,
        read_fn: fn() -> std::io::Result<Event>,
    ) -> Self {
        Self { poll_fn, read_fn }
    }
}

impl PopupEvents for CrosstermEvents {
    fn poll(&mut self, timeout: Duration) -> std::io::Result<bool> {
        (self.poll_fn)(timeout)
    }

    fn read(&mut self) -> std::io::Result<Event> {
        (self.read_fn)()
    }
}

trait PopupRuntime
where
    Self::Writer: Write,
    <CrosstermBackend<Self::Writer> as ratatui::backend::Backend>::Error:
        std::error::Error + Send + Sync + 'static,
{
    type Writer: Write;
    type Events: PopupEvents;

    fn enter(&mut self) -> anyhow::Result<Terminal<CrosstermBackend<Self::Writer>>>;
    fn events(&mut self) -> Self::Events;
    fn exit(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<Self::Writer>>,
    ) -> anyhow::Result<()>;
}

struct CrosstermRuntime {
    viewport: Viewport,
    enable_raw: fn() -> std::io::Result<()>,
    disable_raw: fn() -> std::io::Result<()>,
    poll_fn: fn(Duration) -> std::io::Result<bool>,
    read_fn: fn() -> std::io::Result<Event>,
}

impl CrosstermRuntime {
    fn real() -> Self {
        Self {
            viewport: Viewport::Fullscreen,
            enable_raw: enable_raw_mode,
            disable_raw: disable_raw_mode,
            poll_fn: event::poll,
            read_fn: event::read,
        }
    }
}

#[cfg(test)]
impl CrosstermRuntime {
    fn with_io(
        viewport: Viewport,
        enable_raw: fn() -> std::io::Result<()>,
        disable_raw: fn() -> std::io::Result<()>,
        poll_fn: fn(Duration) -> std::io::Result<bool>,
        read_fn: fn() -> std::io::Result<Event>,
    ) -> Self {
        Self {
            viewport,
            enable_raw,
            disable_raw,
            poll_fn,
            read_fn,
        }
    }
}

impl PopupRuntime for CrosstermRuntime {
    type Writer = std::io::Stdout;
    type Events = CrosstermEvents;

    fn enter(&mut self) -> anyhow::Result<Terminal<CrosstermBackend<Self::Writer>>> {
        enter_popup_terminal(
            std::io::stdout(),
            self.enable_raw,
            enter_alternate_screen,
            self.viewport.clone(),
        )
    }

    fn events(&mut self) -> Self::Events {
        CrosstermEvents {
            poll_fn: self.poll_fn,
            read_fn: self.read_fn,
        }
    }

    fn exit(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<Self::Writer>>,
    ) -> anyhow::Result<()> {
        exit_popup_terminal(terminal, self.disable_raw, leave_alternate_screen)
    }
}

fn enter_alternate_screen<W: Write>(writer: &mut W) -> std::io::Result<()> {
    execute!(writer, EnterAlternateScreen)?;
    Ok(())
}

fn leave_alternate_screen<W>(terminal: &mut Terminal<CrosstermBackend<W>>) -> anyhow::Result<()>
where
    W: Write,
    <CrosstermBackend<W> as ratatui::backend::Backend>::Error:
        std::error::Error + Send + Sync + 'static,
{
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

#[rustfmt::skip]
fn enter_popup_terminal<W>(
    mut writer: W,
    enable_raw: fn() -> std::io::Result<()>,
    enter_alt: fn(&mut W) -> std::io::Result<()>,
    viewport: Viewport,
) -> anyhow::Result<Terminal<CrosstermBackend<W>>>
where
    W: Write,
    <CrosstermBackend<W> as ratatui::backend::Backend>::Error:
        std::error::Error + Send + Sync + 'static,
{
    enable_raw()?;
    enter_alt(&mut writer)?;
    Ok(Terminal::with_options(CrosstermBackend::new(writer), TerminalOptions { viewport })?)
}

fn exit_popup_terminal<W>(
    terminal: &mut Terminal<CrosstermBackend<W>>,
    disable_raw: fn() -> std::io::Result<()>,
    leave_alt: fn(&mut Terminal<CrosstermBackend<W>>) -> anyhow::Result<()>,
) -> anyhow::Result<()>
where
    W: Write,
    <CrosstermBackend<W> as ratatui::backend::Backend>::Error:
        std::error::Error + Send + Sync + 'static,
{
    disable_raw()?;
    leave_alt(terminal)
}

async fn run_popup_with_runtime(
    client: DaemonClient,
    config: Config,
    view_name: Option<String>,
) -> anyhow::Result<()> {
    let mut runtime = CrosstermRuntime::real();
    run_popup_with(client, config, view_name, &mut runtime).await
}

async fn run_popup_with<R>(
    client: DaemonClient,
    config: Config,
    view_name: Option<String>,
    runtime: &mut R,
) -> anyhow::Result<()>
where
    R: PopupRuntime,
{
    let mut terminal = runtime.enter()?;
    let mut app = PopupApp::new(client, config, view_name);
    let mut events = runtime.events();
    let result = app.run(&mut terminal, &mut events).await;
    runtime.exit(&mut terminal)?;
    result
}

pub async fn run_popup(
    client: DaemonClient,
    config: Config,
    view_name: Option<String>,
) -> anyhow::Result<()> {
    run_popup_with_runtime(client, config, view_name).await
}

struct PopupApp {
    client: DaemonClient,
    config: Config,
    view_name: String,
    local_grouping: Option<GroupBy>,
    snapshot: Snapshot,
    resolved: Option<ResolvedView>,
    selected: usize,
    filter_input: String,
    filter_mode: bool,
    show_logs: bool,
    message: String,
    confirm: Option<ActionKind>,
    logs: String,
    last_refresh: Instant,
    last_log_target: Option<String>,
}

impl PopupApp {
    fn new(client: DaemonClient, config: Config, view_name: Option<String>) -> Self {
        Self {
            client,
            view_name: view_name.unwrap_or_else(|| config.default_view.clone()),
            config,
            local_grouping: None,
            snapshot: Snapshot::default(),
            resolved: None,
            selected: 0,
            filter_input: String::new(),
            filter_mode: false,
            show_logs: false,
            message: "loading...".into(),
            confirm: None,
            logs: String::new(),
            last_refresh: Instant::now() - Duration::from_secs(10),
            last_log_target: None,
        }
    }

    async fn run<B, E>(&mut self, terminal: &mut Terminal<B>, events: &mut E) -> anyhow::Result<()>
    where
        B: ratatui::backend::Backend,
        B::Error: std::error::Error + Send + Sync + 'static,
        E: PopupEvents,
    {
        loop {
            if self.last_refresh.elapsed()
                >= Duration::from_secs(self.config.refresh_seconds.max(1))
            {
                self.refresh().await;
            }

            terminal.draw(|frame| self.draw(frame))?;

            if events.poll(Duration::from_millis(100))? {
                let Event::Key(key) = events.read()? else {
                    continue;
                };
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                if self.handle_key(key.code).await? {
                    break;
                }
            }
        }
        Ok(())
    }

    async fn refresh(&mut self) {
        self.last_refresh = Instant::now();
        match self
            .client
            .request(&ClientRequest::Query {
                view: Some(self.view_name.clone()),
            })
            .await
        {
            Ok(ServerResponse::Query { snapshot }) => {
                self.snapshot = snapshot;
                let mut config = self.config.clone();
                if let Some(grouping) = &self.local_grouping {
                    let mut view = config.active_view(Some(&self.view_name));
                    view.grouping = grouping.clone();
                    config.views.insert(self.view_name.clone(), view);
                }
                let mut resolved = resolve_view(&config, Some(&self.view_name), &self.snapshot);
                if !self.filter_input.is_empty() {
                    let filter = self.filter_input.to_lowercase();
                    resolved.resources.retain(|resource| {
                        resource.name.to_lowercase().contains(&filter)
                            || resource.id.to_lowercase().contains(&filter)
                            || resource
                                .project
                                .as_ref()
                                .map(|project| project.to_lowercase().contains(&filter))
                                .unwrap_or(false)
                    });
                }
                if self.selected >= resolved.resources.len() {
                    self.selected = resolved.resources.len().saturating_sub(1);
                }
                self.message = render_status_line(&resolved);
                self.resolved = Some(resolved);
                let _ = self.refresh_logs().await;
            }
            Ok(response) => self.message = format!("unexpected response: {response:?}"),
            Err(error) => self.message = error.to_string(),
        }
    }

    async fn refresh_logs(&mut self) -> anyhow::Result<()> {
        if !self.show_logs {
            return Ok(());
        }
        let Some((resource_id, resource_name)) = self
            .selected_resource()
            .map(|resource| (resource.id.clone(), resource.name.clone()))
        else {
            return Ok(());
        };
        if self.last_log_target.as_deref() == Some(resource_id.as_str()) && !self.logs.is_empty() {
            return Ok(());
        }
        match self
            .client
            .request(&ClientRequest::Logs {
                resource_id: resource_id.clone(),
                lines: 50,
            })
            .await?
        {
            ServerResponse::Logs { content } => {
                self.logs = content;
                self.last_log_target = Some(resource_id);
            }
            ServerResponse::Error { message } => {
                self.logs = format!("{resource_name}: {message}");
                self.last_log_target = Some(resource_id);
            }
            _ => {}
        }
        Ok(())
    }

    async fn handle_key(&mut self, code: KeyCode) -> anyhow::Result<bool> {
        if let Some(action) = self.confirm.take() {
            return match code {
                KeyCode::Char('y') => {
                    self.run_action(action, true).await?;
                    Ok(false)
                }
                KeyCode::Char('n') | KeyCode::Esc => {
                    self.message = "action cancelled".into();
                    Ok(false)
                }
                _ => {
                    self.confirm = Some(action);
                    Ok(false)
                }
            };
        }

        if self.filter_mode {
            match code {
                KeyCode::Esc => self.filter_mode = false,
                KeyCode::Enter => self.filter_mode = false,
                KeyCode::Backspace => {
                    self.filter_input.pop();
                }
                KeyCode::Char(ch) => self.filter_input.push(ch),
                _ => {}
            }
            self.last_refresh = Instant::now() - Duration::from_secs(10);
            return Ok(false);
        }

        match code {
            KeyCode::Char('q') | KeyCode::Esc => Ok(true),
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(resolved) = &self.resolved {
                    self.selected =
                        (self.selected + 1).min(resolved.resources.len().saturating_sub(1));
                }
                self.last_log_target = None;
                Ok(false)
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                self.last_log_target = None;
                Ok(false)
            }
            KeyCode::Char('/') => {
                self.filter_mode = true;
                Ok(false)
            }
            KeyCode::Char('v') => {
                let names: Vec<_> = self.config.views.keys().cloned().collect();
                if let Some(position) = names.iter().position(|name| name == &self.view_name) {
                    self.view_name = names[(position + 1) % names.len()].clone();
                }
                self.last_refresh = Instant::now() - Duration::from_secs(10);
                Ok(false)
            }
            KeyCode::Char('g') => {
                self.local_grouping = Some(
                    match self.local_grouping.clone().unwrap_or(GroupBy::Severity) {
                        GroupBy::Severity => GroupBy::Runtime,
                        GroupBy::Runtime => GroupBy::Project,
                        GroupBy::Project => GroupBy::None,
                        GroupBy::ComposeStack => GroupBy::UnitDomain,
                        GroupBy::UnitDomain => GroupBy::Severity,
                        GroupBy::None => GroupBy::Severity,
                    },
                );
                self.last_refresh = Instant::now() - Duration::from_secs(10);
                Ok(false)
            }
            KeyCode::Char('l') => {
                self.show_logs = !self.show_logs;
                self.last_log_target = None;
                Ok(false)
            }
            KeyCode::Char('r') => {
                self.confirm = Some(ActionKind::Restart);
                self.message = "confirm restart? press y/n".into();
                Ok(false)
            }
            KeyCode::Char('s') => {
                self.confirm = Some(ActionKind::Stop);
                self.message = "confirm stop? press y/n".into();
                Ok(false)
            }
            KeyCode::Char('o') => {
                self.run_action(ActionKind::OpenUrl, false).await?;
                Ok(false)
            }
            KeyCode::Char('c') => {
                self.run_action(ActionKind::CopyPort, false).await?;
                Ok(false)
            }
            KeyCode::Enter => {
                self.show_logs = !self.show_logs;
                self.last_log_target = None;
                Ok(false)
            }
            _ => Ok(false),
        }
    }

    async fn run_action(&mut self, action: ActionKind, confirm: bool) -> anyhow::Result<()> {
        let Some(resource) = self.selected_resource() else {
            self.message = "no resource selected".into();
            return Ok(());
        };
        match self
            .client
            .request(&ClientRequest::Action {
                action,
                resource_id: resource.id.clone(),
                confirm,
            })
            .await?
        {
            ServerResponse::ActionResult { message } => self.message = message,
            ServerResponse::Error { message } => self.message = message,
            _ => self.message = "unexpected action response".into(),
        }
        self.last_refresh = Instant::now() - Duration::from_secs(10);
        Ok(())
    }

    fn selected_resource(&self) -> Option<&ResourceRecord> {
        self.resolved.as_ref()?.resources.get(self.selected)
    }

    fn draw(&self, frame: &mut ratatui::Frame<'_>) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(10),
                Constraint::Length(2),
            ])
            .split(frame.area());
        let body = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
            .split(chunks[1]);

        let header = Paragraph::new(Text::from(vec![
            Line::from(format!(
                "view={} filter={} group={}",
                self.view_name,
                if self.filter_input.is_empty() {
                    "<none>"
                } else {
                    &self.filter_input
                },
                self.local_grouping
                    .as_ref()
                    .map(|grouping| format!("{grouping:?}"))
                    .unwrap_or_else(|| "default".into())
            )),
            Line::from(self.message.clone()),
        ]))
        .block(Block::default().borders(Borders::ALL).title("Giggity"));
        frame.render_widget(header, chunks[0]);

        let resources = self
            .resolved
            .as_ref()
            .map(|resolved| &resolved.resources)
            .cloned()
            .unwrap_or_default();
        let items: Vec<ListItem<'_>> = resources
            .iter()
            .map(|resource| {
                ListItem::new(Line::from(format!(
                    "{:<24} {:<10} {:<10} {}",
                    resource.name,
                    resource.state,
                    resource.runtime,
                    resource
                        .ports
                        .iter()
                        .map(|port| port.host_port.to_string())
                        .collect::<Vec<_>>()
                        .join(",")
                )))
            })
            .collect();
        let mut state = ListState::default();
        state.select(if items.is_empty() {
            None
        } else {
            Some(self.selected)
        });
        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title("Resources"))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        frame.render_stateful_widget(list, body[0], &mut state);

        let detail_text = if self.show_logs {
            self.logs.clone()
        } else {
            self.render_details()
        };
        let title = if self.show_logs { "Logs" } else { "Details" };
        let detail = Paragraph::new(detail_text)
            .block(Block::default().borders(Borders::ALL).title(title))
            .wrap(Wrap { trim: false });
        frame.render_widget(detail, body[1]);

        let footer = Paragraph::new(
            "q quit  / filter  v next-view  g regroup  l logs  r restart  s stop  o open-url  c copy-port",
        )
        .block(Block::default().borders(Borders::ALL));
        frame.render_widget(footer, chunks[2]);
    }

    fn render_details(&self) -> String {
        let Some(resource) = self.selected_resource() else {
            return "no resource selected".into();
        };
        let mut lines = vec![
            format!("name: {}", resource.name),
            format!("id: {}", resource.id),
            format!("state: {}", resource.state),
            format!("runtime: {}", resource.runtime),
        ];
        if let Some(project) = &resource.project {
            lines.push(format!("project: {project}"));
        }
        if !resource.ports.is_empty() {
            lines.push(format!(
                "ports: {}",
                resource
                    .ports
                    .iter()
                    .map(|port| port.host_port.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !resource.urls.is_empty() {
            lines.push(format!(
                "urls: {}",
                resource
                    .urls
                    .iter()
                    .map(|url| url.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        for (key, value) in &resource.metadata {
            lines.push(format!("{key}: {value}"));
        }
        lines.push(String::new());
        lines.push("recent events:".into());
        lines.extend(
            self.snapshot
                .events
                .iter()
                .filter(|event| event.resource_id == resource.id)
                .take(5)
                .map(format_event),
        );
        lines.join("\n")
    }
}

fn format_event(event: &RecentEvent) -> String {
    format!(
        "- {} -> {} at {}{}",
        event
            .from
            .map(|from| from.to_string())
            .unwrap_or_else(|| "new".into()),
        event.to,
        event.timestamp.format("%H:%M:%S"),
        event
            .cause
            .as_ref()
            .map(|cause| format!(" ({cause})"))
            .unwrap_or_default()
    )
}

#[cfg(test)]
#[allow(clippy::await_holding_lock)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::Duration;

    use async_trait::async_trait;
    use chrono::{TimeZone, Utc};
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
    use giggity_collectors::{CollectionOutput, CollectorProvider};
    use giggity_core::config::{Config, ViewConfig};
    use giggity_core::model::{
        HealthState, PortBinding, ResourceKind, ResourceRecord, RuntimeKind, Snapshot,
    };
    use giggity_core::protocol::{ClientRequest, ServerResponse};
    use giggity_core::view::resolve_view;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::{Terminal, Viewport};
    use tempfile::tempdir;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;
    use tokio::sync::oneshot;

    use giggity_core::protocol::ActionKind;
    use giggity_daemon::{DaemonClient, run_daemon_with_collector};

    use super::{
        CrosstermEvents, CrosstermRuntime, PopupApp, PopupEvents, enter_alternate_screen,
        enter_popup_terminal, exit_popup_terminal, format_event, leave_alternate_screen,
        run_popup_with,
    };

    static RAW_MODE_COUNTS: OnceLock<Mutex<(usize, usize)>> = OnceLock::new();
    static CROSSTERM_EVENT_QUEUE: OnceLock<Mutex<std::collections::VecDeque<Event>>> =
        OnceLock::new();
    static TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    #[derive(Debug)]
    struct FakeCollector;

    #[async_trait]
    impl CollectorProvider for FakeCollector {
        async fn collect(&self, _config: &Config) -> anyhow::Result<CollectionOutput> {
            Ok(CollectionOutput {
                resources: vec![ResourceRecord {
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
                    metadata: BTreeMap::new(),
                    last_changed: Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap(),
                }],
                warnings: Vec::new(),
            })
        }
    }

    async fn spawn_popup_app() -> (PopupApp, oneshot::Sender<()>, tempfile::TempDir) {
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
        let config = Config::load_from(&config_path).expect("config load");
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        tokio::spawn(run_daemon_with_collector(
            Some(config_path.clone()),
            Arc::new(FakeCollector),
            Some(shutdown_rx),
        ));
        tokio::time::sleep(Duration::from_millis(250)).await;
        let client = DaemonClient::new(dir.path().join("giggity.sock"));
        let mut config = config;
        config.views.insert("ops".into(), ViewConfig::default());
        (
            PopupApp::new(client, config, Some("default".into())),
            shutdown_tx,
            dir,
        )
    }

    struct FakeEvents {
        queue: std::collections::VecDeque<Event>,
    }

    impl FakeEvents {
        fn new(events: impl IntoIterator<Item = Event>) -> Self {
            Self {
                queue: events.into_iter().collect(),
            }
        }
    }

    struct PausedEvents {
        paused_once: bool,
        inner: FakeEvents,
    }

    impl PausedEvents {
        fn new(events: impl IntoIterator<Item = Event>) -> Self {
            Self {
                paused_once: false,
                inner: FakeEvents::new(events),
            }
        }
    }

    impl PopupEvents for FakeEvents {
        fn poll(&mut self, _timeout: Duration) -> std::io::Result<bool> {
            Ok(!self.queue.is_empty())
        }

        fn read(&mut self) -> std::io::Result<Event> {
            self.queue.pop_front().ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "no more events")
            })
        }
    }

    impl PopupEvents for PausedEvents {
        fn poll(&mut self, timeout: Duration) -> std::io::Result<bool> {
            if !self.paused_once {
                self.paused_once = true;
                return Ok(false);
            }
            self.inner.poll(timeout)
        }

        fn read(&mut self) -> std::io::Result<Event> {
            self.inner.read()
        }
    }

    fn press(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn release(code: KeyCode) -> Event {
        Event::Key(KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Release,
            state: KeyEventState::NONE,
        })
    }

    fn fake_enable_raw_mode() -> std::io::Result<()> {
        RAW_MODE_COUNTS
            .get_or_init(|| Mutex::new((0, 0)))
            .lock()
            .expect("lock")
            .0 += 1;
        Ok(())
    }

    fn fake_disable_raw_mode() -> std::io::Result<()> {
        RAW_MODE_COUNTS
            .get_or_init(|| Mutex::new((0, 0)))
            .lock()
            .expect("lock")
            .1 += 1;
        Ok(())
    }

    fn push_crossterm_events(events: impl IntoIterator<Item = Event>) {
        let mut queue = CROSSTERM_EVENT_QUEUE
            .get_or_init(|| Mutex::new(std::collections::VecDeque::new()))
            .lock()
            .expect("lock");
        queue.clear();
        queue.extend(events);
    }

    fn fake_crossterm_poll(_timeout: Duration) -> std::io::Result<bool> {
        Ok(!CROSSTERM_EVENT_QUEUE
            .get_or_init(|| Mutex::new(std::collections::VecDeque::new()))
            .lock()
            .expect("lock")
            .is_empty())
    }

    fn fake_crossterm_read() -> std::io::Result<Event> {
        CROSSTERM_EVENT_QUEUE
            .get_or_init(|| Mutex::new(std::collections::VecDeque::new()))
            .lock()
            .expect("lock")
            .pop_front()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "no event"))
    }

    async fn spawn_scripted_server(
        responses: Vec<ServerResponse>,
    ) -> (DaemonClient, tokio::task::JoinHandle<()>, tempfile::TempDir) {
        let dir = tempdir().expect("tempdir");
        let socket_path = dir.path().join("giggity.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind");
        let task = tokio::spawn(async move {
            for response in responses {
                let (stream, _) = listener.accept().await.expect("accept");
                let (reader, mut writer) = stream.into_split();
                let mut reader = BufReader::new(reader);
                let mut line = String::new();
                reader.read_line(&mut line).await.expect("read");
                let _request: ClientRequest = serde_json::from_str(line.trim()).expect("request");
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
        (DaemonClient::new(socket_path), task, dir)
    }

    #[tokio::test]
    async fn popup_refreshes_and_filters_resources() {
        let (mut app, shutdown_tx, _dir) = spawn_popup_app().await;
        app.refresh().await;
        assert_eq!(app.resolved.as_ref().expect("resolved").resources.len(), 1);

        app.handle_key(crossterm::event::KeyCode::Char('/'))
            .await
            .expect("enter filter");
        app.handle_key(crossterm::event::KeyCode::Char('z'))
            .await
            .expect("type");
        app.handle_key(crossterm::event::KeyCode::Enter)
            .await
            .expect("leave filter");
        app.refresh().await;
        assert_eq!(app.resolved.as_ref().expect("resolved").resources.len(), 0);

        let _ = shutdown_tx.send(());
    }

    #[tokio::test]
    async fn popup_refresh_filters_by_id_and_project_and_rebounds_selection() {
        let (mut app, shutdown_tx, _dir) = spawn_popup_app().await;
        app.selected = 9;
        app.filter_input = "host:api".into();
        app.refresh().await;
        assert_eq!(app.resolved.as_ref().expect("resolved").resources.len(), 1);
        assert_eq!(app.selected, 0);

        app.filter_input = "dev".into();
        app.refresh().await;
        assert_eq!(app.resolved.as_ref().expect("resolved").resources.len(), 1);

        let _ = shutdown_tx.send(());
    }

    #[tokio::test]
    async fn popup_run_loop_handles_refresh_and_quit_events() {
        let dir = tempdir().expect("tempdir");
        let client = DaemonClient::new(dir.path().join("missing.sock"));
        let mut app = PopupApp::new(client, Config::default(), None);
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut events = FakeEvents::new([
            Event::Resize(80, 20),
            release(KeyCode::Char('q')),
            press(KeyCode::Char('q')),
        ]);

        app.run(&mut terminal, &mut events).await.expect("run loop");
        assert!(!app.message.is_empty());
    }

    #[tokio::test]
    async fn popup_run_loop_continues_after_non_quit_keys() {
        let dir = tempdir().expect("tempdir");
        let client = DaemonClient::new(dir.path().join("missing.sock"));
        let mut app = PopupApp::new(client, Config::default(), None);
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut events = FakeEvents::new([
            press(KeyCode::Char('/')),
            press(KeyCode::Esc),
            press(KeyCode::Char('q')),
        ]);

        app.run(&mut terminal, &mut events).await.expect("run loop");
        assert!(!app.filter_mode);
    }

    #[tokio::test]
    async fn popup_run_loop_handles_empty_poll_before_key_input() {
        let dir = tempdir().expect("tempdir");
        let client = DaemonClient::new(dir.path().join("missing.sock"));
        let mut app = PopupApp::new(client, Config::default(), None);
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut events = PausedEvents::new([press(KeyCode::Char('q'))]);

        app.run(&mut terminal, &mut events).await.expect("run loop");
        assert!(!app.message.is_empty());
    }

    #[test]
    fn crossterm_events_delegate_to_injected_io() {
        let _guard = TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("lock");
        push_crossterm_events([press(KeyCode::Char('q'))]);
        let mut events = CrosstermEvents::with_io(fake_crossterm_poll, fake_crossterm_read);
        assert!(events.poll(Duration::from_millis(1)).expect("poll"));
        assert!(matches!(
            events.read().expect("read"),
            Event::Key(key) if key.code == KeyCode::Char('q')
        ));
    }

    #[test]
    fn popup_terminal_helpers_execute_setup_and_teardown() {
        let _guard = TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("lock");
        *RAW_MODE_COUNTS
            .get_or_init(|| Mutex::new((0, 0)))
            .lock()
            .expect("lock") = (0, 0);
        let mut terminal = enter_popup_terminal(
            Vec::new(),
            fake_enable_raw_mode,
            enter_alternate_screen,
            Viewport::Fixed(Rect::new(0, 0, 80, 20)),
        )
        .expect("enter");
        exit_popup_terminal(&mut terminal, fake_disable_raw_mode, leave_alternate_screen)
            .expect("exit");
        assert_eq!(
            *RAW_MODE_COUNTS
                .get_or_init(|| Mutex::new((0, 0)))
                .lock()
                .expect("lock"),
            (1, 1)
        );
    }

    #[tokio::test]
    async fn popup_runtime_helper_enters_runs_and_exits() {
        let _guard = TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("lock");
        let dir = tempdir().expect("tempdir");
        let client = DaemonClient::new(dir.path().join("missing.sock"));
        *RAW_MODE_COUNTS
            .get_or_init(|| Mutex::new((0, 0)))
            .lock()
            .expect("lock") = (0, 0);
        let mut runtime = CrosstermRuntime::with_io(
            Viewport::Fixed(Rect::new(0, 0, 80, 20)),
            fake_enable_raw_mode,
            fake_disable_raw_mode,
            fake_crossterm_poll,
            fake_crossterm_read,
        );
        push_crossterm_events([press(KeyCode::Char('q'))]);

        run_popup_with(client, Config::default(), None, &mut runtime)
            .await
            .expect("popup runtime");

        assert_eq!(
            *RAW_MODE_COUNTS
                .get_or_init(|| Mutex::new((0, 0)))
                .lock()
                .expect("lock"),
            (1, 1)
        );
    }

    #[tokio::test]
    async fn popup_cycles_view_and_grouping() {
        let (mut app, shutdown_tx, _dir) = spawn_popup_app().await;
        app.refresh().await;
        app.handle_key(crossterm::event::KeyCode::Char('v'))
            .await
            .expect("cycle view");
        assert_eq!(app.view_name, "ops");
        app.handle_key(crossterm::event::KeyCode::Char('g'))
            .await
            .expect("cycle group");
        assert!(app.local_grouping.is_some());

        let _ = shutdown_tx.send(());
    }

    #[tokio::test]
    async fn popup_navigation_without_resolved_state_and_unknown_view_are_safe() {
        let dir = tempdir().expect("tempdir");
        let client = DaemonClient::new(dir.path().join("missing.sock"));
        let mut config = Config::default();
        config.views.clear();
        let mut app = PopupApp::new(client, config, Some("missing".into()));

        assert!(!app.handle_key(KeyCode::Down).await.expect("down"));
        assert!(!app.handle_key(KeyCode::Char('v')).await.expect("cycle"));
        assert_eq!(app.view_name, "missing");
    }

    #[tokio::test]
    async fn popup_grouping_cycles_through_compose_stack_and_unit_domain() {
        let (mut app, shutdown_tx, _dir) = spawn_popup_app().await;
        app.local_grouping = Some(giggity_core::config::GroupBy::ComposeStack);
        app.handle_key(crossterm::event::KeyCode::Char('g'))
            .await
            .expect("cycle compose stack");
        assert!(matches!(
            app.local_grouping,
            Some(giggity_core::config::GroupBy::UnitDomain)
        ));
        app.handle_key(crossterm::event::KeyCode::Char('g'))
            .await
            .expect("cycle unit domain");
        assert!(matches!(
            app.local_grouping,
            Some(giggity_core::config::GroupBy::Severity)
        ));

        let _ = shutdown_tx.send(());
    }

    #[tokio::test]
    async fn popup_refresh_applies_local_grouping_and_handles_unexpected_response() {
        let (mut app, shutdown_tx, _dir) = spawn_popup_app().await;
        app.local_grouping = Some(giggity_core::config::GroupBy::Runtime);
        app.refresh().await;
        assert_eq!(
            app.resolved.as_ref().expect("resolved").grouped[0].label,
            "host"
        );
        let _ = shutdown_tx.send(());

        let (client, server, _dir) = spawn_scripted_server(vec![ServerResponse::Validation {
            warnings: Vec::new(),
        }])
        .await;
        let mut app = PopupApp::new(client, Config::default(), None);
        app.refresh().await;
        assert!(app.message.contains("unexpected response"));
        server.await.expect("server");
    }

    #[test]
    fn fake_events_read_reports_unexpected_eof_when_empty() {
        let mut events = FakeEvents::new([]);
        let error = events.read().expect_err("empty queue");
        assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    #[tokio::test]
    async fn popup_filter_mode_supports_backspace_and_escape() {
        let (mut app, shutdown_tx, _dir) = spawn_popup_app().await;
        app.handle_key(crossterm::event::KeyCode::Char('/'))
            .await
            .expect("enter filter");
        app.handle_key(crossterm::event::KeyCode::Char('z'))
            .await
            .expect("type");
        app.handle_key(crossterm::event::KeyCode::Backspace)
            .await
            .expect("backspace");
        app.handle_key(crossterm::event::KeyCode::Esc)
            .await
            .expect("escape");

        assert!(app.filter_input.is_empty());
        assert!(!app.filter_mode);

        let _ = shutdown_tx.send(());
    }

    #[tokio::test]
    async fn popup_filter_mode_ignores_unhandled_keys() {
        let (mut app, shutdown_tx, _dir) = spawn_popup_app().await;
        app.handle_key(crossterm::event::KeyCode::Char('/'))
            .await
            .expect("enter filter");
        app.handle_key(crossterm::event::KeyCode::Up)
            .await
            .expect("ignore key");

        assert!(app.filter_mode);
        assert!(app.filter_input.is_empty());

        let _ = shutdown_tx.send(());
    }

    #[tokio::test]
    async fn popup_confirmation_can_be_cancelled() {
        let (mut app, shutdown_tx, _dir) = spawn_popup_app().await;
        app.refresh().await;
        app.handle_key(crossterm::event::KeyCode::Char('r'))
            .await
            .expect("request restart");
        assert!(app.confirm.is_some());

        app.handle_key(crossterm::event::KeyCode::Char('n'))
            .await
            .expect("cancel");
        assert!(app.confirm.is_none());
        assert_eq!(app.message, "action cancelled");

        let _ = shutdown_tx.send(());
    }

    #[tokio::test]
    async fn popup_confirmation_yes_and_other_key_paths_are_covered() {
        let (client, server, _dir) = spawn_scripted_server(vec![ServerResponse::ActionResult {
            message: "restarted api".into(),
        }])
        .await;
        let mut app = PopupApp::new(client, Config::default(), None);
        app.snapshot = Snapshot {
            resources: vec![ResourceRecord {
                metadata: BTreeMap::from([("pid".into(), "123".into())]),
                urls: vec!["http://127.0.0.1:3000".parse().expect("url")],
                ..resource_record()
            }],
            ..Snapshot::default()
        };
        app.resolved = Some(resolve_view(&app.config, Some("default"), &app.snapshot));
        app.confirm = Some(ActionKind::Restart);
        assert!(
            !app.handle_key(KeyCode::Char('x'))
                .await
                .expect("hold confirm")
        );
        assert!(app.confirm.is_some());
        assert!(
            !app.handle_key(KeyCode::Char('y'))
                .await
                .expect("confirm yes")
        );
        assert_eq!(app.message, "restarted api");
        server.await.expect("server");
    }

    #[tokio::test]
    async fn popup_log_toggle_fetches_log_content() {
        let (mut app, shutdown_tx, _dir) = spawn_popup_app().await;
        app.refresh().await;
        app.handle_key(crossterm::event::KeyCode::Char('l'))
            .await
            .expect("toggle logs");
        app.refresh_logs().await.expect("refresh logs");

        assert!(app.show_logs);
        assert_eq!(app.logs, "logs unavailable for this resource");

        let _ = shutdown_tx.send(());
    }

    #[tokio::test]
    async fn popup_refresh_logs_covers_empty_cached_error_and_unexpected_paths() {
        let dir = tempdir().expect("tempdir");
        let mut app = PopupApp::new(
            DaemonClient::new(dir.path().join("missing.sock")),
            Config::default(),
            None,
        );
        app.show_logs = true;
        app.refresh_logs().await.expect("no selection");

        app.snapshot = Snapshot {
            resources: vec![resource_record()],
            ..Snapshot::default()
        };
        app.resolved = Some(resolve_view(&app.config, Some("default"), &app.snapshot));
        app.logs = "cached".into();
        app.last_log_target = Some("host:api".into());
        app.refresh_logs().await.expect("cached");
        assert_eq!(app.logs, "cached");

        let (client, server, _dir) = spawn_scripted_server(vec![
            ServerResponse::Error {
                message: "missing logs".into(),
            },
            ServerResponse::Validation {
                warnings: Vec::new(),
            },
        ])
        .await;
        let mut app = PopupApp::new(client, Config::default(), None);
        app.show_logs = true;
        app.snapshot = Snapshot {
            resources: vec![resource_record()],
            ..Snapshot::default()
        };
        app.resolved = Some(resolve_view(&app.config, Some("default"), &app.snapshot));
        app.refresh_logs().await.expect("error response");
        assert!(app.logs.contains("missing logs"));
        app.last_log_target = None;
        app.refresh_logs().await.expect("unexpected response");
        assert!(app.logs.contains("missing logs"));
        server.await.expect("server");
    }

    #[tokio::test]
    async fn popup_render_details_includes_events_and_urls() {
        let (mut app, shutdown_tx, _dir) = spawn_popup_app().await;
        app.refresh().await;
        let details = app.render_details();
        assert!(details.contains("name: api"));
        assert!(details.contains("urls: http://127.0.0.1:3000"));
        assert!(details.contains("recent events:"));

        let _ = shutdown_tx.send(());
    }

    #[tokio::test]
    async fn popup_handle_key_covers_navigation_actions_and_defaults() {
        let (client, server, _dir) = spawn_scripted_server(vec![
            ServerResponse::ActionResult {
                message: "opened".into(),
            },
            ServerResponse::ActionResult {
                message: "copied".into(),
            },
        ])
        .await;
        let mut app = PopupApp::new(client, Config::default(), None);
        app.snapshot = Snapshot {
            resources: vec![
                resource_record(),
                ResourceRecord {
                    id: "host:web".into(),
                    name: "web".into(),
                    ports: vec![PortBinding {
                        host_ip: None,
                        host_port: 4000,
                        container_port: None,
                        protocol: "tcp".into(),
                    }],
                    urls: vec!["http://127.0.0.1:4000".parse().expect("url")],
                    ..resource_record()
                },
            ],
            ..Snapshot::default()
        };
        app.resolved = Some(resolve_view(&app.config, Some("default"), &app.snapshot));
        app.last_log_target = Some("host:api".into());

        assert!(!app.handle_key(KeyCode::Down).await.expect("down"));
        assert_eq!(app.selected, 1);
        assert!(app.last_log_target.is_none());
        assert!(!app.handle_key(KeyCode::Up).await.expect("up"));
        assert_eq!(app.selected, 0);
        assert!(
            !app.handle_key(KeyCode::Char('s'))
                .await
                .expect("stop confirm")
        );
        assert!(matches!(app.confirm, Some(ActionKind::Stop)));
        app.confirm = None;
        assert!(!app.handle_key(KeyCode::Char('o')).await.expect("open"));
        assert_eq!(app.message, "opened");
        assert!(!app.handle_key(KeyCode::Char('c')).await.expect("copy"));
        assert_eq!(app.message, "copied");
        assert!(!app.handle_key(KeyCode::Enter).await.expect("enter"));
        assert!(app.show_logs);
        assert!(!app.handle_key(KeyCode::Tab).await.expect("other"));
        assert!(app.handle_key(KeyCode::Esc).await.expect("quit"));
        server.await.expect("server");
    }

    #[tokio::test]
    async fn popup_grouping_cycles_through_all_variants() {
        let dir = tempdir().expect("tempdir");
        let client = DaemonClient::new(dir.path().join("missing.sock"));
        let mut app = PopupApp::new(client, Config::default(), None);
        app.local_grouping = Some(giggity_core::config::GroupBy::Runtime);
        app.handle_key(KeyCode::Char('g'))
            .await
            .expect("runtime->project");
        assert_eq!(
            app.local_grouping,
            Some(giggity_core::config::GroupBy::Project)
        );
        app.handle_key(KeyCode::Char('g'))
            .await
            .expect("project->none");
        assert_eq!(
            app.local_grouping,
            Some(giggity_core::config::GroupBy::None)
        );
        app.handle_key(KeyCode::Char('g'))
            .await
            .expect("none->severity");
        assert_eq!(
            app.local_grouping,
            Some(giggity_core::config::GroupBy::Severity)
        );
    }

    #[tokio::test]
    async fn popup_run_action_with_no_selection_sets_message() {
        let dir = tempdir().expect("tempdir");
        let client = DaemonClient::new(dir.path().join("missing.sock"));
        let mut app = PopupApp::new(client, Config::default(), None);
        app.run_action(ActionKind::CopyPort, false)
            .await
            .expect("action without selection");
        assert_eq!(app.message, "no resource selected");
    }

    #[tokio::test]
    async fn popup_run_action_handles_error_and_unexpected_responses() {
        let (client, server, _dir) = spawn_scripted_server(vec![
            ServerResponse::Error {
                message: "denied".into(),
            },
            ServerResponse::Validation {
                warnings: Vec::new(),
            },
        ])
        .await;
        let mut app = PopupApp::new(client, Config::default(), None);
        app.snapshot = Snapshot {
            resources: vec![resource_record()],
            ..Snapshot::default()
        };
        app.resolved = Some(resolve_view(&app.config, Some("default"), &app.snapshot));

        app.run_action(ActionKind::Restart, true)
            .await
            .expect("error response");
        assert_eq!(app.message, "denied");
        app.run_action(ActionKind::Restart, true)
            .await
            .expect("unexpected response");
        assert_eq!(app.message, "unexpected action response");
        server.await.expect("server");
    }

    #[test]
    fn popup_draw_renders_screen_sections() {
        let dir = tempdir().expect("tempdir");
        let client = DaemonClient::new(dir.path().join("missing.sock"));
        let mut app = PopupApp::new(client, Config::default(), None);
        app.snapshot = Snapshot {
            resources: vec![ResourceRecord {
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
                metadata: BTreeMap::new(),
                last_changed: Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap(),
            }],
            ..Snapshot::default()
        };
        app.resolved = Some(resolve_view(&app.config, Some("default"), &app.snapshot));
        app.message = "svc 1 ok 1".into();

        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| app.draw(frame)).expect("draw");

        let screen = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(screen.contains("Giggity"));
        assert!(screen.contains("Resources"));
        assert!(screen.contains("Details"));
    }

    #[test]
    fn popup_draw_handles_empty_state_filter_and_logs() {
        let dir = tempdir().expect("tempdir");
        let client = DaemonClient::new(dir.path().join("missing.sock"));
        let mut app = PopupApp::new(client, Config::default(), None);
        app.filter_input = "api".into();
        app.show_logs = true;
        app.logs = "tail line".into();
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| app.draw(frame)).expect("draw");
        let screen = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(screen.contains("filter=api"));
        assert!(screen.contains("tail line"));
    }

    #[test]
    fn popup_render_details_handles_empty_and_metadata_sections() {
        let dir = tempdir().expect("tempdir");
        let client = DaemonClient::new(dir.path().join("missing.sock"));
        let app = PopupApp::new(client, Config::default(), None);
        assert_eq!(app.render_details(), "no resource selected");

        let dir = tempdir().expect("tempdir");
        let client = DaemonClient::new(dir.path().join("missing.sock"));
        let mut app = PopupApp::new(client, Config::default(), None);
        app.snapshot = Snapshot {
            resources: vec![ResourceRecord {
                project: None,
                ports: Vec::new(),
                urls: Vec::new(),
                metadata: BTreeMap::from([("pid".into(), "123".into())]),
                ..resource_record()
            }],
            ..Snapshot::default()
        };
        app.resolved = Some(resolve_view(&app.config, Some("default"), &app.snapshot));
        let details = app.render_details();
        assert!(details.contains("pid: 123"));
        assert!(details.contains("recent events:"));
    }

    #[test]
    fn event_formatter_includes_transition_and_timestamp() {
        let event = giggity_core::model::RecentEvent {
            resource_id: "id".into(),
            resource_name: "svc".into(),
            from: Some(HealthState::Healthy),
            to: HealthState::Degraded,
            timestamp: Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap(),
            cause: Some("probe failed".into()),
        };
        let rendered = format_event(&event);
        assert!(rendered.contains("healthy -> degraded"));
        assert!(rendered.contains("probe failed"));
    }

    fn resource_record() -> ResourceRecord {
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
}
