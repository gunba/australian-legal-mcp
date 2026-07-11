//! Stable legal-source identity and the registry shared by retrieval and acquisition.
//!
//! Public tools resolve exactly one source through this registry. Maintainer source
//! updates use the same registered adapter, preventing retrieval and acquisition
//! source lists from drifting apart.

use crate::frl::{frl_descriptor, FRL_ACQUISITION};
use crate::source_update::{SourceAcquisition, ATO_ACQUISITION};
use anyhow::{bail, Result};
pub(crate) use legal_model::{SourceDescriptor, SourceId};
use std::collections::BTreeMap;
use std::sync::OnceLock;

pub(crate) const DEFAULT_SOURCE_ID: &str = "ato";

pub(crate) trait LegalSource: Send + Sync {
    fn descriptor(&self) -> &SourceDescriptor;

    fn acquisition(&self) -> Option<&dyn SourceAcquisition> {
        None
    }
}

#[derive(Debug)]
struct AtoSource {
    descriptor: SourceDescriptor,
}

impl LegalSource for AtoSource {
    fn descriptor(&self) -> &SourceDescriptor {
        &self.descriptor
    }

    fn acquisition(&self) -> Option<&dyn SourceAcquisition> {
        Some(&ATO_ACQUISITION)
    }
}

#[derive(Debug)]
struct FrlSource {
    descriptor: SourceDescriptor,
}

impl LegalSource for FrlSource {
    fn descriptor(&self) -> &SourceDescriptor {
        &self.descriptor
    }

    fn acquisition(&self) -> Option<&dyn SourceAcquisition> {
        Some(&FRL_ACQUISITION)
    }
}

pub(crate) struct SourceRegistry {
    default: SourceId,
    sources: BTreeMap<SourceId, Box<dyn LegalSource>>,
}

impl SourceRegistry {
    fn production() -> Self {
        let ato: Box<dyn LegalSource> = Box::new(AtoSource {
            descriptor: SourceDescriptor::new(
                DEFAULT_SOURCE_ID.parse().expect("valid default source id"),
                "Australian Taxation Office legal corpus",
            )
            .expect("valid ATO source descriptor"),
        });
        let frl: Box<dyn LegalSource> = Box::new(FrlSource {
            descriptor: frl_descriptor().expect("valid FRL source descriptor"),
        });
        let default = ato.descriptor().id.clone();
        let sources = [ato, frl]
            .into_iter()
            .map(|source| (source.descriptor().id.clone(), source))
            .collect();
        Self { default, sources }
    }

    #[cfg(test)]
    pub(crate) fn try_new(default: SourceId, sources: Vec<Box<dyn LegalSource>>) -> Result<Self> {
        let mut registered = BTreeMap::new();
        for source in sources {
            let source_id = source.descriptor().id.clone();
            if registered.insert(source_id.clone(), source).is_some() {
                bail!("duplicate legal source `{source_id}`");
            }
        }
        if !registered.contains_key(&default) {
            bail!("default legal source `{default}` is not registered");
        }
        Ok(Self {
            default,
            sources: registered,
        })
    }

    pub(crate) fn resolve(&self, requested: Option<&str>) -> Result<SourceId> {
        let requested = requested.unwrap_or(self.default.as_str());
        let Some((source_id, _)) = self
            .sources
            .iter()
            .find(|(source_id, _)| source_id.as_str() == requested)
        else {
            let available = self.source_ids().join(", ");
            bail!("unknown legal source `{requested}`; available sources: {available}");
        };
        Ok(source_id.clone())
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
    fn registry_resolves_one_explicit_source_or_the_default() -> Result<()> {
        let registry = SourceRegistry::try_new(
            "ato".parse()?,
            vec![fake("ato", "ATO"), fake("wa-legislation", "WA legislation")],
        )?;
        assert_eq!(registry.resolve(None)?.as_str(), "ato");
        assert_eq!(
            registry.resolve(Some("wa-legislation"))?.as_str(),
            "wa-legislation"
        );
        assert_eq!(registry.source_ids(), vec!["ato", "wa-legislation"]);
        Ok(())
    }

    #[test]
    fn registry_rejects_unknown_duplicate_invalid_and_missing_default_sources() {
        let duplicate = SourceRegistry::try_new(
            "ato".parse().expect("valid source"),
            vec![fake("ato", "ATO"), fake("ato", "duplicate")],
        );
        assert!(duplicate.is_err());

        assert!("WA".parse::<SourceId>().is_err());

        let missing_default =
            SourceRegistry::try_new("ato".parse().expect("valid source"), vec![fake("wa", "WA")]);
        assert!(missing_default.is_err());

        let registry = SourceRegistry::try_new(
            "ato".parse().expect("valid source"),
            vec![fake("ato", "ATO"), fake("wa", "WA")],
        )
        .unwrap_or_else(|error| panic!("creating test registry: {error}"));
        let error = registry
            .resolve(Some("sa"))
            .expect_err("unknown source must fail");
        assert!(error.to_string().contains("available sources: ato, wa"));
    }

    #[test]
    fn production_registry_contains_the_complete_source_set() -> Result<()> {
        let registry = source_registry();
        assert_eq!(registry.source_ids(), vec!["ato", "frl"]);
        assert_eq!(registry.resolve(None)?.as_str(), DEFAULT_SOURCE_ID);
        let descriptors = registry.descriptors();
        let descriptor = &descriptors[0];
        assert_eq!(
            descriptor.display_name,
            "Australian Taxation Office legal corpus"
        );
        let ato: SourceId = DEFAULT_SOURCE_ID.parse()?;
        assert!(registry.source(&ato)?.acquisition().is_some());
        let frl: SourceId = "frl".parse()?;
        assert!(registry.source(&frl)?.acquisition().is_some());
        assert_eq!(
            serde_json::to_value(registry.descriptors())?,
            serde_json::json!([
                {
                    "id": "ato",
                    "display_name": "Australian Taxation Office legal corpus"
                },
                {
                    "id": "frl",
                    "display_name": "Federal Register of Legislation"
                }
            ])
        );
        Ok(())
    }
}
