use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, HashSet};

use chrono::{DateTime, Utc};
use regex::Regex;

use crate::config::{Config, GroupBy, MatchRule, SortKey, ViewConfig};
use crate::model::{HealthState, ResourceKind, ResourceRecord, RuntimeKind, Snapshot};

#[derive(Debug, Clone)]
pub struct GroupedResources {
    pub label: String,
    pub resources: Vec<ResourceRecord>,
}

#[derive(Debug, Clone)]
pub struct ViewSummary {
    pub total: usize,
    pub healthy: usize,
    pub starting: usize,
    pub degraded: usize,
    pub crashed: usize,
    pub stopped: usize,
    pub unknown: usize,
    pub collector_warnings: usize,
    pub warning_sources: Vec<String>,
    pub issues: Vec<String>,
    pub latest_change: Option<DateTime<Utc>>,
    pub last_crash_at: Option<DateTime<Utc>>,
    pub runtime_counts: BTreeMap<String, usize>,
}

#[derive(Debug, Clone)]
pub struct ResolvedView {
    pub name: String,
    pub config: ViewConfig,
    pub resources: Vec<ResourceRecord>,
    pub grouped: Vec<GroupedResources>,
    pub summary: ViewSummary,
}

#[derive(Debug, Clone)]
pub struct CompiledMatchRule {
    runtime: Option<Vec<RuntimeKind>>,
    kind: Option<Vec<ResourceKind>>,
    state: Option<Vec<HealthState>>,
    name_regex: CompiledRegex,
    project_regex: CompiledRegex,
    namespace_regex: CompiledRegex,
    any_regex: CompiledRegex,
    ports: Option<HashSet<u16>>,
    labels: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
enum CompiledRegex {
    Missing,
    Valid(Regex),
    Invalid,
}

pub fn resolve_view(config: &Config, view_name: Option<&str>, snapshot: &Snapshot) -> ResolvedView {
    let name = view_name.unwrap_or(&config.default_view).to_string();
    let view_config = config.active_view(Some(&name));
    let merged_sources = merged_sources(config, &view_config);
    let hidden = compile_patterns(&view_config.hide);
    let include = compile_match_rules(&view_config.include);
    let exclude = compile_match_rules(&view_config.exclude);
    let mut resources: Vec<_> = snapshot
        .resources
        .iter()
        .filter(|resource| resource_allowed_by_sources(resource.runtime, &merged_sources))
        .filter(|resource| matches_include_compiled(&include, resource))
        .filter(|resource| !matches_any_compiled(&exclude, resource))
        .cloned()
        .map(|mut resource| {
            if let Some(alias) = view_config
                .aliases
                .get(&resource.id)
                .or_else(|| view_config.aliases.get(&resource.name))
            {
                resource.name = alias.clone();
            }
            if let Some(state) = view_config
                .severity_overrides
                .get(&resource.id)
                .or_else(|| view_config.severity_overrides.get(&resource.name))
            {
                resource.state = *state;
            }
            resource
        })
        .filter(|resource| {
            !hidden.iter().any(|regex| {
                regex.is_match(&resource.name)
                    || regex.is_match(&resource.id)
                    || resource
                        .project
                        .as_ref()
                        .map(|project| regex.is_match(project))
                        .unwrap_or(false)
            })
        })
        .collect();

    sort_resources(&mut resources, &view_config);
    pin_resources(&mut resources, &view_config);

    let summary = summarize(
        &resources,
        &view_config,
        &snapshot.warnings,
        snapshot.last_crash_at,
    );
    let grouped = group_resources(&resources, &view_config.grouping);

    ResolvedView {
        name,
        config: view_config,
        resources,
        grouped,
        summary,
    }
}

pub fn render_status_line(resolved: &ResolvedView) -> String {
    if resolved.resources.is_empty() && !resolved.config.status_bar.show_empty {
        return "giggity idle".to_string();
    }

    if resolved.config.status_bar.condensed {
        return render_condensed(resolved);
    }

    let mut output = render_template(resolved);
    if resolved.config.status_bar.show_runtime_counts {
        output.push(' ');
        output.push_str(&render_runtime_counts(resolved));
    }
    output
}

fn render_condensed(resolved: &ResolvedView) -> String {
    let s = &resolved.summary;
    let mut parts = vec![format!("ok:{}", s.healthy)];
    if s.degraded > 0 || s.starting > 0 {
        parts.push(format!("warn:{}", s.degraded + s.starting));
    }
    if s.crashed > 0 {
        parts.push(format!("err:{}", s.crashed));
    }
    if resolved.config.status_bar.show_runtime_counts {
        parts.push(render_runtime_counts(resolved));
    }
    parts.join(" ")
}

fn render_runtime_counts(resolved: &ResolvedView) -> String {
    resolved
        .summary
        .runtime_counts
        .iter()
        .map(|(runtime, count)| format!("{runtime}:{count}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn render_template(resolved: &ResolvedView) -> String {
    let issues = if resolved.summary.issues.is_empty() {
        "none".to_string()
    } else {
        resolved
            .summary
            .issues
            .join(&resolved.config.status_bar.separator)
    };

    let warning_sources = if resolved.summary.warning_sources.is_empty() {
        "none".to_string()
    } else {
        resolved
            .summary
            .warning_sources
            .join(&resolved.config.status_bar.separator)
    };

    resolved
        .config
        .status_bar
        .template
        .replace("{total}", &resolved.summary.total.to_string())
        .replace("{healthy}", &resolved.summary.healthy.to_string())
        .replace("{starting}", &resolved.summary.starting.to_string())
        .replace("{degraded}", &resolved.summary.degraded.to_string())
        .replace("{crashed}", &resolved.summary.crashed.to_string())
        .replace("{stopped}", &resolved.summary.stopped.to_string())
        .replace("{unknown}", &resolved.summary.unknown.to_string())
        .replace(
            "{collector_warnings}",
            &resolved.summary.collector_warnings.to_string(),
        )
        .replace("{warning_sources}", &warning_sources)
        .replace("{issues}", &issues)
}

pub fn render_tmux_status_line(resolved: &ResolvedView) -> String {
    render_tmux_status_line_at(resolved, Utc::now())
}

fn render_tmux_status_line_at(resolved: &ResolvedView, now: DateTime<Utc>) -> String {
    if resolved.resources.is_empty() && !resolved.config.status_bar.show_empty {
        return tmux_wrap("giggity idle", &resolved.config.theme.text_color);
    }

    let issues = if resolved.summary.issues.is_empty() {
        "none".to_string()
    } else {
        resolved
            .summary
            .issues
            .join(&resolved.config.status_bar.separator)
    };
    let warning_sources = if resolved.summary.warning_sources.is_empty() {
        "none".to_string()
    } else {
        resolved
            .summary
            .warning_sources
            .join(&resolved.config.status_bar.separator)
    };

    let base = &resolved.config.theme.text_color;
    let issue_color = if resolved.summary.crashed > 0 {
        &resolved.config.theme.error_color
    } else if resolved.summary.degraded > 0
        || resolved.summary.starting > 0
        || resolved.summary.collector_warnings > 0
    {
        &resolved.config.theme.warn_color
    } else {
        base
    };

    let styled = resolved
        .config
        .status_bar
        .template
        .replace(
            "{total}",
            &tmux_inline(
                &resolved.summary.total.to_string(),
                &resolved.config.theme.text_color,
                base,
            ),
        )
        .replace(
            "{healthy}",
            &tmux_inline(
                &resolved.summary.healthy.to_string(),
                &resolved.config.theme.ok_color,
                base,
            ),
        )
        .replace(
            "{starting}",
            &tmux_inline(
                &resolved.summary.starting.to_string(),
                &resolved.config.theme.warn_color,
                base,
            ),
        )
        .replace(
            "{degraded}",
            &tmux_inline(
                &resolved.summary.degraded.to_string(),
                &resolved.config.theme.warn_color,
                base,
            ),
        )
        .replace(
            "{crashed}",
            &tmux_inline(
                &resolved.summary.crashed.to_string(),
                &resolved.config.theme.error_color,
                base,
            ),
        )
        .replace(
            "{stopped}",
            &tmux_inline(
                &resolved.summary.stopped.to_string(),
                &resolved.config.theme.text_color,
                base,
            ),
        )
        .replace(
            "{unknown}",
            &tmux_inline(
                &resolved.summary.unknown.to_string(),
                &resolved.config.theme.warn_color,
                base,
            ),
        )
        .replace(
            "{collector_warnings}",
            &tmux_inline(
                &resolved.summary.collector_warnings.to_string(),
                &resolved.config.theme.warn_color,
                base,
            ),
        )
        .replace(
            "{warning_sources}",
            &tmux_inline(&warning_sources, issue_color, base),
        )
        .replace("{issues}", &tmux_inline(&issues, issue_color, base));

    let flash = is_crash_flash_active(resolved.summary.last_crash_at, now);
    if flash {
        format!("#[fg=red,bold]{}#[default]", tmux_wrap(&styled, base))
    } else {
        tmux_wrap(&styled, base)
    }
}

fn merged_sources(config: &Config, view: &ViewConfig) -> crate::config::SourceToggles {
    view.sources
        .clone()
        .unwrap_or_else(|| config.sources.clone())
}

fn resource_allowed_by_sources(
    runtime: RuntimeKind,
    sources: &crate::config::SourceToggles,
) -> bool {
    match runtime {
        RuntimeKind::Docker => sources.docker,
        RuntimeKind::Podman => sources.podman,
        RuntimeKind::Nerdctl => sources.nerdctl,
        RuntimeKind::Kubernetes => sources.kubernetes,
        RuntimeKind::Host => sources.host_listeners,
        RuntimeKind::Launchd => sources.launchd,
        RuntimeKind::Probes => true,
        RuntimeKind::Systemd => sources.systemd,
    }
}

pub fn matches_include(include: &[MatchRule], resource: &ResourceRecord) -> bool {
    include.is_empty() || matches_any(include, resource)
}

pub fn matches_any(rules: &[MatchRule], resource: &ResourceRecord) -> bool {
    let compiled = compile_match_rules(rules);
    matches_any_compiled(&compiled, resource)
}

pub fn matches_rule(rule: &MatchRule, resource: &ResourceRecord) -> bool {
    matches_compiled_rule(&compile_match_rule(rule), resource)
}

pub fn compile_match_rule(rule: &MatchRule) -> CompiledMatchRule {
    CompiledMatchRule {
        runtime: rule.runtime.clone(),
        kind: rule.kind.clone(),
        state: rule.state.clone(),
        name_regex: CompiledRegex::from_pattern(rule.name_regex.as_deref()),
        project_regex: CompiledRegex::from_pattern(rule.project_regex.as_deref()),
        namespace_regex: CompiledRegex::from_pattern(rule.namespace_regex.as_deref()),
        any_regex: CompiledRegex::from_pattern(rule.any_regex.as_deref()),
        ports: rule
            .ports
            .as_ref()
            .map(|ports| ports.iter().copied().collect::<HashSet<_>>()),
        labels: rule.labels.clone(),
    }
}

pub fn compile_match_rules(rules: &[MatchRule]) -> Vec<CompiledMatchRule> {
    rules.iter().map(compile_match_rule).collect()
}

pub fn matches_compiled_rule(rule: &CompiledMatchRule, resource: &ResourceRecord) -> bool {
    if let Some(runtimes) = &rule.runtime
        && !runtimes.contains(&resource.runtime)
    {
        return false;
    }
    if let Some(kinds) = &rule.kind
        && !kinds.contains(&resource.kind)
    {
        return false;
    }
    if let Some(states) = &rule.state
        && !states.contains(&resource.state)
    {
        return false;
    }
    if let Some(ports) = &rule.ports {
        let port_set: HashSet<_> = resource.ports.iter().map(|port| port.host_port).collect();
        if !ports.iter().all(|port| port_set.contains(port)) {
            return false;
        }
    }
    if !rule
        .labels
        .iter()
        .all(|(key, value)| resource.labels.get(key) == Some(value))
    {
        return false;
    }

    rule.name_regex.matches(&resource.name)
        && rule
            .project_regex
            .matches(resource.project.as_deref().unwrap_or(""))
        && rule.namespace_regex.matches(
            resource
                .metadata
                .get("namespace")
                .map(String::as_str)
                .unwrap_or(""),
        )
        && rule.any_regex.matches(&full_text(resource))
}

fn matches_include_compiled(include: &[CompiledMatchRule], resource: &ResourceRecord) -> bool {
    include.is_empty() || matches_any_compiled(include, resource)
}

fn matches_any_compiled(rules: &[CompiledMatchRule], resource: &ResourceRecord) -> bool {
    rules
        .iter()
        .any(|rule| matches_compiled_rule(rule, resource))
}

fn full_text(resource: &ResourceRecord) -> String {
    let mut parts = vec![
        resource.id.clone(),
        resource.name.clone(),
        resource.project.clone().unwrap_or_default(),
        resource.runtime.to_string(),
        resource.kind.to_string(),
        resource.state.to_string(),
    ];
    parts.extend(
        resource
            .labels
            .iter()
            .map(|(key, value)| format!("{key}={value}")),
    );
    parts.extend(
        resource
            .metadata
            .iter()
            .map(|(key, value)| format!("{key}={value}")),
    );
    parts.join(" ")
}

fn sort_resources(resources: &mut [ResourceRecord], view: &ViewConfig) {
    match view.sorting {
        SortKey::Severity => resources.sort_by_key(|resource| {
            (
                Reverse(resource.state.severity()),
                resource.name.to_lowercase(),
                resource.runtime.to_string(),
            )
        }),
        SortKey::Name => resources.sort_by_key(|resource| resource.name.to_lowercase()),
        SortKey::LastChange => resources.sort_by_key(|resource| Reverse(resource.last_changed)),
        SortKey::Runtime => resources
            .sort_by_key(|resource| (resource.runtime.to_string(), resource.name.to_lowercase())),
        SortKey::Port => resources.sort_by_key(|resource| {
            (
                resource
                    .ports
                    .first()
                    .map(|port| port.host_port)
                    .unwrap_or(u16::MAX),
                resource.name.to_lowercase(),
            )
        }),
    }
}

fn pin_resources(resources: &mut Vec<ResourceRecord>, view: &ViewConfig) {
    if view.pinned.is_empty() {
        return;
    }
    let mut pinned = Vec::new();
    let mut rest = Vec::new();
    for resource in resources.drain(..) {
        if view
            .pinned
            .iter()
            .any(|entry| entry == &resource.id || entry == &resource.name)
        {
            pinned.push(resource);
        } else {
            rest.push(resource);
        }
    }
    pinned.extend(rest);
    *resources = pinned;
}

fn summarize(
    resources: &[ResourceRecord],
    view: &ViewConfig,
    warnings: &[crate::model::CollectorWarning],
    last_crash_at: Option<DateTime<Utc>>,
) -> ViewSummary {
    let warning_sources = warnings
        .iter()
        .map(|warning| warning.source.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();

    let mut summary = ViewSummary {
        total: resources.len(),
        healthy: 0,
        starting: 0,
        degraded: 0,
        crashed: 0,
        stopped: 0,
        unknown: 0,
        collector_warnings: warnings.len(),
        warning_sources,
        issues: Vec::new(),
        latest_change: resources.iter().map(|resource| resource.last_changed).max(),
        last_crash_at,
        runtime_counts: BTreeMap::new(),
    };

    for resource in resources {
        match resource.state {
            HealthState::Healthy => summary.healthy += 1,
            HealthState::Starting => summary.starting += 1,
            HealthState::Degraded => summary.degraded += 1,
            HealthState::Crashed => summary.crashed += 1,
            HealthState::Stopped => summary.stopped += 1,
            HealthState::Unknown => summary.unknown += 1,
        }
        *summary
            .runtime_counts
            .entry(resource.runtime.to_string())
            .or_default() += 1;
    }

    let stack_keys = resources
        .iter()
        .filter(|resource| resource.kind == ResourceKind::ComposeStack)
        .filter_map(|resource| {
            resource
                .project
                .as_ref()
                .map(|project| (resource.runtime, project.clone()))
        })
        .collect::<BTreeSet<_>>();

    summary.issues = resources
        .iter()
        .filter(|resource| resource.state.is_issue())
        .filter(|resource| {
            if resource.kind != ResourceKind::Container {
                return true;
            }
            match resource.compose_project() {
                Some(project) => !stack_keys.contains(&(resource.runtime, project.to_string())),
                None => true,
            }
        })
        .map(ResourceRecord::summary_name)
        .take(view.status_bar.max_issue_names)
        .collect();

    summary
}

fn group_resources(resources: &[ResourceRecord], grouping: &GroupBy) -> Vec<GroupedResources> {
    let mut buckets: BTreeMap<String, Vec<ResourceRecord>> = BTreeMap::new();
    for resource in resources {
        let key = match grouping {
            GroupBy::Severity => resource.state.to_string(),
            GroupBy::Runtime => resource.runtime.to_string(),
            GroupBy::Project => resource
                .project
                .clone()
                .unwrap_or_else(|| "ungrouped".into()),
            GroupBy::Namespace => resource
                .namespace()
                .map(str::to_owned)
                .unwrap_or_else(|| "ungrouped".into()),
            GroupBy::ComposeStack => resource
                .compose_project()
                .map(str::to_owned)
                .unwrap_or_else(|| "ungrouped".into()),
            GroupBy::UnitDomain => resource
                .metadata
                .get("domain")
                .cloned()
                .unwrap_or_else(|| "ungrouped".into()),
            GroupBy::None => "all".into(),
        };
        buckets.entry(key).or_default().push(resource.clone());
    }
    buckets
        .into_iter()
        .map(|(label, resources)| GroupedResources { label, resources })
        .collect()
}

fn compile_patterns(patterns: &[String]) -> Vec<Regex> {
    patterns
        .iter()
        .filter_map(|pattern| Regex::new(pattern).ok())
        .collect()
}

impl CompiledRegex {
    fn from_pattern(pattern: Option<&str>) -> Self {
        match pattern {
            Some(pattern) => Regex::new(pattern)
                .map(Self::Valid)
                .unwrap_or(Self::Invalid),
            None => Self::Missing,
        }
    }

    fn matches(&self, value: &str) -> bool {
        match self {
            Self::Missing => true,
            Self::Valid(pattern) => pattern.is_match(value),
            Self::Invalid => false,
        }
    }
}

const CRASH_FLASH_SECONDS: i64 = 30;

fn is_crash_flash_active(last_crash_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    match last_crash_at {
        Some(crash_time) => {
            let elapsed = now.signed_duration_since(crash_time).num_seconds();
            elapsed >= 0 && elapsed < CRASH_FLASH_SECONDS
        }
        None => false,
    }
}

fn tmux_inline(value: &str, color: &str, base_color: &str) -> String {
    format!("#[fg={color}]{value}#[fg={base_color}]")
}

fn tmux_wrap(value: &str, color: &str) -> String {
    format!("#[fg={color}]{value}#[default]")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use chrono::{TimeZone, Utc};
    use proptest::prelude::*;

    use crate::config::{Config, GroupBy, MatchRule, SortKey, StatusBarConfig, ViewConfig};
    use crate::model::{
        HealthState, PortBinding, ResourceKind, ResourceRecord, RuntimeKind, Snapshot,
    };

    use super::{
        matches_any, matches_include, matches_rule, render_status_line, render_tmux_status_line,
        resolve_view,
    };

    fn resource(name: &str, state: HealthState, runtime: RuntimeKind) -> ResourceRecord {
        ResourceRecord {
            id: format!("{runtime}:{name}"),
            kind: ResourceKind::HostProcess,
            runtime,
            project: Some("proj".into()),
            name: name.into(),
            state,
            runtime_status: None,
            ports: vec![PortBinding {
                host_ip: None,
                host_port: 3000,
                container_port: None,
                protocol: "tcp".into(),
            }],
            labels: BTreeMap::from([("team".into(), "dev".into())]),
            urls: Vec::new(),
            metadata: BTreeMap::from([("namespace".into(), "ns".into())]),
            last_changed: Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap(),
            state_since: Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap(),
        }
    }

    #[test]
    fn filters_by_runtime_and_renders_issue_summary() {
        let mut config = Config::default();
        config.views.insert(
            "default".into(),
            ViewConfig {
                include: vec![MatchRule {
                    runtime: Some(vec![RuntimeKind::Docker]),
                    ..MatchRule::default()
                }],
                status_bar: StatusBarConfig {
                    template: "{crashed}:{issues}".into(),
                    ..StatusBarConfig::default()
                },
                ..ViewConfig::default()
            },
        );
        let snapshot = Snapshot {
            resources: vec![
                resource("api", HealthState::Crashed, RuntimeKind::Docker),
                resource("db", HealthState::Healthy, RuntimeKind::Host),
            ],
            ..Snapshot::default()
        };

        let resolved = resolve_view(&config, None, &snapshot);
        assert_eq!(resolved.resources.len(), 1);
        assert_eq!(render_status_line(&resolved), "1:proj/api:3000");
    }

    #[test]
    fn resolves_aliases_hides_pins_and_grouping() {
        let mut config = Config::default();
        config.views.insert(
            "default".into(),
            ViewConfig {
                grouping: GroupBy::Project,
                sorting: SortKey::Name,
                pinned: vec!["docker:api".into()],
                aliases: BTreeMap::from([("docker:api".into(), "frontend".into())]),
                hide: vec!["worker".into()],
                ..ViewConfig::default()
            },
        );
        let snapshot = Snapshot {
            resources: vec![
                ResourceRecord {
                    id: "docker:api".into(),
                    kind: ResourceKind::Container,
                    runtime: RuntimeKind::Docker,
                    ..resource("api", HealthState::Healthy, RuntimeKind::Docker)
                },
                ResourceRecord {
                    id: "docker:worker".into(),
                    kind: ResourceKind::Container,
                    runtime: RuntimeKind::Docker,
                    ..resource("worker", HealthState::Degraded, RuntimeKind::Docker)
                },
            ],
            ..Snapshot::default()
        };

        let resolved = resolve_view(&config, None, &snapshot);
        assert_eq!(resolved.resources.len(), 1);
        assert_eq!(resolved.resources[0].name, "frontend");
        assert_eq!(resolved.grouped[0].label, "proj");
    }

    #[test]
    fn match_rules_check_ports_labels_and_regexes() {
        let resource = resource("api", HealthState::Healthy, RuntimeKind::Docker);
        let rule = MatchRule {
            runtime: Some(vec![RuntimeKind::Docker]),
            kind: Some(vec![ResourceKind::HostProcess]),
            state: Some(vec![HealthState::Healthy]),
            name_regex: Some("^api$".into()),
            project_regex: Some("^proj$".into()),
            namespace_regex: Some("^ns$".into()),
            any_regex: Some("team=dev".into()),
            ports: Some(vec![3000]),
            labels: BTreeMap::from([("team".into(), "dev".into())]),
        };
        assert!(matches_rule(&rule, &resource));
    }

    #[test]
    fn render_status_line_returns_idle_when_empty_and_can_show_empty_summary() {
        let config = Config::default();
        let empty = resolve_view(&config, None, &Snapshot::default());
        assert_eq!(render_status_line(&empty), "giggity idle");

        let mut config = Config::default();
        let mut view = config.active_view(None);
        view.status_bar.show_empty = true;
        view.status_bar.template = "svc {total} [{issues}]".into();
        config.views.insert("default".into(), view);
        let empty = resolve_view(&config, None, &Snapshot::default());
        assert_eq!(render_status_line(&empty), "svc 0 [none]");
    }

    #[test]
    fn render_status_line_includes_stopped_and_warning_placeholders() {
        let mut config = Config::default();
        config.views.insert(
            "default".into(),
            ViewConfig {
                status_bar: StatusBarConfig {
                    template: "stop {stopped} src {collector_warnings} [{warning_sources}]".into(),
                    ..StatusBarConfig::default()
                },
                ..ViewConfig::default()
            },
        );
        let snapshot = Snapshot {
            resources: vec![resource("api", HealthState::Stopped, RuntimeKind::Host)],
            warnings: vec![
                crate::model::CollectorWarning {
                    source: "podman".into(),
                    message: "failed".into(),
                },
                crate::model::CollectorWarning {
                    source: "nerdctl".into(),
                    message: "failed".into(),
                },
            ],
            ..Snapshot::default()
        };

        let resolved = resolve_view(&config, None, &snapshot);
        assert_eq!(
            render_status_line(&resolved),
            "stop 1 src 2 [nerdctl podman]"
        );
    }

    #[test]
    fn render_tmux_status_line_applies_theme_colors() {
        let mut config = Config::default();
        config.views.insert(
            "default".into(),
            ViewConfig {
                status_bar: StatusBarConfig {
                    template: "ok {healthy} down {crashed} stop {stopped} [{issues}]".into(),
                    ..StatusBarConfig::default()
                },
                ..ViewConfig::default()
            },
        );
        let mut view = config.active_view(None);
        view.theme.ok_color = "green".into();
        view.theme.error_color = "red".into();
        view.theme.warn_color = "yellow".into();
        view.theme.text_color = "white".into();
        config.views.insert("default".into(), view);
        let snapshot = Snapshot {
            resources: vec![
                resource("api", HealthState::Healthy, RuntimeKind::Host),
                resource("db", HealthState::Crashed, RuntimeKind::Docker),
                resource("cache", HealthState::Stopped, RuntimeKind::Host),
            ],
            ..Snapshot::default()
        };

        let resolved = resolve_view(&config, None, &snapshot);
        let tmux = render_tmux_status_line(&resolved);
        assert!(tmux.contains("#[fg=green]1#[fg=white]"));
        assert!(tmux.contains("#[fg=red]1#[fg=white]"));
        assert!(tmux.contains("stop #[fg=white]1#[fg=white]"));
        assert!(tmux.contains("#[fg=red]proj/db:3000#[fg=white]"));
    }

    #[test]
    fn issue_summary_prefers_compose_stack_over_member_container_entries() {
        let mut config = Config::default();
        let mut view = config.active_view(None);
        view.status_bar.max_issue_names = 5;
        config.views.insert("default".into(), view);

        let stack = ResourceRecord {
            id: "compose:docker:stack".into(),
            kind: ResourceKind::ComposeStack,
            runtime: RuntimeKind::Docker,
            project: Some("stack".into()),
            name: "stack stack".into(),
            state: HealthState::Crashed,
            runtime_status: Some("1/2 healthy".into()),
            ports: vec![PortBinding {
                host_ip: None,
                host_port: 8080,
                container_port: Some(80),
                protocol: "tcp".into(),
            }],
            labels: BTreeMap::from([("com.docker.compose.project".into(), "stack".into())]),
            urls: Vec::new(),
            metadata: BTreeMap::new(),
            last_changed: Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 10).unwrap(),
            state_since: Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 10).unwrap(),
        };
        let web = ResourceRecord {
            kind: ResourceKind::Container,
            runtime: RuntimeKind::Docker,
            project: Some("stack".into()),
            labels: BTreeMap::from([("com.docker.compose.project".into(), "stack".into())]),
            ..resource("web", HealthState::Crashed, RuntimeKind::Docker)
        };
        let standalone = resource("standalone", HealthState::Degraded, RuntimeKind::Host);
        let snapshot = Snapshot {
            resources: vec![web, stack, standalone],
            ..Snapshot::default()
        };

        let resolved = resolve_view(&config, None, &snapshot);
        assert_eq!(
            resolved.summary.issues,
            vec![
                "stack stack:8080".to_string(),
                "proj/standalone:3000".to_string()
            ]
        );
    }

    #[test]
    fn issue_summary_keeps_issue_containers_without_compose_project() {
        let mut config = Config::default();
        let mut view = config.active_view(None);
        view.status_bar.max_issue_names = 5;
        config.views.insert("default".into(), view);

        let standalone_container = ResourceRecord {
            kind: ResourceKind::Container,
            runtime: RuntimeKind::Docker,
            project: None,
            labels: BTreeMap::new(),
            name: "standalone".into(),
            ports: vec![PortBinding {
                host_ip: None,
                host_port: 8081,
                container_port: Some(80),
                protocol: "tcp".into(),
            }],
            ..resource("standalone", HealthState::Crashed, RuntimeKind::Docker)
        };
        let snapshot = Snapshot {
            resources: vec![standalone_container],
            ..Snapshot::default()
        };

        let resolved = resolve_view(&config, None, &snapshot);
        assert_eq!(resolved.summary.issues, vec!["standalone:8081".to_string()]);
    }

    #[test]
    fn resolve_view_applies_name_based_severity_overrides() {
        let mut config = Config::default();
        let mut view = ViewConfig::default();
        view.severity_overrides
            .insert("api".into(), HealthState::Crashed);
        config.views.insert("default".into(), view);

        let snapshot = Snapshot {
            resources: vec![resource("api", HealthState::Healthy, RuntimeKind::Host)],
            ..Snapshot::default()
        };
        let resolved = resolve_view(&config, None, &snapshot);
        assert_eq!(resolved.resources[0].state, HealthState::Crashed);
    }

    #[test]
    fn resolve_view_aliases_by_name_and_hides_by_project() {
        let mut config = Config::default();
        let mut view = ViewConfig::default();
        view.aliases.insert("api".into(), "service".into());
        view.hide.push("^hidden$".into());
        config.views.insert("default".into(), view);

        let mut visible = resource("api", HealthState::Healthy, RuntimeKind::Host);
        visible.project = Some("visible".into());
        let mut hidden = resource("db", HealthState::Healthy, RuntimeKind::Host);
        hidden.project = Some("hidden".into());
        let snapshot = Snapshot {
            resources: vec![visible, hidden],
            ..Snapshot::default()
        };

        let resolved = resolve_view(&config, None, &snapshot);
        assert_eq!(resolved.resources.len(), 1);
        assert_eq!(resolved.resources[0].name, "service");
    }

    #[test]
    fn resolve_view_hides_by_name_and_id_patterns() {
        let mut config = Config::default();
        let mut view = ViewConfig::default();
        view.hide.push("^frontend$".into());
        view.hide.push("^docker:worker$".into());
        config.views.insert("default".into(), view);

        let snapshot = Snapshot {
            resources: vec![
                ResourceRecord {
                    id: "docker:frontend".into(),
                    kind: ResourceKind::Container,
                    runtime: RuntimeKind::Docker,
                    ..resource("frontend", HealthState::Healthy, RuntimeKind::Docker)
                },
                ResourceRecord {
                    id: "docker:worker".into(),
                    kind: ResourceKind::Container,
                    runtime: RuntimeKind::Docker,
                    ..resource("worker", HealthState::Healthy, RuntimeKind::Docker)
                },
                resource("api", HealthState::Healthy, RuntimeKind::Host),
            ],
            ..Snapshot::default()
        };

        let resolved = resolve_view(&config, None, &snapshot);
        assert_eq!(resolved.resources.len(), 1);
        assert_eq!(resolved.resources[0].name, "api");
    }

    #[test]
    fn render_tmux_status_line_covers_idle_warn_and_base_issue_states() {
        let mut config = Config::default();
        config.views.insert(
            "default".into(),
            ViewConfig {
                status_bar: StatusBarConfig {
                    template: "{issues}:{warning_sources}".into(),
                    show_empty: false,
                    ..StatusBarConfig::default()
                },
                ..ViewConfig::default()
            },
        );

        let idle = render_tmux_status_line(&resolve_view(&config, None, &Snapshot::default()));
        assert!(idle.contains("giggity idle"));

        let mut healthy_view = config.active_view(None);
        healthy_view.status_bar.show_empty = true;
        config.views.insert("default".into(), healthy_view);
        let healthy = Snapshot {
            resources: vec![resource("api", HealthState::Healthy, RuntimeKind::Host)],
            ..Snapshot::default()
        };
        let healthy_render = render_tmux_status_line(&resolve_view(&config, None, &healthy));
        assert!(healthy_render.contains("#[fg=white]none#[fg=white]"));

        let warning_snapshot = Snapshot {
            resources: vec![resource("api", HealthState::Starting, RuntimeKind::Host)],
            warnings: vec![crate::model::CollectorWarning {
                source: "podman".into(),
                message: "socket refused".into(),
            }],
            ..Snapshot::default()
        };
        let warning_render =
            render_tmux_status_line(&resolve_view(&config, None, &warning_snapshot));
        assert!(warning_render.contains("#[fg=yellow]"));
        assert!(warning_render.contains("podman"));

        let degraded_snapshot = Snapshot {
            resources: vec![resource("api", HealthState::Degraded, RuntimeKind::Host)],
            ..Snapshot::default()
        };
        let degraded_render =
            render_tmux_status_line(&resolve_view(&config, None, &degraded_snapshot));
        assert!(degraded_render.contains("#[fg=yellow]"));
    }

    #[test]
    fn source_toggles_filters_all_runtime_kinds() {
        let mut config = Config::default();
        config.sources.docker = false;
        config.sources.podman = false;
        config.sources.nerdctl = false;
        config.sources.kubernetes = false;
        config.sources.host_listeners = false;
        config.sources.launchd = true;
        config.sources.systemd = true;

        let snapshot = Snapshot {
            resources: vec![
                resource("docker", HealthState::Healthy, RuntimeKind::Docker),
                resource("podman", HealthState::Healthy, RuntimeKind::Podman),
                resource("nerd", HealthState::Healthy, RuntimeKind::Nerdctl),
                resource("host", HealthState::Healthy, RuntimeKind::Host),
                ResourceRecord {
                    kind: ResourceKind::LaunchdUnit,
                    runtime: RuntimeKind::Launchd,
                    ..resource("launchd", HealthState::Healthy, RuntimeKind::Launchd)
                },
                ResourceRecord {
                    kind: ResourceKind::SystemdUnit,
                    runtime: RuntimeKind::Systemd,
                    ..resource("systemd", HealthState::Healthy, RuntimeKind::Systemd)
                },
            ],
            ..Snapshot::default()
        };

        let resolved = resolve_view(&config, None, &snapshot);
        assert_eq!(
            resolved
                .resources
                .iter()
                .map(|resource| resource.name.as_str())
                .collect::<Vec<_>>(),
            vec!["launchd", "systemd"]
        );
    }

    #[test]
    fn view_level_source_overrides_take_precedence_over_global_sources() {
        let mut config = Config::default();
        config.sources.docker = false;
        config.views.insert(
            "default".into(),
            ViewConfig {
                sources: Some(crate::config::SourceToggles {
                    docker: true,
                    podman: false,
                    nerdctl: false,
                    kubernetes: false,
                    host_listeners: false,
                    launchd: false,
                    systemd: false,
                }),
                ..ViewConfig::default()
            },
        );
        let snapshot = Snapshot {
            resources: vec![
                resource("api", HealthState::Healthy, RuntimeKind::Docker),
                resource("host", HealthState::Healthy, RuntimeKind::Host),
            ],
            ..Snapshot::default()
        };

        let resolved = resolve_view(&config, None, &snapshot);
        assert_eq!(resolved.resources.len(), 1);
        assert_eq!(resolved.resources[0].runtime, RuntimeKind::Docker);
    }

    #[test]
    fn match_helpers_reject_mismatches_and_invalid_regexes() {
        let mut resource = resource("api", HealthState::Healthy, RuntimeKind::Docker);
        resource.kind = ResourceKind::Container;

        assert!(matches_include(&[], &resource));
        assert!(!matches_any(
            &[MatchRule {
                any_regex: Some("[".into()),
                ..MatchRule::default()
            }],
            &resource
        ));
        assert!(!matches_rule(
            &MatchRule {
                kind: Some(vec![ResourceKind::HostProcess]),
                ..MatchRule::default()
            },
            &resource
        ));
        assert!(!matches_rule(
            &MatchRule {
                state: Some(vec![HealthState::Crashed]),
                ..MatchRule::default()
            },
            &resource
        ));
        assert!(!matches_rule(
            &MatchRule {
                ports: Some(vec![9999]),
                ..MatchRule::default()
            },
            &resource
        ));
        assert!(!matches_rule(
            &MatchRule {
                labels: BTreeMap::from([("team".into(), "ops".into())]),
                ..MatchRule::default()
            },
            &resource
        ));
    }

    #[test]
    fn sorting_grouping_and_summary_cover_all_modes() {
        let mut config = Config::default();
        let mut docker = ResourceRecord {
            id: "docker:web".into(),
            kind: ResourceKind::Container,
            runtime: RuntimeKind::Docker,
            labels: BTreeMap::from([("com.docker.compose.project".into(), "stack".into())]),
            last_changed: Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 3).unwrap(),
            ..resource("web", HealthState::Crashed, RuntimeKind::Docker)
        };
        docker.project = Some("stack".into());
        docker.metadata.insert("domain".into(), "system".into());
        let host = ResourceRecord {
            last_changed: Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 2).unwrap(),
            ..resource("api", HealthState::Degraded, RuntimeKind::Host)
        };
        let kubernetes = ResourceRecord {
            kind: ResourceKind::KubernetesPod,
            runtime: RuntimeKind::Kubernetes,
            project: Some("dev".into()),
            state: HealthState::Healthy,
            metadata: BTreeMap::from([("namespace".into(), "dev".into())]),
            last_changed: Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 6).unwrap(),
            ..resource("pod-1", HealthState::Healthy, RuntimeKind::Kubernetes)
        };
        let launchd = ResourceRecord {
            kind: ResourceKind::LaunchdUnit,
            runtime: RuntimeKind::Launchd,
            project: None,
            state: HealthState::Starting,
            metadata: BTreeMap::from([("domain".into(), "user".into())]),
            last_changed: Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 1).unwrap(),
            ..resource("agent", HealthState::Starting, RuntimeKind::Launchd)
        };
        let stopped = ResourceRecord {
            state: HealthState::Stopped,
            last_changed: Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 4).unwrap(),
            ..resource("cache", HealthState::Stopped, RuntimeKind::Host)
        };
        let unknown = ResourceRecord {
            state: HealthState::Unknown,
            last_changed: Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 5).unwrap(),
            ..resource("mystery", HealthState::Unknown, RuntimeKind::Podman)
        };
        let snapshot = Snapshot {
            resources: vec![host, docker, kubernetes, launchd, stopped, unknown],
            warnings: vec![
                crate::model::CollectorWarning {
                    source: "podman".into(),
                    message: "failed".into(),
                },
                crate::model::CollectorWarning {
                    source: "nerdctl".into(),
                    message: "failed".into(),
                },
            ],
            ..Snapshot::default()
        };

        for sorting in [
            SortKey::Severity,
            SortKey::Name,
            SortKey::LastChange,
            SortKey::Runtime,
            SortKey::Port,
        ] {
            let mut view = ViewConfig {
                sorting,
                pinned: vec!["api".into()],
                ..ViewConfig::default()
            };
            view.status_bar.max_issue_names = 10;
            config.views.insert("default".into(), view.clone());
            let resolved = resolve_view(&config, None, &snapshot);
            assert_eq!(resolved.resources.first().expect("first").name, "api");
            assert_eq!(resolved.summary.total, 6);
            assert_eq!(resolved.summary.healthy, 1);
            assert_eq!(resolved.summary.starting, 1);
            assert_eq!(resolved.summary.degraded, 1);
            assert_eq!(resolved.summary.crashed, 1);
            assert_eq!(resolved.summary.stopped, 1);
            assert_eq!(resolved.summary.unknown, 1);
            assert_eq!(resolved.summary.collector_warnings, 2);
            assert_eq!(
                resolved.summary.warning_sources,
                vec!["nerdctl".to_string(), "podman".to_string()]
            );
            assert_eq!(
                resolved.summary.latest_change,
                Some(Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 6).unwrap())
            );
        }

        for grouping in [
            GroupBy::Severity,
            GroupBy::Runtime,
            GroupBy::Project,
            GroupBy::Namespace,
            GroupBy::ComposeStack,
            GroupBy::UnitDomain,
            GroupBy::None,
        ] {
            config.views.insert(
                "default".into(),
                ViewConfig {
                    grouping: grouping.clone(),
                    ..ViewConfig::default()
                },
            );
            let resolved = resolve_view(&config, None, &snapshot);
            assert!(!resolved.grouped.is_empty());
        }
    }

    proptest! {
        #[test]
        fn issue_summary_never_exceeds_limit(limit in 1usize..4, size in 1usize..8) {
            let mut config = Config::default();
            let mut view = config.active_view(None);
            view.status_bar.max_issue_names = limit;
            config.views.insert("default".into(), view.clone());

            let resources = (0..size)
                .map(|idx| resource(&format!("svc-{idx}"), HealthState::Degraded, RuntimeKind::Docker))
                .collect();
            let snapshot = Snapshot {
                resources,
                ..Snapshot::default()
            };

            let resolved = resolve_view(&config, None, &snapshot);
            prop_assert!(resolved.summary.issues.len() <= limit);
        }
    }
}
