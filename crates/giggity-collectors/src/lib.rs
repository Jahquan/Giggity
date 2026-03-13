use anyhow::Result;
use async_trait::async_trait;
use giggity_core::config::Config;
use giggity_core::model::{CollectorWarning, ResourceRecord};

mod command;
pub mod containers;
pub mod host;
pub mod service_managers;

#[derive(Debug, Default, Clone)]
pub struct CollectionOutput {
    pub resources: Vec<ResourceRecord>,
    pub warnings: Vec<CollectorWarning>,
}

impl CollectionOutput {
    pub fn merge(&mut self, other: Self) {
        self.resources.extend(other.resources);
        self.warnings.extend(other.warnings);
    }
}

#[async_trait]
pub trait CollectorProvider: Send + Sync {
    async fn collect(&self, config: &Config) -> Result<CollectionOutput>;
}

#[derive(Debug, Default)]
pub struct SystemCollector;

#[async_trait]
impl CollectorProvider for SystemCollector {
    async fn collect(&self, config: &Config) -> Result<CollectionOutput> {
        let mut output = CollectionOutput::default();
        output.merge(containers::collect(config).await);
        output.merge(host::collect(config).await);
        output.merge(service_managers::collect(config).await);
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::{CollectionOutput, CollectorProvider, SystemCollector};
    use giggity_core::config::Config;
    use giggity_core::model::CollectorWarning;

    #[test]
    fn collection_output_merge_appends_resources_and_warnings() {
        let mut left = CollectionOutput::default();
        left.warnings.push(CollectorWarning {
            source: "left".into(),
            message: "a".into(),
        });
        let mut right = CollectionOutput::default();
        right.warnings.push(CollectorWarning {
            source: "right".into(),
            message: "b".into(),
        });

        left.merge(right);
        assert_eq!(left.warnings.len(), 2);
        assert_eq!(left.warnings[1].source, "right");
    }

    #[tokio::test]
    async fn system_collector_respects_disabled_sources() {
        let mut config = Config::default();
        config.sources.docker = false;
        config.sources.podman = false;
        config.sources.nerdctl = false;
        config.sources.host_listeners = false;
        config.sources.launchd = false;
        config.sources.systemd = false;

        let output = SystemCollector.collect(&config).await.expect("collect");
        assert!(output.resources.is_empty());
        assert!(output.warnings.is_empty());
    }
}
