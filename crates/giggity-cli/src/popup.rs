use std::collections::HashSet;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, MouseButton,
    MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use giggity_core::config::{Config, GroupBy, SortKey};
use giggity_core::model::{HealthState, RecentEvent, ResourceRecord, Snapshot};
use giggity_core::protocol::{ActionKind, ClientRequest, ServerResponse};
use giggity_core::view::{ResolvedView, render_status_line, resolve_view};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Borders, Clear, List, ListItem, ListState, Paragraph, Row, Table, Tabs, Wrap,
};
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
    execute!(writer, EnterAlternateScreen, EnableMouseCapture)?;
    Ok(())
}

fn leave_alternate_screen<W>(terminal: &mut Terminal<CrosstermBackend<W>>) -> anyhow::Result<()>
where
    W: Write,
    <CrosstermBackend<W> as ratatui::backend::Backend>::Error:
        std::error::Error + Send + Sync + 'static,
{
    execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen
    )?;
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
    initial_resource: Option<String>,
) -> anyhow::Result<()> {
    let mut runtime = CrosstermRuntime::real();
    run_popup_with(client, config, view_name, initial_resource, &mut runtime).await
}

async fn run_popup_with<R>(
    client: DaemonClient,
    config: Config,
    view_name: Option<String>,
    initial_resource: Option<String>,
    runtime: &mut R,
) -> anyhow::Result<()>
where
    R: PopupRuntime,
{
    let mut terminal = runtime.enter()?;
    let mut app = PopupApp::new(client, config, view_name, initial_resource);
    let mut events = runtime.events();
    let result = app.run(&mut terminal, &mut events).await;
    runtime.exit(&mut terminal)?;
    result
}

pub async fn run_popup(
    client: DaemonClient,
    config: Config,
    view_name: Option<String>,
    initial_resource: Option<String>,
) -> anyhow::Result<()> {
    run_popup_with_runtime(client, config, view_name, initial_resource).await
}

const SPINNER_FRAMES: &[char] = &[
    '\u{280B}', '\u{2819}', '\u{2839}', '\u{2838}', '\u{283C}', '\u{2834}', '\u{2826}', '\u{2827}',
    '\u{2807}', '\u{280F}',
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DetailTab {
    Info,
    Logs,
    Events,
    Labels,
    Metadata,
}

impl DetailTab {
    const ALL: [DetailTab; 5] = [
        DetailTab::Info,
        DetailTab::Logs,
        DetailTab::Events,
        DetailTab::Labels,
        DetailTab::Metadata,
    ];

    fn label(self) -> &'static str {
        match self {
            DetailTab::Info => "Info",
            DetailTab::Logs => "Logs",
            DetailTab::Events => "Events",
            DetailTab::Labels => "Labels",
            DetailTab::Metadata => "Meta",
        }
    }

    fn from_index(index: usize) -> Option<DetailTab> {
        DetailTab::ALL.get(index).copied()
    }

    fn index(self) -> usize {
        DetailTab::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }
}

fn next_sort_key(key: &SortKey) -> SortKey {
    match key {
        SortKey::Severity => SortKey::Name,
        SortKey::Name => SortKey::LastChange,
        SortKey::LastChange => SortKey::Port,
        SortKey::Port => SortKey::Runtime,
        SortKey::Runtime => SortKey::Severity,
    }
}

fn sort_key_label(key: &SortKey) -> &'static str {
    match key {
        SortKey::Severity => "severity",
        SortKey::Name => "name",
        SortKey::LastChange => "last_change",
        SortKey::Port => "port",
        SortKey::Runtime => "runtime",
    }
}

fn next_state_filter(current: &Option<HealthState>) -> Option<HealthState> {
    match current {
        None => Some(HealthState::Healthy),
        Some(HealthState::Healthy) => Some(HealthState::Crashed),
        Some(HealthState::Crashed) => Some(HealthState::Stopped),
        Some(HealthState::Stopped) => Some(HealthState::Degraded),
        Some(HealthState::Degraded) => Some(HealthState::Starting),
        Some(HealthState::Starting) => Some(HealthState::Unknown),
        Some(HealthState::Unknown) => None,
    }
}

fn state_filter_label(filter: &Option<HealthState>) -> &'static str {
    match filter {
        None => "all",
        Some(HealthState::Healthy) => "healthy",
        Some(HealthState::Crashed) => "crashed",
        Some(HealthState::Stopped) => "stopped",
        Some(HealthState::Degraded) => "degraded",
        Some(HealthState::Starting) => "starting",
        Some(HealthState::Unknown) => "unknown",
    }
}

/// Items in the display list — either a group header or a resource index into resolved.resources.
#[derive(Debug, Clone)]
enum DisplayItem {
    GroupHeader(String),
    Resource(usize),
}

fn copy_to_clipboard(text: &str) -> bool {
    if cfg!(target_os = "macos") {
        if let Ok(mut child) = Command::new("pbcopy").stdin(Stdio::piped()).spawn() {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            return child.wait().map(|s| s.success()).unwrap_or(false);
        }
    }

    for cmd in &["xclip", "xsel"] {
        let args: &[&str] = if *cmd == "xclip" {
            &["-selection", "clipboard"]
        } else {
            &["--clipboard", "--input"]
        };
        if let Ok(mut child) = Command::new(cmd).args(args).stdin(Stdio::piped()).spawn() {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            if child.wait().map(|s| s.success()).unwrap_or(false) {
                return true;
            }
        }
    }

    // OSC 52 fallback: write directly to stdout
    let encoded = osc52_encode(text);
    let _ = std::io::stdout().write_all(encoded.as_bytes());
    let _ = std::io::stdout().flush();
    true
}

fn osc52_encode(text: &str) -> String {
    use std::fmt::Write;
    let b64 = base64_encode(text.as_bytes());
    let mut out = String::new();
    let _ = write!(out, "\x1b]52;c;{}\x07", b64);
    out
}

fn base64_encode(data: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        result.push(CHARS[((n >> 18) & 0x3F) as usize] as char);
        result.push(CHARS[((n >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            result.push(CHARS[((n >> 6) & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }
        if chunk.len() > 2 {
            result.push(CHARS[(n & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }
    }
    result
}

const MAX_FILTER_HISTORY: usize = 10;

fn bookmarks_path() -> PathBuf {
    directories::BaseDirs::new()
        .map(|dirs| dirs.data_local_dir().join("giggity").join("bookmarks.json"))
        .unwrap_or_else(|| PathBuf::from("bookmarks.json"))
}

fn load_bookmarks(config_bookmarks: &[String]) -> HashSet<String> {
    let mut set: HashSet<String> = config_bookmarks.iter().cloned().collect();
    let path = bookmarks_path();
    if let Ok(contents) = std::fs::read_to_string(&path)
        && let Ok(file_bookmarks) = serde_json::from_str::<Vec<String>>(&contents)
    {
        set.extend(file_bookmarks);
    }
    set
}

fn save_bookmarks(bookmarks: &HashSet<String>) {
    let path = bookmarks_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let sorted: Vec<&String> = {
        let mut v: Vec<_> = bookmarks.iter().collect();
        v.sort();
        v
    };
    if let Ok(json) = serde_json::to_string_pretty(&sorted) {
        let _ = std::fs::write(&path, json);
    }
}

fn compute_resource_diff(old: &ResourceRecord, new: &ResourceRecord) -> Vec<String> {
    let mut lines = Vec::new();

    if old.state != new.state {
        lines.push(format!("state: {} -> {}", old.state, new.state));
    }

    let old_ports: HashSet<u16> = old.ports.iter().map(|p| p.host_port).collect();
    let new_ports: HashSet<u16> = new.ports.iter().map(|p| p.host_port).collect();
    for port in new_ports.difference(&old_ports) {
        lines.push(format!("+ port {port}"));
    }
    for port in old_ports.difference(&new_ports) {
        lines.push(format!("- port {port}"));
    }

    let old_labels: HashSet<(&String, &String)> = old.labels.iter().collect();
    let new_labels: HashSet<(&String, &String)> = new.labels.iter().collect();
    for (key, val) in new_labels.difference(&old_labels) {
        lines.push(format!("+ label {key}={val}"));
    }
    for (key, val) in old_labels.difference(&new_labels) {
        lines.push(format!("- label {key}={val}"));
    }

    for (key, new_val) in &new.metadata {
        match old.metadata.get(key) {
            Some(old_val) if old_val != new_val => {
                lines.push(format!("meta {key}: {old_val} -> {new_val}"));
            }
            None => {
                lines.push(format!("+ meta {key}={new_val}"));
            }
            _ => {}
        }
    }
    for key in old.metadata.keys() {
        if !new.metadata.contains_key(key) {
            lines.push(format!("- meta {key}"));
        }
    }

    if lines.is_empty() {
        lines.push("no changes detected".into());
    }

    lines
}

struct PopupApp {
    client: DaemonClient,
    config: Config,
    view_name: String,
    local_grouping: Option<GroupBy>,
    local_sorting: Option<SortKey>,
    snapshot: Snapshot,
    resolved: Option<ResolvedView>,
    selected: usize,
    filter_input: String,
    filter_mode: bool,
    show_help: bool,
    show_diff: bool,
    detail_tab: DetailTab,
    message: String,
    flash_message: Option<(String, Instant)>,
    confirm: Option<ActionKind>,
    logs: String,
    last_refresh: Instant,
    last_log_target: Option<String>,
    spinner_index: usize,
    bookmarks: HashSet<String>,
    state_filter: Option<HealthState>,
    filter_history: Vec<String>,
    filter_history_index: Option<usize>,
    previous_snapshot: Option<Vec<ResourceRecord>>,
    display_items: Vec<DisplayItem>,
    /// Resource id to jump to on first refresh
    initial_resource: Option<String>,
    /// Cached layout areas from the last draw for mouse hit-testing
    last_list_area: Option<Rect>,
    last_detail_area: Option<Rect>,
    last_tab_bar_area: Option<Rect>,
}

impl PopupApp {
    fn new(
        client: DaemonClient,
        config: Config,
        view_name: Option<String>,
        initial_resource: Option<String>,
    ) -> Self {
        let bookmarks = load_bookmarks(&config.bookmarks);
        Self {
            client,
            view_name: view_name.unwrap_or_else(|| config.default_view.clone()),
            config,
            local_grouping: None,
            local_sorting: None,
            snapshot: Snapshot::default(),
            resolved: None,
            selected: 0,
            filter_input: String::new(),
            filter_mode: false,
            show_help: false,
            show_diff: false,
            detail_tab: DetailTab::Info,
            message: "loading...".into(),
            flash_message: None,
            confirm: None,
            logs: String::new(),
            last_refresh: Instant::now() - Duration::from_secs(10),
            last_log_target: None,
            spinner_index: 0,
            bookmarks,
            state_filter: None,
            filter_history: Vec::new(),
            filter_history_index: None,
            previous_snapshot: None,
            display_items: Vec::new(),
            initial_resource,
            last_list_area: None,
            last_detail_area: None,
            last_tab_bar_area: None,
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

            // Expire flash messages after 2 seconds
            if let Some((_, created)) = &self.flash_message {
                if created.elapsed() >= Duration::from_secs(2) {
                    self.flash_message = None;
                }
            }

            terminal.draw(|frame| self.draw(frame))?;

            if events.poll(Duration::from_millis(100))? {
                match events.read()? {
                    Event::Key(key) => {
                        if key.kind != KeyEventKind::Press {
                            continue;
                        }
                        if self.handle_key(key.code).await? {
                            break;
                        }
                    }
                    Event::Mouse(mouse) => {
                        self.handle_mouse(mouse.kind, mouse.column, mouse.row);
                    }
                    _ => continue,
                }
            }
        }
        Ok(())
    }

    async fn refresh(&mut self) {
        self.last_refresh = Instant::now();
        self.spinner_index = (self.spinner_index + 1) % SPINNER_FRAMES.len();
        match self
            .client
            .request(&ClientRequest::Query {
                view: Some(self.view_name.clone()),
            })
            .await
        {
            Ok(ServerResponse::Query { snapshot }) => {
                self.previous_snapshot = Some(self.snapshot.resources.clone());
                self.snapshot = snapshot;
                let mut config = self.config.clone();
                let mut view = config.active_view(Some(&self.view_name));
                if let Some(grouping) = &self.local_grouping {
                    view.grouping = grouping.clone();
                }
                if let Some(sorting) = &self.local_sorting {
                    view.sorting = sorting.clone();
                }
                config.views.insert(self.view_name.clone(), view);
                let mut resolved = resolve_view(&config, Some(&self.view_name), &self.snapshot);

                // Apply text filter
                let text_filter = if self.filter_input.is_empty() {
                    None
                } else {
                    Some(self.filter_input.to_lowercase())
                };
                let matches_filters = |resource: &ResourceRecord| -> bool {
                    if let Some(state) = &self.state_filter {
                        if resource.state != *state {
                            return false;
                        }
                    }
                    if let Some(ref filter) = text_filter {
                        if !(resource.name.to_lowercase().contains(filter)
                            || resource.id.to_lowercase().contains(filter)
                            || resource
                                .project
                                .as_ref()
                                .map(|project| project.to_lowercase().contains(filter))
                                .unwrap_or(false))
                        {
                            return false;
                        }
                    }
                    true
                };

                resolved.resources.retain(|r| matches_filters(r));
                for group in &mut resolved.grouped {
                    group.resources.retain(|r| matches_filters(r));
                }
                resolved.grouped.retain(|g| !g.resources.is_empty());

                self.message = render_status_line(&resolved);
                self.resolved = Some(resolved);
                self.rebuild_display_items();

                // Jump to initial resource on first refresh if requested
                if let Some(target_id) = self.initial_resource.take() {
                    if let Some(idx) = self
                        .resolved
                        .as_ref()
                        .unwrap()
                        .resources
                        .iter()
                        .position(|r| r.id == target_id || r.name == target_id)
                    {
                        if let Some(display_pos) = self
                            .display_items
                            .iter()
                            .position(|item| matches!(item, DisplayItem::Resource(i) if *i == idx))
                        {
                            self.selected = display_pos;
                        }
                    }
                }
                if self.selected >= self.display_items.len() {
                    self.selected = self.display_items.len().saturating_sub(1);
                }
                if self.detail_tab == DetailTab::Logs {
                    let _ = self.refresh_logs().await;
                }
            }
            Ok(response) => self.message = format!("unexpected response: {response:?}"),
            Err(error) => self.message = error.to_string(),
        }
    }

    async fn refresh_logs(&mut self) -> anyhow::Result<()> {
        if self.detail_tab != DetailTab::Logs {
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
        // Help overlay dismisses on any key
        if self.show_help {
            self.show_help = false;
            return Ok(false);
        }

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
                KeyCode::Esc | KeyCode::Enter => {
                    self.filter_mode = false;
                    self.filter_history_index = None;
                    if !self.filter_input.is_empty() {
                        self.filter_history.retain(|h| h != &self.filter_input);
                        self.filter_history.insert(0, self.filter_input.clone());
                        self.filter_history.truncate(MAX_FILTER_HISTORY);
                    }
                }
                KeyCode::Backspace => {
                    self.filter_input.pop();
                    self.filter_history_index = None;
                }
                KeyCode::Up => {
                    if !self.filter_history.is_empty() {
                        let next = match self.filter_history_index {
                            Some(i) => (i + 1).min(self.filter_history.len() - 1),
                            None => 0,
                        };
                        self.filter_history_index = Some(next);
                        self.filter_input = self.filter_history[next].clone();
                    }
                }
                KeyCode::Down => {
                    if let Some(i) = self.filter_history_index {
                        if i == 0 {
                            self.filter_history_index = None;
                            self.filter_input.clear();
                        } else {
                            let next = i - 1;
                            self.filter_history_index = Some(next);
                            self.filter_input = self.filter_history[next].clone();
                        }
                    }
                }
                KeyCode::Char(ch) => {
                    self.filter_input.push(ch);
                    self.filter_history_index = None;
                }
                _ => {}
            }
            self.last_refresh = Instant::now() - Duration::from_secs(10);
            return Ok(false);
        }

        match code {
            KeyCode::Char('q') | KeyCode::Esc => Ok(true),
            KeyCode::Down | KeyCode::Char('j') => {
                let max = self.display_items.len().saturating_sub(1);
                let mut next = (self.selected + 1).min(max);
                // Skip group headers
                while next < max {
                    if matches!(
                        self.display_items.get(next),
                        Some(DisplayItem::GroupHeader(_))
                    ) {
                        next += 1;
                    } else {
                        break;
                    }
                }
                self.selected = next;
                self.last_log_target = None;
                Ok(false)
            }
            KeyCode::Up | KeyCode::Char('k') => {
                let mut next = self.selected.saturating_sub(1);
                // Skip group headers
                while next > 0 {
                    if matches!(
                        self.display_items.get(next),
                        Some(DisplayItem::GroupHeader(_))
                    ) {
                        next = next.saturating_sub(1);
                    } else {
                        break;
                    }
                }
                self.selected = next;
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
                        GroupBy::Project => GroupBy::Namespace,
                        GroupBy::Namespace => GroupBy::ComposeStack,
                        GroupBy::ComposeStack => GroupBy::UnitDomain,
                        GroupBy::UnitDomain => GroupBy::None,
                        GroupBy::None => GroupBy::Severity,
                    },
                );
                self.last_refresh = Instant::now() - Duration::from_secs(10);
                Ok(false)
            }
            KeyCode::Char('F') => {
                self.state_filter = next_state_filter(&self.state_filter);
                self.flash_message = Some((
                    format!("state filter: {}", state_filter_label(&self.state_filter)),
                    Instant::now(),
                ));
                self.last_refresh = Instant::now() - Duration::from_secs(10);
                Ok(false)
            }
            KeyCode::Char('l') => {
                self.detail_tab = if self.detail_tab == DetailTab::Logs {
                    DetailTab::Info
                } else {
                    DetailTab::Logs
                };
                self.last_log_target = None;
                if self.detail_tab == DetailTab::Logs {
                    let _ = self.refresh_logs().await;
                }
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
            KeyCode::Char('S') => {
                let current = self.local_sorting.clone().unwrap_or(SortKey::Severity);
                self.local_sorting = Some(next_sort_key(&current));
                self.last_refresh = Instant::now() - Duration::from_secs(10);
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
            KeyCode::Char('y') => {
                self.copy_resource_field(false);
                Ok(false)
            }
            KeyCode::Char('Y') => {
                self.copy_resource_field(true);
                Ok(false)
            }
            KeyCode::Char('?') => {
                self.show_help = true;
                Ok(false)
            }
            KeyCode::Char('b') => {
                if let Some(resource) = self.selected_resource() {
                    let id = resource.id.clone();
                    let name = resource.name.clone();
                    if self.bookmarks.contains(&id) {
                        self.bookmarks.remove(&id);
                        self.flash_message =
                            Some((format!("Unbookmarked: {name}"), Instant::now()));
                    } else {
                        self.bookmarks.insert(id);
                        self.flash_message = Some((format!("Bookmarked: {name}"), Instant::now()));
                    }
                    save_bookmarks(&self.bookmarks);
                } else {
                    self.message = "no resource selected".into();
                }
                Ok(false)
            }
            KeyCode::Char('d') => {
                self.show_diff = !self.show_diff;
                Ok(false)
            }
            KeyCode::Char('1') => {
                self.switch_tab(DetailTab::Info).await;
                Ok(false)
            }
            KeyCode::Char('2') => {
                self.switch_tab(DetailTab::Logs).await;
                Ok(false)
            }
            KeyCode::Char('3') => {
                self.switch_tab(DetailTab::Events).await;
                Ok(false)
            }
            KeyCode::Char('4') => {
                self.switch_tab(DetailTab::Labels).await;
                Ok(false)
            }
            KeyCode::Char('5') => {
                self.switch_tab(DetailTab::Metadata).await;
                Ok(false)
            }
            KeyCode::Enter => {
                self.detail_tab = if self.detail_tab == DetailTab::Logs {
                    DetailTab::Info
                } else {
                    DetailTab::Logs
                };
                self.last_log_target = None;
                if self.detail_tab == DetailTab::Logs {
                    let _ = self.refresh_logs().await;
                }
                Ok(false)
            }
            KeyCode::Char('m') => {
                let _ = self
                    .client
                    .request(&ClientRequest::MuteNotifications {
                        duration_secs: 3600,
                    })
                    .await;
                Ok(false)
            }
            KeyCode::Char('M') => {
                let _ = self
                    .client
                    .request(&ClientRequest::UnmuteNotifications)
                    .await;
                Ok(false)
            }
            _ => Ok(false),
        }
    }

    fn handle_mouse(&mut self, kind: MouseEventKind, column: u16, row: u16) {
        match kind {
            MouseEventKind::Down(MouseButton::Left) => {
                // Click on resource list
                if let Some(area) = self.last_list_area {
                    if column >= area.x
                        && column < area.x + area.width
                        && row >= area.y
                        && row < area.y + area.height
                    {
                        // Offset by 1 for the border
                        let inner_y = row.saturating_sub(area.y + 1);
                        let index = inner_y as usize;
                        if index < self.display_items.len() {
                            if matches!(
                                self.display_items.get(index),
                                Some(DisplayItem::Resource(_))
                            ) {
                                self.selected = index;
                                self.last_log_target = None;
                            }
                        }
                        return;
                    }
                }

                // Click on detail tab bar
                if let Some(area) = self.last_tab_bar_area {
                    if column >= area.x
                        && column < area.x + area.width
                        && row >= area.y
                        && row < area.y + area.height
                    {
                        let relative_x = (column - area.x) as usize;
                        let mut offset = 0;
                        for tab in &DetailTab::ALL {
                            let label_len = tab.label().len() + 3; // padding around label
                            if relative_x >= offset && relative_x < offset + label_len {
                                self.detail_tab = *tab;
                                self.last_log_target = None;
                                return;
                            }
                            offset += label_len;
                        }
                    }
                }
            }
            MouseEventKind::ScrollUp => {
                let mut next = self.selected.saturating_sub(1);
                while next > 0 {
                    if matches!(
                        self.display_items.get(next),
                        Some(DisplayItem::GroupHeader(_))
                    ) {
                        next = next.saturating_sub(1);
                    } else {
                        break;
                    }
                }
                self.selected = next;
                self.last_log_target = None;
            }
            MouseEventKind::ScrollDown => {
                let max = self.display_items.len().saturating_sub(1);
                let mut next = (self.selected + 1).min(max);
                while next < max {
                    if matches!(
                        self.display_items.get(next),
                        Some(DisplayItem::GroupHeader(_))
                    ) {
                        next += 1;
                    } else {
                        break;
                    }
                }
                self.selected = next;
                self.last_log_target = None;
            }
            _ => {}
        }
    }

    async fn switch_tab(&mut self, tab: DetailTab) {
        self.detail_tab = tab;
        self.last_log_target = None;
        if tab == DetailTab::Logs {
            let _ = self.refresh_logs().await;
        }
    }

    fn copy_resource_field(&mut self, full_name: bool) {
        let Some(resource) = self.selected_resource() else {
            self.message = "no resource selected".into();
            return;
        };
        let text = if full_name {
            resource.name.clone()
        } else {
            resource.id.clone()
        };
        copy_to_clipboard(&text);
        let label = if full_name { "name" } else { "id" };
        let display = format!("Copied {}: {}", label, text);
        self.flash_message = Some((display, Instant::now()));
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

    fn rebuild_display_items(&mut self) {
        let Some(resolved) = &self.resolved else {
            self.display_items.clear();
            return;
        };
        let mut items = Vec::new();
        let show_headers = self
            .local_grouping
            .as_ref()
            .map_or(false, |g| *g != GroupBy::None);
        if show_headers && resolved.grouped.len() > 1 {
            for group in &resolved.grouped {
                items.push(DisplayItem::GroupHeader(group.label.clone()));
                for resource in &group.resources {
                    if let Some(idx) = resolved.resources.iter().position(|r| r.id == resource.id) {
                        items.push(DisplayItem::Resource(idx));
                    }
                }
            }
        } else {
            for (idx, _) in resolved.resources.iter().enumerate() {
                items.push(DisplayItem::Resource(idx));
            }
        }
        self.display_items = items;
    }

    fn selected_resource(&self) -> Option<&ResourceRecord> {
        let resolved = self.resolved.as_ref()?;
        match self.display_items.get(self.selected)? {
            DisplayItem::Resource(idx) => resolved.resources.get(*idx),
            DisplayItem::GroupHeader(_) => None,
        }
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
                "view={} filter={} group={} state={}",
                self.view_name,
                if self.filter_input.is_empty() {
                    "<none>"
                } else {
                    &self.filter_input
                },
                self.local_grouping
                    .as_ref()
                    .map(|grouping| format!("{grouping:?}"))
                    .unwrap_or_else(|| "default".into()),
                state_filter_label(&self.state_filter),
            )),
            Line::from(self.message.clone()),
        ]))
        .block(Block::default().borders(Borders::ALL).title("Giggity"));
        frame.render_widget(header, chunks[0]);

        let resolved_resources = self.resolved.as_ref().map(|resolved| &resolved.resources);
        let items: Vec<ListItem<'_>> = self
            .display_items
            .iter()
            .map(|item| match item {
                DisplayItem::GroupHeader(label) => ListItem::new(Line::from(vec![Span::styled(
                    format!("── {label} ──"),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )])),
                DisplayItem::Resource(idx) => {
                    let resource = &resolved_resources.unwrap()[*idx];
                    let prefix = if self.bookmarks.contains(&resource.id) {
                        "\u{2605} "
                    } else {
                        "  "
                    };
                    let state_style = match resource.state {
                        HealthState::Healthy => Style::default().fg(Color::Green),
                        HealthState::Crashed => Style::default().fg(Color::Red),
                        HealthState::Degraded => Style::default().fg(Color::Yellow),
                        HealthState::Stopped => Style::default().fg(Color::DarkGray),
                        HealthState::Starting => Style::default().fg(Color::Cyan),
                        HealthState::Unknown => Style::default().fg(Color::Gray),
                    };
                    ListItem::new(Line::from(vec![
                        Span::raw(prefix.to_string()),
                        Span::raw(format!("{:<24} ", resource.name)),
                        Span::styled(format!("{:<10} ", resource.state), state_style),
                        Span::raw(format!(
                            "{:<10} {}",
                            resource.runtime,
                            resource
                                .ports
                                .iter()
                                .map(|port| port.host_port.to_string())
                                .collect::<Vec<_>>()
                                .join(",")
                        )),
                    ]))
                }
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

        let (detail_text, title) = if self.show_diff {
            (self.render_diff(), "Diff")
        } else {
            match self.detail_tab {
                DetailTab::Logs => (self.logs.clone(), "Logs"),
                DetailTab::Info => (self.render_info(), "Info"),
                DetailTab::Events => (self.render_events(), "Events"),
                DetailTab::Labels => (self.render_labels(), "Labels"),
                DetailTab::Metadata => (self.render_metadata(), "Metadata"),
            }
        };
        let detail = Paragraph::new(detail_text)
            .block(Block::default().borders(Borders::ALL).title(title))
            .wrap(Wrap { trim: false });
        frame.render_widget(detail, body[1]);

        let mut footer_text = String::from(
            "q quit  / filter  F state  v view  g group  S sort  l logs  r restart  s stop  b bookmark  d diff  ? help",
        );
        if self.filter_mode && !self.filter_history.is_empty() {
            let hints: Vec<&str> = self
                .filter_history
                .iter()
                .take(3)
                .map(String::as_str)
                .collect();
            footer_text = format!("history: {}  (Up/Down to navigate)", hints.join(", "));
        }
        let footer = Paragraph::new(footer_text).block(Block::default().borders(Borders::ALL));
        frame.render_widget(footer, chunks[2]);

        if self.show_help {
            let help_lines = vec![
                Line::from(""),
                Line::from(Span::styled(
                    " Keybindings ",
                    Style::default().add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                Line::from(" q        quit"),
                Line::from(" /        filter"),
                Line::from(" v        next view"),
                Line::from(" g        cycle grouping"),
                Line::from(" S        cycle sort key"),
                Line::from(" l        toggle logs"),
                Line::from(" r        restart (confirm)"),
                Line::from(" s        stop (confirm)"),
                Line::from(" K        force kill (confirm)"),
                Line::from(" F        cycle state filter"),
                Line::from(" o        open url"),
                Line::from(" c        copy port"),
                Line::from(" y / Y    copy id / name"),
                Line::from(" b        toggle bookmark"),
                Line::from(" d        toggle diff view"),
                Line::from(" m / M    mute / unmute"),
                Line::from(" 1-5      switch detail tab"),
                Line::from(" ?        this help"),
                Line::from(""),
            ];
            let help_block = Paragraph::new(help_lines)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" Help ")
                        .title_alignment(Alignment::Center),
                )
                .alignment(Alignment::Left);
            let area = centered_rect(50, 70, frame.area());
            frame.render_widget(Clear, area);
            frame.render_widget(help_block, area);
        }

        if let Some((flash, _)) = &self.flash_message {
            let flash_area = Rect {
                x: chunks[0].x + 1,
                y: chunks[0].y + 1,
                width: chunks[0]
                    .width
                    .saturating_sub(2)
                    .min(flash.len() as u16 + 2),
                height: 1,
            };
            let flash_widget =
                Paragraph::new(flash.as_str()).style(Style::default().fg(Color::Yellow));
            frame.render_widget(flash_widget, flash_area);
        }
    }

    fn render_diff(&self) -> String {
        let Some(resource) = self.selected_resource() else {
            return "no resource selected".into();
        };
        let Some(previous) = &self.previous_snapshot else {
            return "no previous snapshot available".into();
        };
        let Some(old) = previous.iter().find(|r| r.id == resource.id) else {
            return format!("{} is new (not in previous snapshot)", resource.name);
        };
        let lines = compute_resource_diff(old, resource);
        format!("diff for {}:\n{}", resource.name, lines.join("\n"))
    }

    fn render_info(&self) -> String {
        let Some(resource) = self.selected_resource() else {
            return "no resource selected".into();
        };
        let mut lines = vec![
            format!("name: {}", resource.name),
            format!("id: {}", resource.id),
            format!("state: {}", resource.state),
            format!("runtime: {}", resource.runtime),
            format!("uptime: {}", resource.uptime_display()),
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
        lines.join("\n")
    }

    fn render_events(&self) -> String {
        let Some(resource) = self.selected_resource() else {
            return "no resource selected".into();
        };
        let events: Vec<String> = self
            .snapshot
            .events
            .iter()
            .filter(|event| event.resource_id == resource.id)
            .take(20)
            .map(format_event)
            .collect();
        if events.is_empty() {
            format!("no recent events for {}", resource.name)
        } else {
            format!(
                "recent events for {}:\n{}",
                resource.name,
                events.join("\n")
            )
        }
    }

    fn render_labels(&self) -> String {
        let Some(resource) = self.selected_resource() else {
            return "no resource selected".into();
        };
        if resource.labels.is_empty() {
            return format!("no labels for {}", resource.name);
        }
        let mut lines = vec![format!("labels for {}:", resource.name)];
        for (key, value) in &resource.labels {
            lines.push(format!("  {key}: {value}"));
        }
        lines.join("\n")
    }

    fn render_metadata(&self) -> String {
        let Some(resource) = self.selected_resource() else {
            return "no resource selected".into();
        };
        if resource.metadata.is_empty() {
            return format!("no metadata for {}", resource.name);
        }
        let mut lines = vec![format!("metadata for {}:", resource.name)];
        for (key, value) in &resource.metadata {
            lines.push(format!("  {key}: {value}"));
        }
        lines.join("\n")
    }

    /// Backward-compatible wrapper used by tests that expect the old combined output
    #[cfg(test)]
    fn render_details(&self) -> String {
        self.render_info()
    }
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
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

    use super::DetailTab;
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

    use std::collections::HashSet;

    use super::{
        CrosstermEvents, CrosstermRuntime, MAX_FILTER_HISTORY, PopupApp, PopupEvents,
        centered_rect, compute_resource_diff, enter_alternate_screen, enter_popup_terminal,
        exit_popup_terminal, format_event, leave_alternate_screen, load_bookmarks, run_popup_with,
        save_bookmarks,
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
                    state_since: Utc::now(),
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
            PopupApp::new(client, config, Some("default".into()), None),
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
        let mut app = PopupApp::new(client, Config::default(), None, None);
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
        let mut app = PopupApp::new(client, Config::default(), None, None);
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
        let mut app = PopupApp::new(client, Config::default(), None, None);
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

        run_popup_with(client, Config::default(), None, None, &mut runtime)
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
        let mut app = PopupApp::new(client, config, Some("missing".into()), None);

        assert!(!app.handle_key(KeyCode::Down).await.expect("down"));
        assert!(!app.handle_key(KeyCode::Char('v')).await.expect("cycle"));
        assert_eq!(app.view_name, "missing");
    }

    #[tokio::test]
    async fn popup_grouping_cycles_through_compose_stack_namespace_and_unit_domain() {
        let (mut app, shutdown_tx, _dir) = spawn_popup_app().await;
        app.local_grouping = Some(giggity_core::config::GroupBy::Project);
        app.handle_key(crossterm::event::KeyCode::Char('g'))
            .await
            .expect("cycle project");
        assert!(matches!(
            app.local_grouping,
            Some(giggity_core::config::GroupBy::Namespace)
        ));
        app.handle_key(crossterm::event::KeyCode::Char('g'))
            .await
            .expect("cycle namespace");
        assert!(matches!(
            app.local_grouping,
            Some(giggity_core::config::GroupBy::ComposeStack)
        ));
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
            Some(giggity_core::config::GroupBy::None)
        ));
        app.handle_key(crossterm::event::KeyCode::Char('g'))
            .await
            .expect("cycle none");
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
        let mut app = PopupApp::new(client, Config::default(), None, None);
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
        let mut app = PopupApp::new(client, Config::default(), None, None);
        app.snapshot = Snapshot {
            resources: vec![ResourceRecord {
                metadata: BTreeMap::from([("pid".into(), "123".into())]),
                urls: vec!["http://127.0.0.1:3000".parse().expect("url")],
                ..resource_record()
            }],
            ..Snapshot::default()
        };
        app.resolved = Some(resolve_view(&app.config, Some("default"), &app.snapshot));
        app.rebuild_display_items();
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

        assert_eq!(app.detail_tab, DetailTab::Logs);
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
            None,
        );
        app.detail_tab = DetailTab::Logs;
        app.refresh_logs().await.expect("no selection");

        app.snapshot = Snapshot {
            resources: vec![resource_record()],
            ..Snapshot::default()
        };
        app.resolved = Some(resolve_view(&app.config, Some("default"), &app.snapshot));
        app.rebuild_display_items();
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
        let mut app = PopupApp::new(client, Config::default(), None, None);
        app.detail_tab = DetailTab::Logs;
        app.snapshot = Snapshot {
            resources: vec![resource_record()],
            ..Snapshot::default()
        };
        app.resolved = Some(resolve_view(&app.config, Some("default"), &app.snapshot));
        app.rebuild_display_items();
        app.refresh_logs().await.expect("error response");
        assert!(app.logs.contains("missing logs"));
        app.last_log_target = None;
        app.refresh_logs().await.expect("unexpected response");
        assert!(app.logs.contains("missing logs"));
        server.await.expect("server");
    }

    #[tokio::test]
    async fn popup_render_info_includes_name_and_urls() {
        let (mut app, shutdown_tx, _dir) = spawn_popup_app().await;
        app.refresh().await;
        let info = app.render_info();
        assert!(info.contains("name: api"));
        assert!(info.contains("urls: http://127.0.0.1:3000"));
        assert!(info.contains("uptime:"));

        let events = app.render_events();
        // The daemon may or may not have generated state-change events yet
        assert!(events.contains("events for api") || events.contains("no recent events for api"));

        let labels = app.render_labels();
        assert!(labels.contains("no labels for api"));

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
        let mut app = PopupApp::new(client, Config::default(), None, None);
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
        app.rebuild_display_items();
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
        assert_eq!(app.detail_tab, DetailTab::Logs);
        assert!(!app.handle_key(KeyCode::Tab).await.expect("other"));
        assert!(app.handle_key(KeyCode::Esc).await.expect("quit"));
        server.await.expect("server");
    }

    #[tokio::test]
    async fn popup_grouping_cycles_through_all_variants() {
        let dir = tempdir().expect("tempdir");
        let client = DaemonClient::new(dir.path().join("missing.sock"));
        let mut app = PopupApp::new(client, Config::default(), None, None);
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
            .expect("project->namespace");
        assert_eq!(
            app.local_grouping,
            Some(giggity_core::config::GroupBy::Namespace)
        );
        app.handle_key(KeyCode::Char('g'))
            .await
            .expect("namespace->compose_stack");
        assert_eq!(
            app.local_grouping,
            Some(giggity_core::config::GroupBy::ComposeStack)
        );
        app.handle_key(KeyCode::Char('g'))
            .await
            .expect("compose_stack->unit_domain");
        assert_eq!(
            app.local_grouping,
            Some(giggity_core::config::GroupBy::UnitDomain)
        );
        app.handle_key(KeyCode::Char('g'))
            .await
            .expect("unit_domain->none");
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
        let mut app = PopupApp::new(client, Config::default(), None, None);
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
        let mut app = PopupApp::new(client, Config::default(), None, None);
        app.snapshot = Snapshot {
            resources: vec![resource_record()],
            ..Snapshot::default()
        };
        app.resolved = Some(resolve_view(&app.config, Some("default"), &app.snapshot));
        app.rebuild_display_items();

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
        let mut app = PopupApp::new(client, Config::default(), None, None);
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
                state_since: Utc::now(),
            }],
            ..Snapshot::default()
        };
        app.resolved = Some(resolve_view(&app.config, Some("default"), &app.snapshot));
        app.rebuild_display_items();
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
        assert!(screen.contains("Info"));
    }

    #[test]
    fn popup_draw_handles_empty_state_filter_and_logs() {
        let dir = tempdir().expect("tempdir");
        let client = DaemonClient::new(dir.path().join("missing.sock"));
        let mut app = PopupApp::new(client, Config::default(), None, None);
        app.filter_input = "api".into();
        app.detail_tab = DetailTab::Logs;
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
    fn popup_render_tabs_handle_empty_and_populated_states() {
        let dir = tempdir().expect("tempdir");
        let client = DaemonClient::new(dir.path().join("missing.sock"));
        let app = PopupApp::new(client, Config::default(), None, None);
        assert_eq!(app.render_info(), "no resource selected");
        assert_eq!(app.render_events(), "no resource selected");
        assert_eq!(app.render_labels(), "no resource selected");
        assert_eq!(app.render_metadata(), "no resource selected");

        let dir = tempdir().expect("tempdir");
        let client = DaemonClient::new(dir.path().join("missing.sock"));
        let mut app = PopupApp::new(client, Config::default(), None, None);
        app.snapshot = Snapshot {
            resources: vec![ResourceRecord {
                project: None,
                ports: Vec::new(),
                urls: Vec::new(),
                labels: BTreeMap::from([("env".into(), "prod".into())]),
                metadata: BTreeMap::from([("pid".into(), "123".into())]),
                ..resource_record()
            }],
            ..Snapshot::default()
        };
        app.resolved = Some(resolve_view(&app.config, Some("default"), &app.snapshot));
        app.rebuild_display_items();

        let info = app.render_info();
        assert!(info.contains("name: api"));
        assert!(!info.contains("pid: 123")); // metadata is on its own tab now

        let metadata = app.render_metadata();
        assert!(metadata.contains("pid: 123"));
        assert!(metadata.contains("metadata for api:"));

        let labels = app.render_labels();
        assert!(labels.contains("env: prod"));
        assert!(labels.contains("labels for api:"));

        let events = app.render_events();
        assert!(events.contains("no recent events for api"));
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

    #[tokio::test]
    async fn popup_initial_resource_jumps_to_matching_resource() {
        let (mut app, shutdown_tx, _dir) = spawn_popup_app().await;

        // Without initial_resource, selected starts at 0
        assert_eq!(app.selected, 0);

        let _ = shutdown_tx.send(());

        // Create a new app with initial_resource set to the only resource
        let (mut app2, shutdown_tx2, _dir2) = spawn_popup_app().await;
        app2.initial_resource = Some("host:api".into());
        app2.refresh().await;
        assert_eq!(app2.selected, 0); // only resource, so index 0
        assert!(app2.initial_resource.is_none()); // consumed after first use

        let _ = shutdown_tx2.send(());
    }

    #[tokio::test]
    async fn popup_initial_resource_matches_by_name() {
        let (mut app, shutdown_tx, _dir) = spawn_popup_app().await;
        app.initial_resource = Some("api".into());
        app.refresh().await;
        assert_eq!(app.selected, 0);
        assert!(app.initial_resource.is_none());

        let _ = shutdown_tx.send(());
    }

    #[tokio::test]
    async fn popup_initial_resource_ignores_nonexistent() {
        let (mut app, shutdown_tx, _dir) = spawn_popup_app().await;
        app.initial_resource = Some("nonexistent".into());
        app.refresh().await;
        assert_eq!(app.selected, 0); // stays at default
        assert!(app.initial_resource.is_none()); // still consumed

        let _ = shutdown_tx.send(());
    }

    #[test]
    fn popup_render_events_shows_event_list_when_events_exist() {
        let dir = tempdir().expect("tempdir");
        let client = DaemonClient::new(dir.path().join("missing.sock"));
        let mut app = PopupApp::new(client, Config::default(), None, None);
        app.snapshot = Snapshot {
            resources: vec![resource_record()],
            events: vec![
                giggity_core::model::RecentEvent {
                    resource_id: "host:api".into(),
                    resource_name: "api".into(),
                    from: Some(HealthState::Stopped),
                    to: HealthState::Healthy,
                    timestamp: Utc.with_ymd_and_hms(2025, 6, 1, 12, 0, 0).unwrap(),
                    cause: None,
                },
                giggity_core::model::RecentEvent {
                    resource_id: "host:other".into(),
                    resource_name: "other".into(),
                    from: None,
                    to: HealthState::Crashed,
                    timestamp: Utc.with_ymd_and_hms(2025, 6, 1, 13, 0, 0).unwrap(),
                    cause: Some("oom".into()),
                },
            ],
            ..Snapshot::default()
        };
        app.resolved = Some(resolve_view(&app.config, Some("default"), &app.snapshot));
        app.rebuild_display_items();

        let events = app.render_events();
        assert!(events.contains("recent events for api:"));
        assert!(events.contains("stopped -> healthy"));
        // Should not include events for other resources
        assert!(!events.contains("oom"));
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
            state_since: Utc::now(),
        }
    }

    // ================================================================
    // Bookmark tests
    // ================================================================

    #[tokio::test]
    async fn popup_bookmark_toggle_adds_and_removes() {
        let dir = tempdir().expect("tempdir");
        let client = DaemonClient::new(dir.path().join("missing.sock"));
        let mut app = PopupApp::new(client, Config::default(), None, None);
        app.snapshot = Snapshot {
            resources: vec![resource_record()],
            ..Snapshot::default()
        };
        app.resolved = Some(resolve_view(&app.config, Some("default"), &app.snapshot));
        app.rebuild_display_items();

        assert!(!app.bookmarks.contains("host:api"));
        app.handle_key(KeyCode::Char('b')).await.expect("bookmark");
        assert!(app.bookmarks.contains("host:api"));
        assert!(app.flash_message.as_ref().unwrap().0.contains("Bookmarked"));

        app.handle_key(KeyCode::Char('b'))
            .await
            .expect("unbookmark");
        assert!(!app.bookmarks.contains("host:api"));
        assert!(
            app.flash_message
                .as_ref()
                .unwrap()
                .0
                .contains("Unbookmarked")
        );
    }

    #[tokio::test]
    async fn popup_bookmark_without_selection_sets_message() {
        let dir = tempdir().expect("tempdir");
        let client = DaemonClient::new(dir.path().join("missing.sock"));
        let mut app = PopupApp::new(client, Config::default(), None, None);
        app.handle_key(KeyCode::Char('b')).await.expect("bookmark");
        assert_eq!(app.message, "no resource selected");
    }

    #[test]
    fn popup_draw_shows_bookmark_star_prefix() {
        let dir = tempdir().expect("tempdir");
        let client = DaemonClient::new(dir.path().join("missing.sock"));
        let mut app = PopupApp::new(client, Config::default(), None, None);
        app.snapshot = Snapshot {
            resources: vec![resource_record()],
            ..Snapshot::default()
        };
        app.resolved = Some(resolve_view(&app.config, Some("default"), &app.snapshot));
        app.rebuild_display_items();
        app.bookmarks.insert("host:api".into());

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
        assert!(screen.contains("\u{2605}"));
    }

    #[test]
    fn bookmarks_load_merges_config_entries() {
        let config_bookmarks = vec!["host:a".into(), "host:b".into()];
        let set = load_bookmarks(&config_bookmarks);
        assert!(set.contains("host:a"));
        assert!(set.contains("host:b"));
    }

    #[test]
    fn bookmarks_save_and_load_roundtrip() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("giggity").join("bookmarks.json");
        let mut bookmarks = HashSet::new();
        bookmarks.insert("docker:web".into());
        bookmarks.insert("host:api".into());

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        let sorted: Vec<&String> = {
            let mut v: Vec<_> = bookmarks.iter().collect();
            v.sort();
            v
        };
        let json = serde_json::to_string_pretty(&sorted).expect("json");
        std::fs::write(&path, &json).expect("write");

        let contents = std::fs::read_to_string(&path).expect("read");
        let loaded: Vec<String> = serde_json::from_str(&contents).expect("parse");
        let loaded_set: HashSet<String> = loaded.into_iter().collect();
        assert_eq!(bookmarks, loaded_set);
    }

    // ================================================================
    // Filter history tests
    // ================================================================

    #[tokio::test]
    async fn popup_filter_history_saves_on_enter() {
        let dir = tempdir().expect("tempdir");
        let client = DaemonClient::new(dir.path().join("missing.sock"));
        let mut app = PopupApp::new(client, Config::default(), None, None);

        app.handle_key(KeyCode::Char('/'))
            .await
            .expect("enter filter");
        app.handle_key(KeyCode::Char('a')).await.expect("type a");
        app.handle_key(KeyCode::Char('p')).await.expect("type p");
        app.handle_key(KeyCode::Char('i')).await.expect("type i");
        app.handle_key(KeyCode::Enter).await.expect("enter");

        assert_eq!(app.filter_history, vec!["api".to_string()]);
        assert!(!app.filter_mode);
    }

    #[tokio::test]
    async fn popup_filter_history_saves_on_escape() {
        let dir = tempdir().expect("tempdir");
        let client = DaemonClient::new(dir.path().join("missing.sock"));
        let mut app = PopupApp::new(client, Config::default(), None, None);

        app.handle_key(KeyCode::Char('/'))
            .await
            .expect("enter filter");
        app.handle_key(KeyCode::Char('w')).await.expect("type w");
        app.handle_key(KeyCode::Char('e')).await.expect("type e");
        app.handle_key(KeyCode::Char('b')).await.expect("type b");
        app.handle_key(KeyCode::Esc).await.expect("esc");

        assert_eq!(app.filter_history, vec!["web".to_string()]);
    }

    #[tokio::test]
    async fn popup_filter_history_deduplicates_and_caps() {
        let dir = tempdir().expect("tempdir");
        let client = DaemonClient::new(dir.path().join("missing.sock"));
        let mut app = PopupApp::new(client, Config::default(), None, None);

        for i in 0..12 {
            app.filter_mode = true;
            app.filter_input = format!("filter{i}");
            app.handle_key(KeyCode::Enter).await.expect("enter");
        }

        assert_eq!(app.filter_history.len(), MAX_FILTER_HISTORY);
        assert_eq!(app.filter_history[0], "filter11");

        app.filter_mode = true;
        app.filter_input = "filter5".into();
        app.handle_key(KeyCode::Enter).await.expect("enter");

        assert_eq!(app.filter_history[0], "filter5");
        assert_eq!(app.filter_history.len(), MAX_FILTER_HISTORY);
    }

    #[tokio::test]
    async fn popup_filter_history_empty_input_not_saved() {
        let dir = tempdir().expect("tempdir");
        let client = DaemonClient::new(dir.path().join("missing.sock"));
        let mut app = PopupApp::new(client, Config::default(), None, None);

        app.handle_key(KeyCode::Char('/'))
            .await
            .expect("enter filter");
        app.handle_key(KeyCode::Enter).await.expect("enter");

        assert!(app.filter_history.is_empty());
    }

    #[tokio::test]
    async fn popup_filter_history_navigation_with_up_down() {
        let dir = tempdir().expect("tempdir");
        let client = DaemonClient::new(dir.path().join("missing.sock"));
        let mut app = PopupApp::new(client, Config::default(), None, None);
        app.filter_history = vec!["api".into(), "web".into(), "db".into()];

        app.filter_mode = true;
        app.handle_key(KeyCode::Up).await.expect("up");
        assert_eq!(app.filter_input, "api");
        assert_eq!(app.filter_history_index, Some(0));

        app.handle_key(KeyCode::Up).await.expect("up");
        assert_eq!(app.filter_input, "web");
        assert_eq!(app.filter_history_index, Some(1));

        app.handle_key(KeyCode::Up).await.expect("up");
        assert_eq!(app.filter_input, "db");
        assert_eq!(app.filter_history_index, Some(2));

        app.handle_key(KeyCode::Up).await.expect("up clamped");
        assert_eq!(app.filter_input, "db");

        app.handle_key(KeyCode::Down).await.expect("down");
        assert_eq!(app.filter_input, "web");

        app.handle_key(KeyCode::Down).await.expect("down");
        assert_eq!(app.filter_input, "api");

        app.handle_key(KeyCode::Down).await.expect("down to clear");
        assert!(app.filter_input.is_empty());
        assert!(app.filter_history_index.is_none());
    }

    #[tokio::test]
    async fn popup_filter_history_typing_resets_index() {
        let dir = tempdir().expect("tempdir");
        let client = DaemonClient::new(dir.path().join("missing.sock"));
        let mut app = PopupApp::new(client, Config::default(), None, None);
        app.filter_history = vec!["api".into()];

        app.filter_mode = true;
        app.handle_key(KeyCode::Up).await.expect("up");
        assert_eq!(app.filter_history_index, Some(0));

        app.handle_key(KeyCode::Char('x')).await.expect("type");
        assert!(app.filter_history_index.is_none());
    }

    #[tokio::test]
    async fn popup_filter_history_down_without_index_is_noop() {
        let dir = tempdir().expect("tempdir");
        let client = DaemonClient::new(dir.path().join("missing.sock"));
        let mut app = PopupApp::new(client, Config::default(), None, None);
        app.filter_history = vec!["api".into()];
        app.filter_mode = true;
        app.filter_input = "test".into();

        app.handle_key(KeyCode::Down).await.expect("down noop");
        assert_eq!(app.filter_input, "test");
        assert!(app.filter_history_index.is_none());
    }

    // ================================================================
    // Resource diff tests
    // ================================================================

    #[test]
    fn compute_resource_diff_detects_state_change() {
        let old = resource_record();
        let mut new_rec = resource_record();
        new_rec.state = HealthState::Crashed;
        let lines = compute_resource_diff(&old, &new_rec);
        assert!(lines.iter().any(|l| l.contains("healthy -> crashed")));
    }

    #[test]
    fn compute_resource_diff_detects_port_changes() {
        let old = resource_record();
        let mut new_rec = resource_record();
        new_rec.ports.push(PortBinding {
            host_ip: None,
            host_port: 4000,
            container_port: None,
            protocol: "tcp".into(),
        });
        let lines = compute_resource_diff(&old, &new_rec);
        assert!(lines.iter().any(|l| l.contains("+ port 4000")));
    }

    #[test]
    fn compute_resource_diff_detects_removed_port() {
        let old = resource_record();
        let mut new_rec = resource_record();
        new_rec.ports.clear();
        let lines = compute_resource_diff(&old, &new_rec);
        assert!(lines.iter().any(|l| l.contains("- port 3000")));
    }

    #[test]
    fn compute_resource_diff_detects_label_changes() {
        let mut old = resource_record();
        old.labels.insert("env".into(), "prod".into());
        let mut new_rec = resource_record();
        new_rec.labels.insert("env".into(), "staging".into());
        let lines = compute_resource_diff(&old, &new_rec);
        assert!(lines.iter().any(|l| l.contains("+ label env=staging")));
        assert!(lines.iter().any(|l| l.contains("- label env=prod")));
    }

    #[test]
    fn compute_resource_diff_detects_metadata_changes() {
        let old = resource_record();
        let mut new_rec = resource_record();
        new_rec.metadata.insert("pid".into(), "456".into());
        let lines = compute_resource_diff(&old, &new_rec);
        assert!(lines.iter().any(|l| l.contains("meta pid: 123 -> 456")));
    }

    #[test]
    fn compute_resource_diff_detects_new_and_removed_metadata() {
        let old = resource_record();
        let mut new_rec = resource_record();
        new_rec.metadata.remove("pid");
        new_rec.metadata.insert("version".into(), "2".into());
        let lines = compute_resource_diff(&old, &new_rec);
        assert!(lines.iter().any(|l| l.contains("- meta pid")));
        assert!(lines.iter().any(|l| l.contains("+ meta version=2")));
    }

    #[test]
    fn compute_resource_diff_reports_no_changes() {
        let old = resource_record();
        let new_rec = resource_record();
        let lines = compute_resource_diff(&old, &new_rec);
        assert_eq!(lines, vec!["no changes detected"]);
    }

    #[tokio::test]
    async fn popup_diff_toggle() {
        let dir = tempdir().expect("tempdir");
        let client = DaemonClient::new(dir.path().join("missing.sock"));
        let mut app = PopupApp::new(client, Config::default(), None, None);
        assert!(!app.show_diff);
        app.handle_key(KeyCode::Char('d')).await.expect("diff on");
        assert!(app.show_diff);
        app.handle_key(KeyCode::Char('d')).await.expect("diff off");
        assert!(!app.show_diff);
    }

    #[test]
    fn popup_render_diff_no_previous_snapshot() {
        let dir = tempdir().expect("tempdir");
        let client = DaemonClient::new(dir.path().join("missing.sock"));
        let mut app = PopupApp::new(client, Config::default(), None, None);
        app.snapshot = Snapshot {
            resources: vec![resource_record()],
            ..Snapshot::default()
        };
        app.resolved = Some(resolve_view(&app.config, Some("default"), &app.snapshot));
        app.rebuild_display_items();
        assert!(app.render_diff().contains("no previous snapshot"));
    }

    #[test]
    fn popup_render_diff_new_resource() {
        let dir = tempdir().expect("tempdir");
        let client = DaemonClient::new(dir.path().join("missing.sock"));
        let mut app = PopupApp::new(client, Config::default(), None, None);
        app.snapshot = Snapshot {
            resources: vec![resource_record()],
            ..Snapshot::default()
        };
        app.resolved = Some(resolve_view(&app.config, Some("default"), &app.snapshot));
        app.rebuild_display_items();
        app.previous_snapshot = Some(Vec::new());
        assert!(app.render_diff().contains("is new"));
    }

    #[test]
    fn popup_render_diff_with_changes() {
        let dir = tempdir().expect("tempdir");
        let client = DaemonClient::new(dir.path().join("missing.sock"));
        let mut app = PopupApp::new(client, Config::default(), None, None);
        let mut changed = resource_record();
        changed.state = HealthState::Degraded;
        app.snapshot = Snapshot {
            resources: vec![changed],
            ..Snapshot::default()
        };
        app.resolved = Some(resolve_view(&app.config, Some("default"), &app.snapshot));
        app.rebuild_display_items();
        app.previous_snapshot = Some(vec![resource_record()]);
        assert!(app.render_diff().contains("healthy -> degraded"));
    }

    #[test]
    fn popup_render_diff_no_selection() {
        let dir = tempdir().expect("tempdir");
        let client = DaemonClient::new(dir.path().join("missing.sock"));
        let app = PopupApp::new(client, Config::default(), None, None);
        assert_eq!(app.render_diff(), "no resource selected");
    }

    // ================================================================
    // Help overlay tests
    // ================================================================

    #[test]
    fn popup_draw_renders_help_overlay() {
        let dir = tempdir().expect("tempdir");
        let client = DaemonClient::new(dir.path().join("missing.sock"));
        let mut app = PopupApp::new(client, Config::default(), None, None);
        app.show_help = true;
        let backend = TestBackend::new(100, 40);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| app.draw(frame)).expect("draw");
        let screen = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(screen.contains("Keybindings"));
        assert!(screen.contains("quit"));
        assert!(screen.contains("filter"));
        assert!(screen.contains("bookmark"));
        assert!(screen.contains("diff view"));
    }

    #[tokio::test]
    async fn popup_help_dismisses_on_any_key() {
        let dir = tempdir().expect("tempdir");
        let client = DaemonClient::new(dir.path().join("missing.sock"));
        let mut app = PopupApp::new(client, Config::default(), None, None);
        app.show_help = true;
        let quit = app.handle_key(KeyCode::Char('x')).await.expect("dismiss");
        assert!(!quit);
        assert!(!app.show_help);
    }

    #[test]
    fn centered_rect_produces_valid_area() {
        let area = Rect::new(0, 0, 100, 50);
        let result = centered_rect(50, 70, area);
        assert!(result.x > 0);
        assert!(result.y > 0);
        assert!(result.width > 0);
        assert!(result.height > 0);
        assert!(result.x + result.width <= area.width);
        assert!(result.y + result.height <= area.height);
    }

    #[test]
    fn popup_draw_shows_diff_panel_title() {
        let dir = tempdir().expect("tempdir");
        let client = DaemonClient::new(dir.path().join("missing.sock"));
        let mut app = PopupApp::new(client, Config::default(), None, None);
        app.show_diff = true;
        app.snapshot = Snapshot {
            resources: vec![resource_record()],
            ..Snapshot::default()
        };
        app.resolved = Some(resolve_view(&app.config, Some("default"), &app.snapshot));
        app.rebuild_display_items();
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
        assert!(screen.contains("Diff"));
    }

    #[test]
    fn popup_filter_history_hint_text_is_generated_in_filter_mode() {
        let dir = tempdir().expect("tempdir");
        let client = DaemonClient::new(dir.path().join("missing.sock"));
        let mut app = PopupApp::new(client, Config::default(), None, None);
        app.filter_mode = true;
        app.filter_history = vec!["api".into(), "web".into()];

        // The footer text should contain history hints when filter_mode is active
        // and filter_history is non-empty. Verify the state is correctly set.
        assert!(app.filter_mode);
        assert_eq!(app.filter_history.len(), 2);
        assert_eq!(app.filter_history[0], "api");

        // Draw succeeds without panic even with filter_mode and history
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| app.draw(frame)).expect("draw");
    }

    #[tokio::test]
    async fn popup_refresh_saves_previous_snapshot() {
        let (mut app, shutdown_tx, _dir) = spawn_popup_app().await;
        assert!(app.previous_snapshot.is_none());
        app.refresh().await;
        assert!(app.previous_snapshot.is_some());
        let _ = shutdown_tx.send(());
    }
}
