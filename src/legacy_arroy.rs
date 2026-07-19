//! Maintenance-only model of the retired Arroy generation contract.
//!
//! Runtime generation parsing remains flat-int8-only. These types exist solely
//! for the two one-shot immutable generation derivations and deliberately match
//! the exact schema-10/schema-11 Arroy manifests produced before the hard cut.

use crate::config::LEGAL_DB_FILENAME;
use crate::semantic::EMBEDDING_MODEL_FILES;
use crate::source::{ManifestDb, ManifestFile, ModelInfo};
use crate::{EMBEDDING_DIM, EMBEDDING_MODEL_FINGERPRINT, EMBEDDING_MODEL_ID};
use anyhow::{bail, Context, Result};
use legal_model::SourceId;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

pub(crate) const ARROY_FORMAT: &str = "arroy-cosine-f32";
pub(crate) const ARROY_FORMAT_VERSION: u32 = 3;
pub(crate) const ARROY_LIBRARY: &str = "arroy";
pub(crate) const ARROY_LIBRARY_VERSION: &str = "0.6.4";
pub(crate) const ARROY_SEED: u64 = 0x4155_534c_414e_4e31;
pub(crate) const ARROY_RNG: &str = "chacha12-rand_chacha-0.3.1";
pub(crate) const ARROY_TREES: u32 = 16;
pub(crate) const ARROY_SPLIT_AFTER: u32 = 64;
pub(crate) const ARROY_ID_ENCODING: &str = "sqlite-chunk-id-u32";
pub(crate) const ARROY_METRIC: &str = "cosine-f32-candidates+dot-i8-rerank";

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct LegacyArroyManifest {
    pub(crate) schema_version: u32,
    pub(crate) index_version: String,
    pub(crate) created_at: String,
    pub(crate) min_client_version: String,
    pub(crate) model: ModelInfo,
    pub(crate) db: ManifestDb,
    pub(crate) ann: BTreeMap<SourceId, LegacyArroyAnn>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct LegacyArroyAnn {
    pub(crate) source_id: SourceId,
    pub(crate) format: String,
    pub(crate) format_version: u32,
    pub(crate) library: String,
    pub(crate) library_version: String,
    pub(crate) path: String,
    pub(crate) sha256: String,
    pub(crate) size: u64,
    pub(crate) corpus_id: String,
    pub(crate) embedding_model_id: String,
    pub(crate) embedding_dimension: u32,
    pub(crate) embedding_set_sha256: String,
    pub(crate) vector_count: u64,
    pub(crate) seed: u64,
    pub(crate) rng: String,
    pub(crate) trees: u32,
    pub(crate) split_after: u32,
    pub(crate) id_encoding: String,
    pub(crate) metric: String,
}

pub(crate) fn decode_manifest(bytes: &[u8], expected_schema: u32) -> Result<LegacyArroyManifest> {
    let manifest: LegacyArroyManifest =
        serde_json::from_slice(bytes).context("parsing legacy Arroy manifest")?;
    validate_manifest(&manifest, expected_schema)?;
    Ok(manifest)
}

pub(crate) fn validate_manifest(
    manifest: &LegacyArroyManifest,
    expected_schema: u32,
) -> Result<()> {
    if manifest.schema_version != expected_schema {
        bail!(
            "derivation source must use exactly schema {expected_schema}, got {}",
            manifest.schema_version
        );
    }
    if manifest.index_version.trim() != manifest.index_version
        || manifest.index_version.is_empty()
        || manifest.index_version.chars().any(char::is_control)
    {
        bail!("legacy manifest index_version is malformed");
    }
    chrono::DateTime::parse_from_rfc3339(&manifest.created_at)
        .context("legacy manifest created_at must be RFC 3339")?;
    let minimum = parse_release_version(
        &manifest.min_client_version,
        "legacy manifest min_client_version",
    )?;
    let current = parse_release_version(env!("CARGO_PKG_VERSION"), "binary version")?;
    if minimum > current {
        bail!(
            "legacy manifest requires australian-legal-mcp >= {}, but this binary is {}",
            manifest.min_client_version,
            env!("CARGO_PKG_VERSION")
        );
    }
    validate_model(&manifest.model)?;
    if manifest.db.path != LEGAL_DB_FILENAME
        || manifest.db.size == 0
        || !is_lower_sha256(&manifest.db.sha256)
    {
        bail!("legacy manifest database metadata is malformed");
    }

    let expected_sources = crate::legal_source::source_registry()
        .source_ids()
        .into_iter()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    let manifest_sources = manifest
        .ann
        .keys()
        .map(|source| source.as_str().to_owned())
        .collect::<BTreeSet<_>>();
    if manifest_sources != expected_sources {
        bail!(
            "legacy manifest source set does not match this binary: registered={expected_sources:?}, manifest={manifest_sources:?}"
        );
    }
    for (source_id, ann) in &manifest.ann {
        validate_ann(source_id, ann, &manifest.model)?;
    }
    Ok(())
}

fn validate_model(model: &ModelInfo) -> Result<()> {
    if model.id != EMBEDDING_MODEL_ID || model.fingerprint != EMBEDDING_MODEL_FINGERPRINT {
        bail!("legacy manifest model identity does not match the pinned model");
    }
    validate_model_file(&model.model, &EMBEDDING_MODEL_FILES[0], "model graph")?;
    validate_model_file(
        &model.tokenizer,
        &EMBEDDING_MODEL_FILES[1],
        "model tokenizer",
    )?;
    Ok(())
}

fn validate_model_file(
    actual: &ManifestFile,
    expected: &crate::semantic::EmbeddingModelFile,
    label: &str,
) -> Result<()> {
    if actual.path != expected.output_name
        || actual.size != expected.size
        || actual.sha256 != expected.sha256
    {
        bail!("legacy manifest {label} metadata does not match the pinned model");
    }
    Ok(())
}

fn validate_ann(source_id: &SourceId, ann: &LegacyArroyAnn, model: &ModelInfo) -> Result<()> {
    if &ann.source_id != source_id {
        bail!(
            "legacy ANN source mismatch: key `{source_id}`, entry `{}`",
            ann.source_id
        );
    }
    let expected_path = format!("ann/{source_id}.ann");
    if ann.path != expected_path
        || Path::new(&ann.path).components().count() != 2
        || ann.format != ARROY_FORMAT
        || ann.format_version != ARROY_FORMAT_VERSION
        || ann.library != ARROY_LIBRARY
        || ann.library_version != ARROY_LIBRARY_VERSION
        || ann.embedding_model_id != model.id
        || ann.embedding_dimension != EMBEDDING_DIM as u32
        || ann.seed != ARROY_SEED
        || ann.rng != ARROY_RNG
        || ann.trees != ARROY_TREES
        || ann.split_after != ARROY_SPLIT_AFTER
        || ann.id_encoding != ARROY_ID_ENCODING
        || ann.metric != ARROY_METRIC
    {
        bail!("legacy ANN contract for source `{source_id}` is incompatible");
    }
    if ann.size == 0
        || ann.vector_count == 0
        || !is_lower_sha256(&ann.sha256)
        || !is_corpus_id(&ann.corpus_id)
        || !is_lower_sha256(&ann.embedding_set_sha256)
    {
        bail!("legacy ANN integrity metadata for source `{source_id}` is malformed");
    }
    Ok(())
}

pub(crate) fn immutable_artifact_paths(manifest: &LegacyArroyManifest) -> Vec<String> {
    let mut paths = vec![
        manifest.model.model.path.clone(),
        manifest.model.tokenizer.path.clone(),
    ];
    paths.extend(manifest.ann.values().map(|ann| ann.path.clone()));
    paths
}

pub(crate) fn checked_artifact_bytes(manifest: &LegacyArroyManifest) -> Result<u64> {
    manifest
        .db
        .size
        .checked_add(manifest.model.model.size)
        .and_then(|value| value.checked_add(manifest.model.tokenizer.size))
        .and_then(|value| {
            manifest
                .ann
                .values()
                .try_fold(value, |total, ann| total.checked_add(ann.size))
        })
        .ok_or_else(|| anyhow::anyhow!("legacy generation artifact sizes overflow u64"))
}

pub(crate) fn is_lower_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_corpus_id(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(is_lower_sha256)
}

fn parse_release_version(value: &str, label: &str) -> Result<Vec<u32>> {
    if value.is_empty() || value.trim() != value {
        bail!("{label} is malformed");
    }
    let (core, suffix) = value
        .split_once('-')
        .map_or((value, None), |(core, suffix)| (core, Some(suffix)));
    if suffix.is_some_and(|suffix| {
        suffix.is_empty()
            || !suffix
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
    }) {
        bail!("{label} is malformed");
    }
    let fields = core.split('.').collect::<Vec<_>>();
    if fields.len() != 3 {
        bail!("{label} must contain major.minor.patch");
    }
    fields
        .into_iter()
        .map(|field| {
            if field.is_empty()
                || !field.bytes().all(|byte| byte.is_ascii_digit())
                || (field.len() > 1 && field.starts_with('0'))
            {
                bail!("{label} is malformed");
            }
            field
                .parse::<u32>()
                .with_context(|| format!("{label} component is too large"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const V19_GENERATION: &str = "1a6beead567b55babebbe253b5ae13efcd9ce2e8ab55b60c2de4106e39f180f4";
    const V20_GENERATION: &str = "a6e7da47edf2c332dbe616b2014a8b63dbdd9e793065c85da959cf56a2791aa3";

    #[test]
    fn parses_real_v19_and_v20_arroy_manifests() -> Result<()> {
        let v19 = decode_manifest(
            include_bytes!("../tests/fixtures/generation-v19-arroy.json"),
            10,
        )?;
        let v20 = decode_manifest(
            include_bytes!("../tests/fixtures/generation-v20-arroy.json"),
            11,
        )?;
        assert_eq!(
            crate::source::generation_key(&v19)?.as_str(),
            V19_GENERATION
        );
        assert_eq!(
            crate::source::generation_key(&v20)?.as_str(),
            V20_GENERATION
        );
        assert_eq!(v19.ann.len(), 10);
        assert_eq!(v20.ann.len(), 10);
        Ok(())
    }

    #[test]
    fn rejects_unknown_manifest_and_ann_fields() -> Result<()> {
        let manifest = decode_manifest(
            include_bytes!("../tests/fixtures/generation-v20-arroy.json"),
            11,
        )?;
        let mut value = serde_json::to_value(manifest)?;
        value["unexpected"] = serde_json::Value::Bool(true);
        assert!(decode_manifest(&serde_json::to_vec(&value)?, 11).is_err());

        let mut value: serde_json::Value = serde_json::from_slice(include_bytes!(
            "../tests/fixtures/generation-v20-arroy.json"
        ))?;
        value["ann"]["ato"]["unexpected"] = serde_json::Value::Bool(true);
        assert!(decode_manifest(&serde_json::to_vec(&value)?, 11).is_err());
        Ok(())
    }

    #[test]
    fn schema_is_not_inferred_from_arroy_shape() {
        assert!(decode_manifest(
            include_bytes!("../tests/fixtures/generation-v20-arroy.json"),
            10
        )
        .is_err());
        assert!(
            serde_json::from_slice::<crate::source::Manifest>(include_bytes!(
                "../tests/fixtures/generation-v20-arroy.json"
            ))
            .is_err()
        );
    }
}
