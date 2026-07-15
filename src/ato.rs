//! Australian Taxation Office source adapter.
//!
//! Acquisition owns the committed index and payload workspace. This adapter turns that
//! authoritative local snapshot into the same `NormalizedDocument` stream consumed for every
//! other source; corpus orchestration has no ATO-specific ingestion path.

use crate::extract::{
    extract_compose_title, extract_em_front_matter, extract_leading_headings,
    metadata_extract_pub_date, metadata_parse_docid, rewrite_images_html,
};
use crate::html::{
    canonical_ato_native_id, canonical_source_character, clean_ato_html, normalise_named_anchors,
    rewrite_links_html, strip_attributes,
};
use crate::rules::{derive_metadata, RuleInputs};
use anyhow::{anyhow, bail, Context, Result};
use legal_model::{AssetRef, DocumentId, SourceId};
use legal_source_sdk::{sha256_bytes, NormalizedAsset, NormalizedDocument, SourceInventoryRecord};
use rayon::prelude::*;
use serde_json::Value as JsonValue;
use std::collections::BTreeMap;
use std::fs;
use std::io::BufRead as _;
use std::path::{Component, Path, PathBuf};

const NORMALIZATION_BATCH_SIZE: usize = 256;

pub(crate) fn normalized_document_results(
    source: &SourceId,
    workspace: &Path,
) -> Result<Box<dyn Iterator<Item = Result<NormalizedDocument>>>> {
    if source.as_str() != crate::source_catalog::ATO_SOURCE_ID {
        bail!("ATO loader cannot load source `{source}`");
    }
    let workspace = workspace
        .canonicalize()
        .with_context(|| format!("canonicalizing ATO workspace {}", workspace.display()))?;
    let index_path = workspace.join("index.jsonl");
    let index_metadata = fs::symlink_metadata(&index_path)
        .with_context(|| format!("reading ATO index {}", index_path.display()))?;
    if index_metadata.file_type().is_symlink() || !index_metadata.is_file() {
        bail!("ATO index must be a real file: {}", index_path.display());
    }
    let records = load_authoritative_records(&index_path)?;
    if records.is_empty() {
        bail!("ATO workspace has no buildable authoritative records");
    }
    Ok(Box::new(AtoDocuments {
        source: source.clone(),
        workspace,
        records: records.into_iter(),
        ready: Vec::new().into_iter(),
    }))
}

struct AtoDocuments {
    source: SourceId,
    workspace: PathBuf,
    records: std::vec::IntoIter<JsonValue>,
    ready: std::vec::IntoIter<Result<NormalizedDocument>>,
}

impl Iterator for AtoDocuments {
    type Item = Result<NormalizedDocument>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(document) = self.ready.next() {
                return Some(document);
            }
            let batch = self
                .records
                .by_ref()
                .take(NORMALIZATION_BATCH_SIZE)
                .collect::<Vec<_>>();
            if batch.is_empty() {
                return None;
            }
            let source = &self.source;
            let workspace = &self.workspace;
            self.ready = batch
                .into_par_iter()
                .map(|record| normalize_record(source, workspace, record))
                .collect::<Vec<_>>()
                .into_iter();
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.ready.len() + self.records.len();
        (remaining, Some(remaining))
    }
}

fn normalize_record(
    source: &SourceId,
    workspace: &Path,
    record: JsonValue,
) -> Result<NormalizedDocument> {
    let canonical_id = record
        .get("canonical_id")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| anyhow!("ATO index record missing canonical_id"))?;
    let native_id = canonical_ato_native_id(&crate::extract::metadata_doc_id_for(canonical_id));
    let document_id = DocumentId::new(source.clone(), native_id.clone())?;
    let payload_path_raw = record
        .get("payload_path")
        .and_then(JsonValue::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("ATO index record {native_id} has no payload path"))?;
    let payload_path = confined_payload_path(workspace, payload_path_raw)?;
    let expected_size = record
        .get("size")
        .and_then(JsonValue::as_u64)
        .ok_or_else(|| anyhow!("ATO index record {native_id} has no payload size"))?;
    let expected_sha256 = record
        .get("sha256")
        .and_then(JsonValue::as_str)
        .filter(|value| value.len() == 64)
        .ok_or_else(|| anyhow!("ATO index record {native_id} has no payload SHA-256"))?;
    let payload = fs::read(&payload_path)
        .with_context(|| format!("reading ATO payload {}", payload_path.display()))?;
    if payload.len() as u64 != expected_size || sha256_bytes(&payload) != expected_sha256 {
        bail!("ATO payload integrity mismatch for {native_id}");
    }
    let html = String::from_utf8(payload)
        .with_context(|| format!("ATO payload is not UTF-8: {}", payload_path.display()))?;

    let cleaned = clean_ato_html(&html);
    let (rewritten_html, extracted_assets) =
        rewrite_images_html(&cleaned.html, Some(&native_id), Some(&payload_path));
    let normalized_anchors = normalise_named_anchors(&rewritten_html);
    let rewritten_links = rewrite_links_html(&normalized_anchors);
    let final_html = strip_attributes(&rewritten_links)
        .chars()
        .filter_map(canonical_source_character)
        .collect::<String>();

    let document_type = metadata_parse_docid(canonical_id).unwrap_or_default();
    let leading = extract_leading_headings(&cleaned.html);
    let raw_title = extract_compose_title(&leading)
        .or(cleaned.title)
        .unwrap_or_else(|| canonical_id.to_string());
    let fragment = scraper::Html::parse_fragment(&final_html);
    let heading_selector = scraper::Selector::parse("h1, h2, h3, h4, h5, h6")
        .map_err(|error| anyhow!("parsing ATO heading selector: {error:?}"))?;
    let mut headings = Vec::new();
    let mut heading_levels = Vec::new();
    for heading in fragment.select(&heading_selector) {
        let text = crate::extract::anchors_node_text(heading);
        if text.is_empty() {
            continue;
        }
        headings.push(text);
        heading_levels.push(match heading.value().name() {
            "h1" => 1,
            "h2" => 2,
            "h3" => 3,
            "h4" => 4,
            "h5" => 5,
            "h6" => 6,
            _ => 0,
        });
    }
    let body_head = cleaned.text.chars().take(3_000).collect::<String>();
    let publication_date = metadata_extract_pub_date(&body_head);
    let (front_matter_refs, front_matter_phrase) = extract_em_front_matter(&cleaned.html);
    let derived = derive_metadata(&RuleInputs {
        doc_id: native_id.clone(),
        title: Some(raw_title.clone()),
        headings,
        heading_levels,
        body_head,
        category: Some(document_type.clone()),
        pub_date: publication_date,
        front_matter_refs,
        front_matter_phrase,
    });
    let title = derived.title.unwrap_or(raw_title);

    let assets = extracted_assets
        .into_iter()
        .map(|asset| {
            NormalizedAsset::new(
                AssetRef::new(source.clone(), asset.asset_id)?,
                asset
                    .media_type
                    .unwrap_or_else(|| "application/octet-stream".to_string()),
                asset.alt,
                asset.title,
                asset.sha256,
                asset.data,
            )
            .map_err(Into::into)
        })
        .collect::<Result<Vec<_>>>()?;
    let inventory = SourceInventoryRecord::new(
        document_id,
        ato_upstream_version(&record, canonical_id),
        reqwest::Url::parse("https://www.ato.gov.au")?
            .join(
                record
                    .get("href")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("ATO index record {native_id} has no upstream href"))?,
            )?
            .to_string(),
        document_type,
        title,
        derived.date,
        payload_path_raw,
        expected_sha256,
        expected_size,
        "text/html",
    )?;
    NormalizedDocument::new(inventory, final_html, assets).map_err(Into::into)
}

fn load_authoritative_records(index_path: &Path) -> Result<Vec<JsonValue>> {
    let file = fs::File::open(index_path)
        .with_context(|| format!("opening ATO index {}", index_path.display()))?;
    let mut latest = BTreeMap::<String, Option<JsonValue>>::new();
    for line in std::io::BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let record: JsonValue = serde_json::from_str(&line).context("parsing ATO index line")?;
        let canonical_id = record
            .get("canonical_id")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("ATO index record missing canonical_id"))?;
        let inventory_key =
            canonical_ato_native_id(&crate::extract::metadata_doc_id_for(canonical_id));
        let has_payload = record
            .get("payload_path")
            .and_then(JsonValue::as_str)
            .is_some_and(|path| !path.is_empty())
            && record.get("size").and_then(JsonValue::as_u64).is_some()
            && record
                .get("sha256")
                .and_then(JsonValue::as_str)
                .is_some_and(|digest| digest.len() == 64);
        let status = record.get("status").and_then(JsonValue::as_str);
        if has_payload && status == Some("success") {
            latest.insert(inventory_key, Some(record));
        } else if matches!(status, Some("confirmed_404" | "confirmed_stub")) {
            latest.insert(inventory_key, None);
        } else {
            latest.entry(inventory_key).or_insert(None);
        }
    }
    Ok(latest.into_values().flatten().collect())
}

fn ato_upstream_version(record: &JsonValue, canonical_id: &str) -> Option<String> {
    for field in ["upstream_version", "version", "pit"] {
        if let Some(value) = record
            .get(field)
            .and_then(JsonValue::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return Some(value.to_string());
        }
    }
    record
        .get("href")
        .and_then(JsonValue::as_str)
        .and_then(|href| query_parameter(href, "pit"))
        .or_else(|| query_parameter(canonical_id, "pit"))
}

fn query_parameter(value: &str, name: &str) -> Option<String> {
    let parsed = url::Url::parse(value).ok().or_else(|| {
        let path = if value.starts_with('/') {
            value.to_string()
        } else {
            format!("/{value}")
        };
        url::Url::parse(&format!("https://www.ato.gov.au{path}")).ok()
    })?;
    parsed
        .query_pairs()
        .find(|(key, value)| key.eq_ignore_ascii_case(name) && !value.trim().is_empty())
        .map(|(_, value)| value.into_owned())
}

fn confined_payload_path(workspace: &Path, raw: &str) -> Result<PathBuf> {
    if raw.is_empty()
        || raw.contains(['\\', ':'])
        || raw.starts_with(['/', '\\'])
        || raw.contains(['?', '#'])
    {
        bail!("unsafe ATO payload path `{raw}`");
    }
    let relative = Path::new(raw);
    let mut components = relative.components();
    if components.next() != Some(Component::Normal("payloads".as_ref()))
        || components.any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        bail!("ATO payload path must be relative beneath payloads/: `{raw}`");
    }
    let mut current = workspace.to_path_buf();
    for component in relative.components() {
        if matches!(component, Component::CurDir) {
            continue;
        }
        current.push(component.as_os_str());
        let metadata = fs::symlink_metadata(&current)
            .with_context(|| format!("reading ATO payload component {}", current.display()))?;
        if metadata.file_type().is_symlink() {
            bail!("ATO payload path contains symlink {}", current.display());
        }
    }
    let canonical = current.canonicalize()?;
    if !canonical.starts_with(workspace) || !canonical.is_file() {
        bail!(
            "ATO payload path escaped workspace: {}",
            canonical.display()
        );
    }
    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use super::normalized_document_results;
    use anyhow::{Context, Result};
    use legal_model::SourceId;

    #[test]
    #[ignore = "requires LEGAL_MCP_ATO_PAGES_DIR"]
    fn benchmark_normalization_throughput() -> Result<()> {
        let workspace = std::env::var("LEGAL_MCP_ATO_PAGES_DIR")
            .context("LEGAL_MCP_ATO_PAGES_DIR is required")?;
        let requested = std::env::var("LEGAL_MCP_BENCH_SAMPLES")
            .ok()
            .map(|value| value.parse::<usize>())
            .transpose()?
            .unwrap_or(10_000);
        let source = SourceId::new("ato")?;
        let started = std::time::Instant::now();
        let mut documents = 0usize;
        let mut html_bytes = 0usize;
        for document in
            normalized_document_results(&source, std::path::Path::new(&workspace))?.take(requested)
        {
            let document = document?;
            documents += 1;
            html_bytes += document.html.len();
        }
        let elapsed = started.elapsed().as_secs_f64();
        eprintln!(
            "ATO_NORMALIZE_BENCH documents={documents} html_mb={:.1} elapsed_s={elapsed:.3} documents_per_s={:.1}",
            html_bytes as f64 / (1024.0 * 1024.0),
            documents as f64 / elapsed,
        );
        Ok(())
    }
}
