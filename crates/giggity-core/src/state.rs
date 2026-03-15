use std::collections::{BTreeMap, HashMap, VecDeque};
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::model::{HealthState, RecentEvent, ResourceKind, ResourceRecord, Snapshot};

const FLAPPING_THRESHOLD: usize = 3;
const FLAPPING_WINDOW_SECS: i64 = 600;
const RESTART_WINDOW_SECS: i64 = 3600;

#[derive(Debug, Clone)]
pub struct StateEngine {
    host_ttl: Duration,
    history_limit: usize,
    previous: BTreeMap<String, ResourceRecord>,
    events: VecDeque<RecentEvent>,
    last_crash_at: Option<DateTime<Utc>>,
}

impl StateEngine {
    pub fn new(host_ttl: Duration) -> Self {
        Self {
            host_ttl,
            history_limit: 200,
            previous: BTreeMap::new(),
            events: VecDeque::new(),
            last_crash_at: None,
        }
    }

    pub fn ingest(
        &mut self,
        generated_at: DateTime<Utc>,
        resources: Vec<ResourceRecord>,
        warnings: Vec<crate::model::CollectorWarning>,
    ) -> Snapshot {
        let mut next = BTreeMap::new();
        let mut materialized = Vec::new();

        for mut resource in resources {
            self.apply_state_tracking(&mut resource, generated_at);
            next.insert(resource.id.clone(), resource.clone());
            materialized.push(resource);
        }

        let previous_values: Vec<_> = self.previous.values().cloned().collect();
        for previous in &previous_values {
            if next.contains_key(&previous.id) {
                continue;
            }
            if let Some(resource) = self.retain_disappeared(previous, generated_at) {
                next.insert(resource.id.clone(), resource.clone());
                materialized.push(resource);
            }
        }

        detect_port_conflicts(&mut materialized);
        self.annotate_restart_frequency(&mut materialized, generated_at);

        materialized.sort_by_key(|resource| {
            (
                std::cmp::Reverse(resource.state.severity()),
                resource.name.to_lowercase(),
            )
        });
        self.previous = next;
        self.prune_events(generated_at);

        for event in &self.events {
            if event.to == HealthState::Crashed || event.to == HealthState::Degraded {
                match self.last_crash_at {
                    Some(prev) if event.timestamp > prev => {
                        self.last_crash_at = Some(event.timestamp);
                    }
                    None => {
                        self.last_crash_at = Some(event.timestamp);
                    }
                    _ => {}
                }
            }
        }

        Snapshot {
            api_version: 1,
            generated_at,
            resources: materialized,
            events: self.events.iter().cloned().collect(),
            warnings,
            last_crash_at: self.last_crash_at,
        }
    }

    fn apply_state_tracking(&mut self, resource: &mut ResourceRecord, generated_at: DateTime<Utc>) {
        match self.previous.get(&resource.id) {
            Some(previous) if previous.state == resource.state => {
                resource.last_changed = previous.last_changed;
                resource.state_since = previous.state_since;
            }
            Some(previous) => {
                self.push_event(RecentEvent {
                    resource_id: resource.id.clone(),
                    resource_name: resource.name.clone(),
                    from: Some(previous.state),
                    to: resource.state,
                    timestamp: generated_at,
                    cause: Some("state transition".into()),
                });
                resource.last_changed = generated_at;
                resource.state_since = generated_at;
            }
            None => {
                self.push_event(RecentEvent {
                    resource_id: resource.id.clone(),
                    resource_name: resource.name.clone(),
                    from: None,
                    to: resource.state,
                    timestamp: generated_at,
                    cause: Some("new resource discovered".into()),
                });
                resource.last_changed = generated_at;
                resource.state_since = generated_at;
            }
        }
    }

    fn retain_disappeared(
        &mut self,
        previous: &ResourceRecord,
        generated_at: DateTime<Utc>,
    ) -> Option<ResourceRecord> {
        let age = generated_at
            .signed_duration_since(previous.last_changed)
            .to_std()
            .unwrap_or_default();

        match previous.kind {
            ResourceKind::HostProcess if age > self.host_ttl => None,
            ResourceKind::HostProcess => {
                if previous.state != HealthState::Stopped {
                    self.push_event(RecentEvent {
                        resource_id: previous.id.clone(),
                        resource_name: previous.name.clone(),
                        from: Some(previous.state),
                        to: HealthState::Stopped,
                        timestamp: generated_at,
                        cause: Some("host process disappeared".into()),
                    });
                }
                Some(ResourceRecord {
                    state: HealthState::Stopped,
                    runtime_status: Some("disappeared".into()),
                    state_since: generated_at,
                    ..previous.clone()
                })
            }
            _ if age > self.host_ttl.saturating_mul(5) => None,
            _ => Some(previous.clone()),
        }
    }

    fn annotate_restart_frequency(&self, resources: &mut [ResourceRecord], now: DateTime<Utc>) {
        let hour_cutoff = now - chrono::Duration::seconds(RESTART_WINDOW_SECS);
        let flap_cutoff = now - chrono::Duration::seconds(FLAPPING_WINDOW_SECS);

        let mut hourly_counts: HashMap<&str, usize> = HashMap::new();
        let mut flap_counts: HashMap<&str, usize> = HashMap::new();

        for event in &self.events {
            if !is_restart_event(event) {
                continue;
            }
            if event.timestamp >= hour_cutoff {
                *hourly_counts.entry(&event.resource_id).or_default() += 1;
            }
            if event.timestamp >= flap_cutoff {
                *flap_counts.entry(&event.resource_id).or_default() += 1;
            }
        }

        for resource in resources.iter_mut() {
            if let Some(&count) = hourly_counts.get(resource.id.as_str()) {
                if count > 0 {
                    resource
                        .metadata
                        .insert("restart_count_1h".into(), count.to_string());
                }
            }
            if let Some(&count) = flap_counts.get(resource.id.as_str()) {
                if count >= FLAPPING_THRESHOLD {
                    resource
                        .metadata
                        .insert("restart_flapping".into(), "true".into());
                }
            }
        }
    }

    fn push_event(&mut self, event: RecentEvent) {
        self.events.push_front(event);
        while self.events.len() > self.history_limit {
            self.events.pop_back();
        }
    }

    fn prune_events(&mut self, now: DateTime<Utc>) {
        let max_age = self.host_ttl.saturating_mul(10);
        self.events.retain(|event| {
            now.signed_duration_since(event.timestamp)
                .to_std()
                .map(|age| age <= max_age)
                .unwrap_or(false)
        });
    }
}

fn is_restart_event(event: &RecentEvent) -> bool {
    event.from.is_some()
        && matches!(
            event.to,
            HealthState::Starting | HealthState::Healthy | HealthState::Crashed
        )
}

fn detect_port_conflicts(resources: &mut [ResourceRecord]) {
    let mut port_owners: HashMap<(u16, String), Vec<usize>> = HashMap::new();

    for (idx, resource) in resources.iter().enumerate() {
        for port in &resource.ports {
            let key = (port.host_port, port.protocol.clone());
            port_owners.entry(key).or_default().push(idx);
        }
    }

    let conflicts: Vec<(usize, String)> = port_owners
        .values()
        .filter(|indices| indices.len() > 1)
        .flat_map(|indices| {
            indices.iter().flat_map(|&idx| {
                let others: Vec<String> = indices
                    .iter()
                    .filter(|&&other| other != idx)
                    .map(|&other| resources[other].id.clone())
                    .collect();
                others.into_iter().map(move |other_id| (idx, other_id))
            })
        })
        .collect();

    for (idx, other_id) in conflicts {
        resources[idx]
            .metadata
            .insert("port_conflict".into(), "true".into());
        resources[idx]
            .metadata
            .entry("port_conflict_with".into())
            .and_modify(|existing| {
                existing.push(',');
                existing.push_str(&other_id);
            })
            .or_insert(other_id);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use chrono::{TimeZone, Utc};

    use crate::model::{
        HealthState, PortBinding, RecentEvent, ResourceKind, ResourceRecord, RuntimeKind,
    };

    use super::{StateEngine, detect_port_conflicts, is_restart_event};

    fn resource(id: &str, state: HealthState) -> ResourceRecord {
        ResourceRecord {
            id: id.into(),
            kind: ResourceKind::HostProcess,
            runtime: RuntimeKind::Host,
            project: None,
            name: id.into(),
            state,
            runtime_status: None,
            ports: Vec::new(),
            labels: BTreeMap::new(),
            urls: Vec::new(),
            metadata: BTreeMap::new(),
            last_changed: Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap(),
            state_since: Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap(),
        }
    }

    fn resource_with_port(id: &str, state: HealthState, port: u16) -> ResourceRecord {
        let mut r = resource(id, state);
        r.ports.push(PortBinding {
            host_ip: None,
            host_port: port,
            container_port: Some(port),
            protocol: "tcp".into(),
        });
        r
    }

    #[test]
    fn remembers_state_transitions_and_disappeared_hosts() {
        let mut engine = StateEngine::new(Duration::from_secs(30));
        let first = engine.ingest(
            Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap(),
            vec![resource("api", HealthState::Healthy)],
            Vec::new(),
        );
        assert_eq!(first.resources[0].state, HealthState::Healthy);

        let second = engine.ingest(
            Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 10).unwrap(),
            vec![resource("api", HealthState::Degraded)],
            Vec::new(),
        );
        assert_eq!(second.resources[0].state, HealthState::Degraded);
        assert_eq!(second.events[0].to, HealthState::Degraded);

        let third = engine.ingest(
            Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 20).unwrap(),
            Vec::new(),
            Vec::new(),
        );
        assert_eq!(third.resources[0].state, HealthState::Stopped);
    }

    #[test]
    fn retains_last_changed_for_steady_state_and_prunes_old_entries() {
        let mut engine = StateEngine::new(Duration::from_secs(10));
        let first_time = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
        let second_time = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 5).unwrap();
        let stale_time = Utc.with_ymd_and_hms(2025, 1, 1, 0, 2, 0).unwrap();

        let first = engine.ingest(
            first_time,
            vec![resource("api", HealthState::Healthy)],
            Vec::new(),
        );
        let second = engine.ingest(
            second_time,
            vec![resource("api", HealthState::Healthy)],
            Vec::new(),
        );
        assert_eq!(
            first.resources[0].last_changed,
            second.resources[0].last_changed
        );

        let fourth = engine.ingest(stale_time, Vec::new(), Vec::new());
        assert!(fourth.events.is_empty());
        assert!(fourth.resources.is_empty());
    }

    #[test]
    fn non_host_resources_are_retained_then_evicted_after_extended_ttl() {
        let mut engine = StateEngine::new(Duration::from_secs(10));
        let mut container = resource("web", HealthState::Healthy);
        container.kind = ResourceKind::Container;
        container.runtime = RuntimeKind::Docker;

        let first_time = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
        let retain_time = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 30).unwrap();
        let evict_time = Utc.with_ymd_and_hms(2025, 1, 1, 0, 1, 0).unwrap();

        let first = engine.ingest(first_time, vec![container.clone()], Vec::new());
        assert_eq!(first.resources.len(), 1);

        let retained = engine.ingest(retain_time, Vec::new(), Vec::new());
        assert_eq!(retained.resources.len(), 1);
        assert_eq!(retained.resources[0].name, "web");

        let evicted = engine.ingest(evict_time, Vec::new(), Vec::new());
        assert!(evicted.resources.is_empty());
    }

    #[test]
    fn ingested_resources_sort_by_severity_and_name() {
        let mut engine = StateEngine::new(Duration::from_secs(60));
        let now = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
        let snapshot = engine.ingest(
            now,
            vec![
                resource("zeta", HealthState::Healthy),
                resource("alpha", HealthState::Crashed),
                resource("beta", HealthState::Starting),
            ],
            Vec::new(),
        );

        let names: Vec<_> = snapshot
            .resources
            .iter()
            .map(|resource| resource.name.as_str())
            .collect();
        assert_eq!(names, vec!["alpha", "beta", "zeta"]);
    }

    #[test]
    fn event_history_is_trimmed_to_limit() {
        let mut engine = StateEngine::new(Duration::from_secs(60));
        let now = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
        let resources = (0..205)
            .map(|index| resource(&format!("svc-{index}"), HealthState::Healthy))
            .collect();

        let snapshot = engine.ingest(now, resources, Vec::new());
        assert_eq!(snapshot.events.len(), 200);
        assert!(
            snapshot
                .events
                .iter()
                .all(|event| event.resource_id != "svc-0")
        );
        assert!(
            snapshot
                .events
                .iter()
                .any(|event| event.resource_id == "svc-204")
        );
    }

    #[test]
    fn state_since_persists_across_steady_state_ingestions() {
        let mut engine = StateEngine::new(Duration::from_secs(60));
        let t0 = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 10).unwrap();
        let t2 = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 20).unwrap();

        let first = engine.ingest(t0, vec![resource("api", HealthState::Healthy)], Vec::new());
        assert_eq!(first.resources[0].state_since, t0);

        let second = engine.ingest(t1, vec![resource("api", HealthState::Healthy)], Vec::new());
        assert_eq!(second.resources[0].state_since, t0);

        let third = engine.ingest(t2, vec![resource("api", HealthState::Degraded)], Vec::new());
        assert_eq!(third.resources[0].state_since, t2);
    }

    #[test]
    fn disappeared_host_gets_new_state_since() {
        let mut engine = StateEngine::new(Duration::from_secs(30));
        let t0 = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 10).unwrap();

        engine.ingest(t0, vec![resource("api", HealthState::Healthy)], Vec::new());
        let snapshot = engine.ingest(t1, Vec::new(), Vec::new());
        assert_eq!(snapshot.resources[0].state, HealthState::Stopped);
        assert_eq!(snapshot.resources[0].state_since, t1);
    }

    #[test]
    fn port_conflict_detection_marks_both_resources() {
        let mut resources = vec![
            resource_with_port("web", HealthState::Healthy, 8080),
            resource_with_port("api", HealthState::Healthy, 8080),
            resource_with_port("db", HealthState::Healthy, 5432),
        ];

        detect_port_conflicts(&mut resources);

        assert_eq!(resources[0].metadata.get("port_conflict").unwrap(), "true");
        assert!(
            resources[0]
                .metadata
                .get("port_conflict_with")
                .unwrap()
                .contains("api")
        );
        assert_eq!(resources[1].metadata.get("port_conflict").unwrap(), "true");
        assert!(
            resources[1]
                .metadata
                .get("port_conflict_with")
                .unwrap()
                .contains("web")
        );
        assert!(!resources[2].metadata.contains_key("port_conflict"));
    }

    #[test]
    fn port_conflict_not_triggered_for_different_ports() {
        let mut resources = vec![
            resource_with_port("web", HealthState::Healthy, 8080),
            resource_with_port("api", HealthState::Healthy, 9090),
        ];
        detect_port_conflicts(&mut resources);
        assert!(!resources[0].metadata.contains_key("port_conflict"));
        assert!(!resources[1].metadata.contains_key("port_conflict"));
    }

    #[test]
    fn port_conflict_integrates_with_ingest() {
        let mut engine = StateEngine::new(Duration::from_secs(60));
        let now = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
        let snapshot = engine.ingest(
            now,
            vec![
                resource_with_port("web", HealthState::Healthy, 8080),
                resource_with_port("api", HealthState::Healthy, 8080),
            ],
            Vec::new(),
        );
        let web = snapshot.resources.iter().find(|r| r.id == "web").unwrap();
        let api = snapshot.resources.iter().find(|r| r.id == "api").unwrap();
        assert_eq!(web.metadata["port_conflict"], "true");
        assert_eq!(api.metadata["port_conflict"], "true");
    }

    #[test]
    fn restart_frequency_tracking_counts_events_in_window() {
        let mut engine = StateEngine::new(Duration::from_secs(7200));
        let base = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();

        engine.ingest(
            base,
            vec![resource("svc", HealthState::Healthy)],
            Vec::new(),
        );

        for i in 1..=4 {
            let t = base + chrono::Duration::minutes(i * 2);
            engine.ingest(t, vec![resource("svc", HealthState::Crashed)], Vec::new());
            let t2 = t + chrono::Duration::seconds(5);
            engine.ingest(t2, vec![resource("svc", HealthState::Healthy)], Vec::new());
        }

        let final_time = base + chrono::Duration::minutes(9);
        let snapshot = engine.ingest(
            final_time,
            vec![resource("svc", HealthState::Healthy)],
            Vec::new(),
        );

        let svc = &snapshot.resources[0];
        assert!(svc.metadata.contains_key("restart_count_1h"));
        let count: usize = svc.metadata["restart_count_1h"].parse().unwrap();
        assert!(count >= 4);
        assert_eq!(svc.metadata.get("restart_flapping").unwrap(), "true");
    }

    #[test]
    fn restart_flapping_not_set_below_threshold() {
        let mut engine = StateEngine::new(Duration::from_secs(7200));
        let base = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();

        engine.ingest(
            base,
            vec![resource("svc", HealthState::Healthy)],
            Vec::new(),
        );

        let t1 = base + chrono::Duration::minutes(1);
        engine.ingest(t1, vec![resource("svc", HealthState::Crashed)], Vec::new());
        let t2 = t1 + chrono::Duration::seconds(5);
        engine.ingest(t2, vec![resource("svc", HealthState::Healthy)], Vec::new());

        let final_time = base + chrono::Duration::minutes(5);
        let snapshot = engine.ingest(
            final_time,
            vec![resource("svc", HealthState::Healthy)],
            Vec::new(),
        );

        let svc = &snapshot.resources[0];
        assert!(!svc.metadata.contains_key("restart_flapping"));
    }

    #[test]
    fn is_restart_event_classifies_correctly() {
        let restart = RecentEvent {
            resource_id: "svc".into(),
            resource_name: "svc".into(),
            from: Some(HealthState::Crashed),
            to: HealthState::Starting,
            timestamp: Utc::now(),
            cause: None,
        };
        assert!(is_restart_event(&restart));

        let new_discovery = RecentEvent {
            from: None,
            ..restart.clone()
        };
        assert!(!is_restart_event(&new_discovery));

        let stopped = RecentEvent {
            to: HealthState::Stopped,
            ..restart
        };
        assert!(!is_restart_event(&stopped));
    }
}
