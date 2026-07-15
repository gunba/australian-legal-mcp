//! Complete production source catalogue.

use crate::frl::{frl_descriptor, FRL_ACQUISITION};
use crate::official_sources;
use crate::source_update::{SourceAcquisition, ATO_ACQUISITION};
use anyhow::Result;
use legal_model::SourceDescriptor;
use legal_model::SourceId;
use legal_source_sdk::NormalizedDocument;
use std::path::Path;

pub(crate) const ATO_SOURCE_ID: &str = "ato";

pub(crate) struct SourceRegistration {
    pub(crate) descriptor: SourceDescriptor,
    pub(crate) acquisition: &'static dyn SourceAcquisition,
    normalized_documents: Option<NormalizedDocumentLoader>,
}

type NormalizedDocumentResults = Box<dyn Iterator<Item = Result<NormalizedDocument>>>;
type NormalizedDocumentLoader = fn(&SourceId, &Path) -> Result<NormalizedDocumentResults>;

pub(crate) fn production_registrations() -> Result<Vec<SourceRegistration>> {
    let mut registrations = vec![
        SourceRegistration {
            descriptor: SourceDescriptor::new(
                ATO_SOURCE_ID.parse()?,
                "Australian Taxation Office legal corpus",
            )?,
            acquisition: &ATO_ACQUISITION,
            normalized_documents: Some(crate::ato::normalized_document_results),
        },
        SourceRegistration {
            descriptor: frl_descriptor()?,
            acquisition: &FRL_ACQUISITION,
            normalized_documents: Some(load_frl_documents),
        },
    ];
    for descriptor in official_sources::descriptors()? {
        let acquisition =
            official_sources::acquisition_for(descriptor.id.as_str()).ok_or_else(|| {
                anyhow::anyhow!("source {} has no acquisition adapter", descriptor.id)
            })?;
        registrations.push(SourceRegistration {
            descriptor,
            acquisition,
            normalized_documents: Some(official_sources::normalized_document_results),
        });
    }
    Ok(registrations)
}

pub(crate) fn normalized_document_results(
    source: &SourceId,
    workspace: &Path,
) -> Result<NormalizedDocumentResults> {
    let registration = production_registrations()?
        .into_iter()
        .find(|registration| &registration.descriptor.id == source)
        .ok_or_else(|| anyhow::anyhow!("source `{source}` is not registered"))?;
    let loader = registration.normalized_documents.ok_or_else(|| {
        anyhow::anyhow!("source `{source}` owns a source-specific corpus builder")
    })?;
    loader(source, workspace)
}

fn load_frl_documents(source: &SourceId, workspace: &Path) -> Result<NormalizedDocumentResults> {
    if source.as_str() != crate::frl::FRL_SOURCE_ID {
        anyhow::bail!("FRL workspace loader cannot load source `{source}`");
    }
    crate::frl::normalized_document_results(workspace)
}
