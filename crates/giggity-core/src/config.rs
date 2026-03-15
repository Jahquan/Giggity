use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use directories::ProjectDirs;
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::model::{HealthState, ResourceKind, RuntimeKind};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config at {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("failed to parse config at {path}: {source}")]
    Parse {
        path: String,
        source: toml::de::Error,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_refresh_seconds")]
    pub refresh_seconds: u64,
    #[serde(default = "default_startup_grace_seconds")]
    pub startup_grace_seconds: u64,
    #[serde(default = "default_host_event_ttl_seconds")]
    pub host_event_ttl_seconds: u64,
    #[serde(default = "default_cache_dir")]
    pub cache_dir: PathBuf,
    #[serde(default = "default_socket_path")]
    pub socket_path: PathBuf,
    #[serde(default = "default_default_view")]
    pub default_view: String,
    #[serde(default)]
    pub sources: SourceToggles,
    #[serde(default)]
    pub probes: Vec<ProbeSpec>,
    #[serde(default)]
    pub views: BTreeMap<String, ViewConfig>,
    #[serde(default)]
    pub bookmarks: Vec<String>,
    #[serde(default)]
    pub notifications: NotificationsConfig,
    #[serde(default)]
    pub integrations: IntegrationsConfig,
    #[serde(default)]
    pub popup: PopupConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PopupConfig {
    #[serde(default = "default_popup_size")]
    pub width: String,
    #[serde(default = "default_popup_size")]
    pub height: String,
}

impl Default for PopupConfig {
    fn default() -> Self {
        Self {
            width: default_popup_size(),
            height: default_popup_size(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceToggles {
    #[serde(default = "default_true")]
    pub docker: bool,
    #[serde(default = "default_true")]
    pub podman: bool,
    #[serde(default = "default_true")]
    pub nerdctl: bool,
    #[serde(default = "default_true")]
    pub kubernetes: bool,
    #[serde(default = "default_true")]
    pub host_listeners: bool,
    #[serde(default = "default_true")]
    pub launchd: bool,
    #[serde(default = "default_true")]
    pub systemd: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewConfig {
    #[serde(default)]
    pub sources: Option<SourceToggles>,
    #[serde(default)]
    pub include: Vec<MatchRule>,
    #[serde(default)]
    pub exclude: Vec<MatchRule>,
    #[serde(default)]
    pub grouping: GroupBy,
    #[serde(default)]
    pub sorting: SortKey,
    #[serde(default = "default_columns")]
    pub columns: Vec<Column>,
    #[serde(default = "default_details_fields")]
    pub details_fields: Vec<DetailField>,
    #[serde(default)]
    pub pinned: Vec<String>,
    #[serde(default)]
    pub aliases: BTreeMap<String, String>,
    #[serde(default)]
    pub hide: Vec<String>,
    #[serde(default)]
    pub severity_overrides: BTreeMap<String, HealthState>,
    #[serde(default)]
    pub status_bar: StatusBarConfig,
    #[serde(default = "default_actions")]
    pub actions: Vec<QuickAction>,
    #[serde(default)]
    pub theme: ThemeConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum GroupBy {
    #[default]
    Severity,
    Runtime,
    Project,
    Namespace,
    ComposeStack,
    UnitDomain,
    None,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SortKey {
    #[default]
    Severity,
    Name,
    LastChange,
    Runtime,
    Port,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Column {
    Name,
    Runtime,
    State,
    Project,
    Ports,
    Urls,
    Updated,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DetailField {
    Labels,
    Metadata,
    Urls,
    Ports,
    Events,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuickAction {
    Logs,
    Restart,
    Stop,
    OpenUrl,
    CopyPort,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusBarConfig {
    #[serde(default = "default_status_template")]
    pub template: String,
    #[serde(default = "default_separator")]
    pub separator: String,
    #[serde(default = "default_max_issue_names")]
    pub max_issue_names: usize,
    #[serde(default)]
    pub show_empty: bool,
    #[serde(default)]
    pub condensed: bool,
    #[serde(default)]
    pub show_runtime_counts: bool,
    #[serde(default = "default_segment")]
    pub segment: StatusSegment,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum StatusSegment {
    #[default]
    Right,
    Left,
    Center,
}

impl Default for StatusBarConfig {
    fn default() -> Self {
        Self {
            template: default_status_template(),
            separator: default_separator(),
            max_issue_names: default_max_issue_names(),
            show_empty: false,
            condensed: false,
            show_runtime_counts: false,
            segment: StatusSegment::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThemeConfig {
    #[serde(default = "default_ok_color")]
    pub ok_color: String,
    #[serde(default = "default_warn_color")]
    pub warn_color: String,
    #[serde(default = "default_error_color")]
    pub error_color: String,
    #[serde(default = "default_text_color")]
    pub text_color: String,
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            ok_color: default_ok_color(),
            warn_color: default_warn_color(),
            error_color: default_error_color(),
            text_color: default_text_color(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct MatchRule {
    #[serde(default)]
    pub runtime: Option<Vec<RuntimeKind>>,
    #[serde(default)]
    pub kind: Option<Vec<ResourceKind>>,
    #[serde(default)]
    pub state: Option<Vec<HealthState>>,
    #[serde(default)]
    pub name_regex: Option<String>,
    #[serde(default)]
    pub project_regex: Option<String>,
    #[serde(default)]
    pub namespace_regex: Option<String>,
    #[serde(default)]
    pub any_regex: Option<String>,
    #[serde(default)]
    pub ports: Option<Vec<u16>>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeSpec {
    pub name: String,
    #[serde(flatten)]
    pub matcher: MatchRule,
    #[serde(flatten)]
    pub kind: ProbeKind,
    #[serde(default = "default_probe_timeout_millis")]
    pub timeout_millis: u64,
    #[serde(default)]
    pub retries: u32,
    #[serde(default = "default_backoff_secs")]
    pub backoff_secs: u64,
    #[serde(default)]
    pub warn_latency_ms: Option<u64>,
    #[serde(default)]
    pub critical_latency_ms: Option<u64>,
    #[serde(default = "default_probe_interval_secs")]
    pub interval_secs: u64,
    #[serde(default = "default_probe_type")]
    pub probe_type: ProbeType,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProbeType {
    #[default]
    Http,
    Grpc,
    Tcp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "probe", rename_all = "snake_case")]
pub enum ProbeKind {
    Tcp {
        #[serde(default)]
        host: Option<String>,
        #[serde(default)]
        port: Option<u16>,
    },
    Http {
        url: String,
        #[serde(default = "default_http_expected_status")]
        expected_status: u16,
    },
    Command {
        program: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        contains: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationsConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub on_crash: bool,
    #[serde(default)]
    pub on_recovery: bool,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            on_crash: true,
            on_recovery: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationsConfig {
    #[serde(default = "default_cooldown_secs")]
    pub cooldown_secs: u64,
    #[serde(default)]
    pub slack: Option<SlackConfig>,
    #[serde(default)]
    pub telegram: Option<TelegramConfig>,
}

impl Default for IntegrationsConfig {
    fn default() -> Self {
        Self {
            cooldown_secs: default_cooldown_secs(),
            slack: None,
            telegram: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlackConfig {
    pub webhook_url: String,
    #[serde(default = "default_true")]
    pub on_crash: bool,
    #[serde(default)]
    pub on_recovery: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TelegramConfig {
    pub bot_token: String,
    pub chat_id: String,
    #[serde(default = "default_true")]
    pub on_crash: bool,
    #[serde(default)]
    pub on_recovery: bool,
}

pub type TmuxOverrides = BTreeMap<String, String>;

impl Default for Config {
    fn default() -> Self {
        let mut views = BTreeMap::new();
        views.insert("default".into(), ViewConfig::default());

        Self {
            refresh_seconds: default_refresh_seconds(),
            startup_grace_seconds: default_startup_grace_seconds(),
            host_event_ttl_seconds: default_host_event_ttl_seconds(),
            cache_dir: default_cache_dir(),
            socket_path: default_socket_path(),
            default_view: default_default_view(),
            sources: SourceToggles::default(),
            probes: Vec::new(),
            views,
            bookmarks: Vec::new(),
            notifications: NotificationsConfig::default(),
            integrations: IntegrationsConfig::default(),
            popup: PopupConfig::default(),
        }
    }
}

impl Default for SourceToggles {
    fn default() -> Self {
        Self {
            docker: true,
            podman: true,
            nerdctl: true,
            kubernetes: true,
            host_listeners: true,
            launchd: cfg!(target_os = "macos"),
            systemd: cfg!(target_os = "linux"),
        }
    }
}

impl Default for ViewConfig {
    fn default() -> Self {
        Self {
            sources: None,
            include: Vec::new(),
            exclude: Vec::new(),
            grouping: GroupBy::Severity,
            sorting: SortKey::Severity,
            columns: default_columns(),
            details_fields: default_details_fields(),
            pinned: Vec::new(),
            aliases: BTreeMap::new(),
            hide: Vec::new(),
            severity_overrides: BTreeMap::new(),
            status_bar: StatusBarConfig::default(),
            actions: default_actions(),
            theme: ThemeConfig::default(),
        }
    }
}

impl Config {
    pub fn default_path() -> PathBuf {
        let base = project_dirs()
            .map(|dirs| dirs.config_dir().to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("config.toml")
    }

    pub fn load() -> Result<Self, ConfigError> {
        Self::load_from(Self::default_path())
    }

    pub fn load_from(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        if !path.exists() {
            return Ok(Self::default());
        }

        let contents = fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.display().to_string(),
            source,
        })?;

        let mut parsed: Self = toml::from_str(&contents).map_err(|source| ConfigError::Parse {
            path: path.display().to_string(),
            source,
        })?;

        if parsed.views.is_empty() {
            parsed.views.insert("default".into(), ViewConfig::default());
        }

        Ok(parsed)
    }

    pub fn load_with_tmux_overrides(
        path: impl AsRef<Path>,
        overrides: &TmuxOverrides,
    ) -> Result<Self, ConfigError> {
        let mut config = Self::load_from(path)?;
        config.merge_tmux_overrides(overrides);
        Ok(config)
    }

    pub fn active_view(&self, view: Option<&str>) -> ViewConfig {
        let name = view.unwrap_or(&self.default_view);
        self.views.get(name).cloned().unwrap_or_default()
    }

    pub fn validate(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        if self.refresh_seconds == 0 {
            warnings.push("refresh_seconds should be at least 1".into());
        }
        if !self.views.contains_key(&self.default_view) {
            warnings.push(format!(
                "default_view '{}' is not defined; falling back to 'default'",
                self.default_view
            ));
        }

        for (view_name, view) in &self.views {
            for rule in view.include.iter().chain(&view.exclude) {
                for regex in [
                    &rule.name_regex,
                    &rule.project_regex,
                    &rule.namespace_regex,
                    &rule.any_regex,
                ]
                .into_iter()
                .flatten()
                {
                    if let Err(error) = Regex::new(regex) {
                        warnings.push(format!(
                            "view '{view_name}' has invalid regex '{regex}': {error}"
                        ));
                    }
                }
            }
        }

        for probe in &self.probes {
            for regex in [
                &probe.matcher.name_regex,
                &probe.matcher.project_regex,
                &probe.matcher.namespace_regex,
                &probe.matcher.any_regex,
            ]
            .into_iter()
            .flatten()
            {
                if let Err(error) = Regex::new(regex) {
                    warnings.push(format!(
                        "probe '{}' has invalid regex '{regex}': {error}",
                        probe.name
                    ));
                }
            }
            if probe.timeout_millis == 0 {
                warnings.push(format!(
                    "probe '{}' timeout_millis should be at least 1",
                    probe.name
                ));
            }
        }

        if let Some(slack) = &self.integrations.slack {
            if slack.webhook_url.is_empty() {
                warnings.push("integrations.slack.webhook_url is empty".into());
            }
        }
        if let Some(telegram) = &self.integrations.telegram {
            if telegram.bot_token.is_empty() {
                warnings.push("integrations.telegram.bot_token is empty".into());
            }
            if telegram.chat_id.is_empty() {
                warnings.push("integrations.telegram.chat_id is empty".into());
            }
        }

        let mut probe_names = std::collections::HashSet::new();
        for probe in &self.probes {
            if !probe_names.insert(&probe.name) {
                warnings.push(format!("duplicate probe name '{}'", probe.name));
            }
        }

        warnings
    }

    pub fn merge_tmux_overrides(&mut self, overrides: &TmuxOverrides) {
        for (key, value) in overrides {
            match key.as_str() {
                "view" => self.default_view = value.clone(),
                "refresh_seconds" => {
                    if let Ok(parsed) = value.parse() {
                        self.refresh_seconds = parsed;
                    }
                }
                "startup_grace_seconds" => {
                    if let Ok(parsed) = value.parse() {
                        self.startup_grace_seconds = parsed;
                    }
                }
                "max_issue_names" => {
                    if let Ok(parsed) = value.parse() {
                        self.default_view_mut().status_bar.max_issue_names = parsed;
                    }
                }
                "template" => self.default_view_mut().status_bar.template = value.clone(),
                "docker_enabled" => self.sources.docker = parse_bool(value, self.sources.docker),
                "podman_enabled" => self.sources.podman = parse_bool(value, self.sources.podman),
                "nerdctl_enabled" => self.sources.nerdctl = parse_bool(value, self.sources.nerdctl),
                "kubernetes_enabled" => {
                    self.sources.kubernetes = parse_bool(value, self.sources.kubernetes)
                }
                "host_enabled" => {
                    self.sources.host_listeners = parse_bool(value, self.sources.host_listeners)
                }
                "launchd_enabled" => self.sources.launchd = parse_bool(value, self.sources.launchd),
                "systemd_enabled" => self.sources.systemd = parse_bool(value, self.sources.systemd),
                "cooldown_secs" => {
                    if let Ok(parsed) = value.parse() {
                        self.integrations.cooldown_secs = parsed;
                    }
                }
                "hide_patterns" => {
                    self.default_view_mut().hide = value
                        .split(',')
                        .map(str::trim)
                        .filter(|segment| !segment.is_empty())
                        .map(ToOwned::to_owned)
                        .collect();
                }
                _ => {}
            }
        }
    }

    fn default_view_mut(&mut self) -> &mut ViewConfig {
        self.views.entry("default".into()).or_default()
    }
}

fn parse_bool(value: &str, default: bool) -> bool {
    match value {
        "1" | "true" | "on" | "yes" => true,
        "0" | "false" | "off" | "no" => false,
        _ => default,
    }
}

fn project_dirs() -> Option<ProjectDirs> {
    ProjectDirs::from("", "", "giggity")
}

fn default_refresh_seconds() -> u64 {
    2
}

fn default_startup_grace_seconds() -> u64 {
    10
}

fn default_host_event_ttl_seconds() -> u64 {
    60
}

fn default_cache_dir() -> PathBuf {
    project_dirs()
        .map(|dirs| dirs.cache_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from(".giggity-cache"))
}

fn default_socket_path() -> PathBuf {
    default_cache_dir().join("giggity.sock")
}

fn default_default_view() -> String {
    "default".to_string()
}

fn default_true() -> bool {
    true
}

fn default_cooldown_secs() -> u64 {
    300
}

fn default_probe_timeout_millis() -> u64 {
    1_000
}

fn default_backoff_secs() -> u64 {
    2
}

fn default_probe_interval_secs() -> u64 {
    30
}

fn default_probe_type() -> ProbeType {
    ProbeType::Http
}

fn default_segment() -> StatusSegment {
    StatusSegment::Right
}

fn default_http_expected_status() -> u16 {
    200
}

fn default_columns() -> Vec<Column> {
    vec![
        Column::Name,
        Column::State,
        Column::Runtime,
        Column::Project,
        Column::Ports,
    ]
}

fn default_details_fields() -> Vec<DetailField> {
    vec![
        DetailField::Ports,
        DetailField::Urls,
        DetailField::Labels,
        DetailField::Metadata,
        DetailField::Events,
    ]
}

fn default_actions() -> Vec<QuickAction> {
    vec![
        QuickAction::Logs,
        QuickAction::Restart,
        QuickAction::Stop,
        QuickAction::OpenUrl,
        QuickAction::CopyPort,
    ]
}

fn default_status_template() -> String {
    "svc {total} ok {healthy} warn {degraded} down {crashed} stop {stopped} src {collector_warnings} [{issues}]".to_string()
}

fn default_separator() -> String {
    " ".to_string()
}

fn default_max_issue_names() -> usize {
    3
}

fn default_ok_color() -> String {
    "green".to_string()
}

fn default_warn_color() -> String {
    "yellow".to_string()
}

fn default_error_color() -> String {
    "red".to_string()
}

fn default_text_color() -> String {
    "white".to_string()
}

fn default_popup_size() -> String {
    "80%".to_string()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::sync::{Mutex, OnceLock};

    use tempfile::tempdir;

    use super::{
        Config, DetailField, PopupConfig, ProbeKind, ProbeSpec, ProbeType, QuickAction,
        SourceToggles, StatusBarConfig, ThemeConfig, TmuxOverrides, ViewConfig, parse_bool,
    };
    use crate::test_support::EnvVarGuard;

    static TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    #[test]
    fn missing_config_uses_defaults() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let config = Config::load_from(path).expect("load");

        assert_eq!(config.refresh_seconds, 2);
        assert!(config.views.contains_key("default"));
    }

    #[test]
    fn tmux_overrides_take_precedence() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            r#"
refresh_seconds = 5

[views.default.status_bar]
template = "base"
"#,
        )
        .expect("write config");

        let mut overrides = BTreeMap::new();
        overrides.insert("refresh_seconds".into(), "1".into());
        overrides.insert("template".into(), "override".into());

        let config = Config::load_with_tmux_overrides(path, &TmuxOverrides::from(overrides))
            .expect("load with overrides");

        assert_eq!(config.refresh_seconds, 1);
        assert_eq!(config.active_view(None).status_bar.template, "override");
    }

    #[test]
    fn validates_probe_regexes() {
        let mut config = Config::default();
        config.probes.push(super::ProbeSpec {
            name: "bad".into(),
            matcher: super::MatchRule {
                name_regex: Some("[".into()),
                ..super::MatchRule::default()
            },
            kind: super::ProbeKind::Tcp {
                host: None,
                port: Some(3000),
            },
            timeout_millis: 0,
            retries: 0,
            backoff_secs: 2,
            warn_latency_ms: None,
            critical_latency_ms: None,
            interval_secs: 30,
            probe_type: super::ProbeType::default(),
        });

        let warnings = config.validate();
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("invalid regex"))
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("timeout_millis"))
        );
    }

    #[test]
    fn default_path_points_to_config_toml() {
        let path = Config::default_path();
        assert_eq!(
            path.file_name().and_then(|value| value.to_str()),
            Some("config.toml")
        );
    }

    #[test]
    fn load_uses_default_path_and_preserves_existing_view_set() {
        let _guard = TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("lock");
        let dir = tempdir().expect("tempdir");
        let _env = EnvVarGuard::set("XDG_CONFIG_HOME", dir.path().as_os_str().to_os_string());
        let config_dir = Config::default_path()
            .parent()
            .expect("config dir")
            .to_path_buf();
        fs::create_dir_all(&config_dir).expect("config dir");
        fs::write(
            Config::default_path(),
            r#"
default_view = "ops"

[views.ops.status_bar]
template = "ops {total}"
"#,
        )
        .expect("write config");

        let config = Config::load().expect("load");
        assert_eq!(config.default_view, "ops");
        assert_eq!(
            config.active_view(Some("ops")).status_bar.template,
            "ops {total}"
        );
    }

    #[test]
    fn load_from_surfaces_read_and_parse_errors() {
        let dir = tempdir().expect("tempdir");
        let read_error = Config::load_from(dir.path()).expect_err("directory read error");
        let read_message = read_error.to_string();
        assert!(read_message.contains("failed to read"));

        let path = dir.path().join("config.toml");
        fs::write(&path, "refresh_seconds = ").expect("bad config");
        let parse_error = Config::load_from(&path).expect_err("parse error");
        let parse_message = parse_error.to_string();
        assert!(parse_message.contains("failed to parse"));
    }

    #[test]
    fn tmux_overrides_update_source_toggles_and_hide_patterns() {
        let mut config = Config::default();
        config.merge_tmux_overrides(&TmuxOverrides::from(BTreeMap::from([
            ("docker_enabled".into(), "off".into()),
            ("kubernetes_enabled".into(), "off".into()),
            ("host_enabled".into(), "no".into()),
            ("hide_patterns".into(), "^foo,^bar".into()),
            ("max_issue_names".into(), "5".into()),
            ("startup_grace_seconds".into(), "12".into()),
            ("unknown".into(), "ignored".into()),
        ])));

        assert!(!config.sources.docker);
        assert!(!config.sources.kubernetes);
        assert!(!config.sources.host_listeners);
        assert_eq!(config.active_view(None).hide, vec!["^foo", "^bar"]);
        assert_eq!(config.active_view(None).status_bar.max_issue_names, 5);
        assert_eq!(config.startup_grace_seconds, 12);
    }

    #[test]
    fn validate_warns_when_default_view_is_missing_and_view_regex_is_invalid() {
        let mut config = Config {
            default_view: "ops".into(),
            ..Config::default()
        };
        config.active_view(None);
        config
            .views
            .entry("default".into())
            .or_default()
            .include
            .push(super::MatchRule {
                any_regex: Some("[".into()),
                ..super::MatchRule::default()
            });

        let warnings = config.validate();
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("default_view 'ops' is not defined"))
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("view 'default' has invalid regex"))
        );
    }

    #[test]
    fn merge_tmux_overrides_ignores_invalid_numbers_and_bool_defaults_hold() {
        let mut config = Config::default();
        config.merge_tmux_overrides(&TmuxOverrides::from(BTreeMap::from([
            ("refresh_seconds".into(), "bogus".into()),
            ("startup_grace_seconds".into(), "bogus".into()),
            ("docker_enabled".into(), "maybe".into()),
            ("podman_enabled".into(), "yes".into()),
            ("nerdctl_enabled".into(), "0".into()),
            ("kubernetes_enabled".into(), "1".into()),
            ("launchd_enabled".into(), "true".into()),
            ("systemd_enabled".into(), "false".into()),
        ])));

        assert_eq!(config.refresh_seconds, 2);
        assert_eq!(config.startup_grace_seconds, 10);
        assert!(config.sources.docker);
        assert!(config.sources.podman);
        assert!(!config.sources.nerdctl);
        assert!(config.sources.kubernetes);
        assert!(config.sources.launchd);
        assert!(!config.sources.systemd);
    }

    #[test]
    fn source_toggle_serde_defaults_and_parse_bool_cover_all_paths() {
        let toggles: SourceToggles = toml::from_str("").expect("empty toggles");
        assert!(toggles.docker);
        assert!(toggles.podman);
        assert!(toggles.nerdctl);
        assert!(toggles.kubernetes);
        assert!(toggles.host_listeners);
        assert!(toggles.launchd);
        assert!(toggles.systemd);

        assert!(parse_bool("yes", false));
        assert!(parse_bool("on", false));
        assert!(!parse_bool("no", true));
        assert!(!parse_bool("off", true));
        assert!(parse_bool("maybe", true));
        assert!(!parse_bool("maybe", false));
    }

    #[test]
    fn active_view_and_unknown_tmux_overrides_fall_back_safely() {
        let mut config = Config::default();
        config.views.insert("ops".into(), ViewConfig::default());
        config.merge_tmux_overrides(&TmuxOverrides::from(BTreeMap::from([
            ("unknown_key".into(), "ignored".into()),
            ("view".into(), "missing".into()),
            ("docker_enabled".into(), "no".into()),
        ])));

        assert_eq!(config.default_view, "missing");
        assert!(!config.sources.docker);
        assert_eq!(config.active_view(Some("missing")), ViewConfig::default());
    }

    #[test]
    fn active_view_uses_named_default_and_load_inserts_missing_default_view() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            r#"
refresh_seconds = 7
default_view = "ops"
"#,
        )
        .expect("write config");

        let mut loaded = Config::load_from(&path).expect("load");
        assert_eq!(loaded.refresh_seconds, 7);
        assert!(loaded.views.contains_key("default"));

        loaded.views.insert(
            "ops".into(),
            ViewConfig {
                status_bar: StatusBarConfig {
                    template: "ops {total}".into(),
                    ..StatusBarConfig::default()
                },
                ..ViewConfig::default()
            },
        );
        loaded.default_view = "ops".into();
        assert_eq!(loaded.active_view(None).status_bar.template, "ops {total}");
    }

    #[test]
    fn tmux_overrides_cover_remaining_keys_and_zero_refresh_warning() {
        let mut config = Config::default();
        config.views.insert("ops".into(), ViewConfig::default());
        config.merge_tmux_overrides(&TmuxOverrides::from(BTreeMap::from([
            ("view".into(), "ops".into()),
            ("template".into(), "ops {healthy}".into()),
            ("refresh_seconds".into(), "0".into()),
            ("startup_grace_seconds".into(), "9".into()),
            ("podman_enabled".into(), "false".into()),
            ("nerdctl_enabled".into(), "off".into()),
            ("kubernetes_enabled".into(), "no".into()),
            ("host_enabled".into(), "true".into()),
            ("launchd_enabled".into(), "1".into()),
            ("systemd_enabled".into(), "yes".into()),
            ("hide_patterns".into(), "  ".into()),
        ])));

        assert_eq!(config.default_view, "ops");
        assert_eq!(config.refresh_seconds, 0);
        assert_eq!(config.startup_grace_seconds, 9);
        assert_eq!(
            config.active_view(Some("default")).status_bar.template,
            "ops {healthy}"
        );
        assert!(config.active_view(Some("default")).hide.is_empty());
        assert_eq!(config.active_view(None), ViewConfig::default());
        assert!(!config.sources.podman);
        assert!(!config.sources.nerdctl);
        assert!(!config.sources.kubernetes);
        assert!(config.sources.host_listeners);
        assert!(config.sources.launchd);
        assert!(config.sources.systemd);

        let warnings = config.validate();
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("refresh_seconds should be at least 1"))
        );
    }

    #[test]
    fn validate_warns_when_probe_timeout_is_zero() {
        let mut config = Config::default();
        config.probes.push(ProbeSpec {
            name: "tcp".into(),
            matcher: Default::default(),
            kind: ProbeKind::Tcp {
                host: None,
                port: Some(80),
            },
            timeout_millis: 0,
            retries: 0,
            backoff_secs: 2,
            warn_latency_ms: None,
            critical_latency_ms: None,
            interval_secs: 30,
            probe_type: ProbeType::default(),
        });

        let warnings = config.validate();
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("timeout_millis should be at least 1"))
        );
    }

    #[test]
    fn validate_accepts_clean_config_and_invalid_max_issue_override_is_ignored() {
        let mut config = Config::default();
        config.views.insert(
            "default".into(),
            ViewConfig {
                include: vec![super::MatchRule {
                    name_regex: Some("^api$".into()),
                    project_regex: Some("^proj$".into()),
                    namespace_regex: Some("^ns$".into()),
                    any_regex: Some("team=dev".into()),
                    ..super::MatchRule::default()
                }],
                ..ViewConfig::default()
            },
        );
        config.probes.push(ProbeSpec {
            name: "http".into(),
            matcher: super::MatchRule {
                name_regex: Some("^api$".into()),
                ..super::MatchRule::default()
            },
            kind: ProbeKind::Http {
                url: "http://127.0.0.1:3000/health".into(),
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
        let baseline = config.active_view(None).status_bar.max_issue_names;
        config.merge_tmux_overrides(&TmuxOverrides::from(BTreeMap::from([(
            "max_issue_names".into(),
            "bogus".into(),
        )])));

        assert_eq!(
            config.active_view(None).status_bar.max_issue_names,
            baseline
        );
        assert!(config.validate().is_empty());
    }

    #[test]
    fn serde_defaults_cover_theme_actions_probe_and_view_defaults() {
        let config: Config = toml::from_str(
            r#"
[views.default]
"#,
        )
        .expect("config");
        let view = config.active_view(None);
        assert_eq!(view.columns.len(), 5);
        assert_eq!(view.details_fields.len(), 5);
        assert_eq!(view.actions.len(), 5);
        assert_eq!(
            view.status_bar.template,
            "svc {total} ok {healthy} warn {degraded} down {crashed} stop {stopped} src {collector_warnings} [{issues}]"
        );
        assert_eq!(view.status_bar.separator, " ");
        assert_eq!(view.status_bar.max_issue_names, 3);
        assert_eq!(view.theme.ok_color, "green");
        assert_eq!(view.theme.warn_color, "yellow");
        assert_eq!(view.theme.error_color, "red");
        assert_eq!(view.theme.text_color, "white");

        let probe: ProbeSpec = toml::from_str(
            r#"
name = "http"
probe = "http"
url = "http://127.0.0.1:3000/health"
"#,
        )
        .expect("probe");
        assert_eq!(probe.timeout_millis, 1_000);
        assert!(matches!(
            probe.kind,
            ProbeKind::Http {
                expected_status: 200,
                ..
            }
        ));

        let status_bar = StatusBarConfig::default();
        assert_eq!(status_bar.max_issue_names, 3);
        let theme = ThemeConfig::default();
        assert_eq!(theme.ok_color, "green");
        let view = ViewConfig::default();
        assert_eq!(
            view.columns,
            vec![
                super::Column::Name,
                super::Column::State,
                super::Column::Runtime,
                super::Column::Project,
                super::Column::Ports,
            ]
        );
        assert_eq!(
            view.details_fields,
            vec![
                DetailField::Ports,
                DetailField::Urls,
                DetailField::Labels,
                DetailField::Metadata,
                DetailField::Events,
            ]
        );
        assert_eq!(
            view.actions,
            vec![
                QuickAction::Logs,
                QuickAction::Restart,
                QuickAction::Stop,
                QuickAction::OpenUrl,
                QuickAction::CopyPort,
            ]
        );
    }

    #[test]
    fn popup_config_defaults_to_80_percent() {
        let popup = PopupConfig::default();
        assert_eq!(popup.width, "80%");
        assert_eq!(popup.height, "80%");
    }

    #[test]
    fn popup_config_deserializes_from_toml() {
        let config: Config = toml::from_str(
            r#"
[popup]
width = "60%"
height = "40"
"#,
        )
        .expect("config with popup");
        assert_eq!(config.popup.width, "60%");
        assert_eq!(config.popup.height, "40");
    }

    #[test]
    fn popup_config_uses_defaults_when_omitted() {
        let config: Config = toml::from_str("").expect("empty config");
        assert_eq!(config.popup.width, "80%");
        assert_eq!(config.popup.height, "80%");
    }
}
