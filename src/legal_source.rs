//! Stable legal-source identity and the registry shared by retrieval and acquisition.
//!
//! Public tools resolve exactly one source through this registry. Maintainer source
//! updates use the same registered adapter, preventing retrieval and acquisition
//! source lists from drifting apart.

use crate::source_catalog::production_registrations;
use crate::source_update::SourceAcquisition;
#[cfg(test)]
use anyhow::bail;
use anyhow::Result;
pub(crate) use legal_model::{SourceDescriptor, SourceId};
use std::collections::BTreeMap;
use std::sync::OnceLock;

pub(crate) trait LegalSource: Send + Sync {
    fn descriptor(&self) -> &SourceDescriptor;

    fn acquisition(&self) -> Option<&dyn SourceAcquisition> {
        None
    }
}

struct RegisteredSource {
    descriptor: SourceDescriptor,
    acquisition: &'static dyn SourceAcquisition,
}

impl LegalSource for RegisteredSource {
    fn descriptor(&self) -> &SourceDescriptor {
        &self.descriptor
    }

    fn acquisition(&self) -> Option<&dyn SourceAcquisition> {
        Some(self.acquisition)
    }
}

pub(crate) struct SourceRegistry {
    sources: BTreeMap<SourceId, Box<dyn LegalSource>>,
}

impl SourceRegistry {
    fn production() -> Self {
        let sources = production_registrations()
            .expect("valid production source catalogue")
            .into_iter()
            .map(|registration| {
                let source: Box<dyn LegalSource> = Box::new(RegisteredSource {
                    descriptor: registration.descriptor,
                    acquisition: registration.acquisition,
                });
                (source.descriptor().id.clone(), source)
            })
            .collect();
        Self { sources }
    }

    #[cfg(test)]
    pub(crate) fn try_new(sources: Vec<Box<dyn LegalSource>>) -> Result<Self> {
        let mut registered = BTreeMap::new();
        for source in sources {
            let source_id = source.descriptor().id.clone();
            if registered.insert(source_id.clone(), source).is_some() {
                bail!("duplicate legal source `{source_id}`");
            }
        }
        Ok(Self {
            sources: registered,
        })
    }

    pub(crate) fn source(&self, source_id: &SourceId) -> Result<&dyn LegalSource> {
        self.sources.get(source_id).map(Box::as_ref).ok_or_else(|| {
            anyhow::anyhow!(
                "unknown legal source `{source_id}`; available sources: {}",
                self.source_ids().join(", ")
            )
        })
    }

    pub(crate) fn source_ids(&self) -> Vec<&str> {
        self.sources
            .keys()
            .map(|source_id| source_id.as_str())
            .collect()
    }

    pub(crate) fn descriptors(&self) -> Vec<SourceDescriptor> {
        self.sources
            .values()
            .map(|source| source.descriptor().clone())
            .collect()
    }
}

pub(crate) fn source_registry() -> &'static SourceRegistry {
    static REGISTRY: OnceLock<SourceRegistry> = OnceLock::new();
    REGISTRY.get_or_init(SourceRegistry::production)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct FakeSource(SourceDescriptor);

    impl LegalSource for FakeSource {
        fn descriptor(&self) -> &SourceDescriptor {
            &self.0
        }
    }

    fn fake(id: &str, name: &str) -> Box<dyn LegalSource> {
        Box::new(FakeSource(
            SourceDescriptor::new(id.parse().expect("valid test source id"), name)
                .expect("valid test source descriptor"),
        ))
    }

    #[test]
    fn registry_resolves_only_explicit_sources() -> Result<()> {
        let registry = SourceRegistry::try_new(vec![
            fake("ato", "ATO"),
            fake("wa-legislation", "WA legislation"),
        ])?;
        assert!(registry.source(&"wa-legislation".parse()?).is_ok());
        assert_eq!(registry.source_ids(), vec!["ato", "wa-legislation"]);
        Ok(())
    }

    #[test]
    fn registry_rejects_unknown_duplicate_and_invalid_sources() {
        let duplicate = SourceRegistry::try_new(vec![fake("ato", "ATO"), fake("ato", "duplicate")]);
        assert!(duplicate.is_err());

        assert!("WA".parse::<SourceId>().is_err());

        let registry = SourceRegistry::try_new(vec![fake("ato", "ATO"), fake("wa", "WA")])
            .unwrap_or_else(|error| panic!("creating test registry: {error}"));
        let error = match registry.source(&"sa".parse().expect("valid source")) {
            Ok(_) => panic!("unknown source must fail"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("available sources: ato, wa"));
    }

    #[test]
    fn production_registry_contains_the_complete_source_set() -> Result<()> {
        let registry = source_registry();
        assert_eq!(
            registry.source_ids(),
            vec![
                "ato",
                "federal-court",
                "frl",
                "high-court",
                "nsw-caselaw",
                "nsw-legislation",
                "qld-legislation",
                "sa-legislation",
                "tas-legislation",
                "wa-legislation",
            ]
        );
        assert_eq!(registry.descriptors().len(), 10);
        for source_id in registry.source_ids() {
            let source: SourceId = source_id.parse()?;
            assert!(registry.source(&source)?.acquisition().is_some());
        }
        Ok(())
    }
}
