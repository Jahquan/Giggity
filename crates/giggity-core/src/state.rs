use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::model::{HealthState, RecentEvent, ResourceKind, ResourceRecord, Snapshot};

#[derive(Debug, Clone)]
pub struct StateEngine {
    host_ttl: Duration,
    history_limit: usize,
    previous: BTreeMap<String, ResourceRecord>,
    events: VecDeque<RecentEvent>,
}

impl StateEngine {
    pub fn new(host_ttl: Duration) -> Self {
        Self {
            host_ttl,
            history_limit: 200,
            previous: BTreeMap::new(),
            events: VecDeque::new(),
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
            match self.previous.get(&resource.id) {
                Some(previous) if previous.state == resource.state => {
                    resource.last_changed = previous.last_changed;
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
                }
            }
            next.insert(resource.id.clone(), resource.clone());
            materialized.push(resource);
        }

        let previous_values: Vec<_> = self.previous.values().cloned().collect();
        for previous in &previous_values {
            if next.contains_key(&previous.id) {
                continue;
            }

            let retained = self.retain_disappeared(previous, generated_at);
            if let Some(resource) = retained {
                next.insert(resource.id.clone(), resource.clone());
                materialized.push(resource);
            }
        }

        materialized.sort_by_key(|resource| {
            (
                std::cmp::Reverse(resource.state.severity()),
                resource.name.to_lowercase(),
            )
        });
        self.previous = next;
        self.prune_events(generated_at);

        Snapshot {
            api_version: 1,
            generated_at,
            resources: materialized,
            events: self.events.iter().cloned().collect(),
            warnings,
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
                    ..previous.clone()
                })
            }
            _ if age > self.host_ttl.saturating_mul(5) => None,
            _ => Some(previous.clone()),
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use chrono::{TimeZone, Utc};

    use crate::model::{HealthState, ResourceKind, ResourceRecord, RuntimeKind};

    use super::StateEngine;

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
        }
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
}
