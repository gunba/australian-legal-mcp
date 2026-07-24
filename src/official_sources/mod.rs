//! Clean-room acquisition adapters for official Australian legal databases.
//!
//! The adapters share one resumable, source-qualified workspace contract while
//! retaining source-owned discovery, pacing, concurrency, and parsing rules.

mod federal_court;
mod high_court;
mod nsw_caselaw;
mod nsw_legislation;
mod queensland_legislation;
mod south_australian_legislation;
mod tasmanian_legislation;
mod western_australian_legislation;

use crate::adaptive_http::{AdaptiveConcurrency, RequestOutcome, SOURCE_WORKER_CEILING};
use crate::browser_http::BrowserHttpTransport;
use crate::config::atomic_write;
use crate::html::canonical_source_character;
use crate::source_update::{
    SourceAcquisition, SourceDiscoveryBatch, SourceFetchReport, SourceInventoryFingerprint,
    SourceRatePolicy, SourceUpdateContext, SourceUpdateMode,
};
use anyhow::{anyhow, bail, Context, Result};
use chrono::Utc;
use ego_tree::NodeRef;
use encoding_rs::{Encoding, UTF_8, WINDOWS_1252};
use legal_model::{AssetRef, DocumentId, SourceDescriptor, SourceId};
use legal_source_sdk::{NormalizedAsset, NormalizedDocument, SourceInventoryRecord};
use rayon::prelude::*;
use reqwest::blocking::{Client, Response};
use reqwest::header::{ACCEPT, CONTENT_LENGTH, CONTENT_TYPE, LOCATION, RETRY_AFTER};
use reqwest::{StatusCode, Url};
use scraper::node::{Element, Node};
use scraper::{CaseSensitivity, Html, Selector};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

const STATE_SCHEMA_VERSION: u32 = 1;
const PLAN_SCHEMA_VERSION: u32 = 1;
const MAX_PLAN_BYTES: u64 = 512 * 1024 * 1024;
const MAX_STATE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_INDEX_BYTES: u64 = 64 * 1024 * 1024;
const MAX_DOCUMENT_BYTES: u64 = 128 * 1024 * 1024;
const MAX_ASSET_BYTES: u64 = 64 * 1024 * 1024;
const MIN_PDF_TEXT_ALPHANUMERIC_CHARS: usize = 128;
const MAX_HTTP_ATTEMPTS: usize = 5;
const MAX_RATE_LIMIT_ATTEMPTS: usize = 20;
const MAX_REDIRECTS: usize = 5;
const MAX_RETRY_DELAY: Duration = Duration::from_secs(120);
const NORMALIZED_MEDIA_TYPE: &str = "application/vnd.australian-legal.normalized+json";
static OCR_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

pub(crate) const FEDERAL_COURT_SOURCE_ID: &str = "federal-court";
pub(crate) const HIGH_COURT_SOURCE_ID: &str = "high-court";
pub(crate) const NSW_CASELAW_SOURCE_ID: &str = "nsw-caselaw";
pub(crate) const NSW_LEGISLATION_SOURCE_ID: &str = "nsw-legislation";
pub(crate) const QUEENSLAND_LEGISLATION_SOURCE_ID: &str = "qld-legislation";
pub(crate) const WESTERN_AUSTRALIAN_LEGISLATION_SOURCE_ID: &str = "wa-legislation";
pub(crate) const SOUTH_AUSTRALIAN_LEGISLATION_SOURCE_ID: &str = "sa-legislation";
pub(crate) const TASMANIAN_LEGISLATION_SOURCE_ID: &str = "tas-legislation";

pub(crate) static FEDERAL_COURT_ACQUISITION: OfficialAcquisition =
    OfficialAcquisition::new(&federal_court::ADAPTER);
pub(crate) static HIGH_COURT_ACQUISITION: OfficialAcquisition =
    OfficialAcquisition::new(&high_court::ADAPTER);
pub(crate) static NSW_CASELAW_ACQUISITION: OfficialAcquisition =
    OfficialAcquisition::new(&nsw_caselaw::ADAPTER);
pub(crate) static NSW_LEGISLATION_ACQUISITION: OfficialAcquisition =
    OfficialAcquisition::new(&nsw_legislation::ADAPTER);
pub(crate) static QUEENSLAND_LEGISLATION_ACQUISITION: OfficialAcquisition =
    OfficialAcquisition::new(&queensland_legislation::ADAPTER);
pub(crate) static WESTERN_AUSTRALIAN_LEGISLATION_ACQUISITION: OfficialAcquisition =
    OfficialAcquisition::new(&western_australian_legislation::ADAPTER);
pub(crate) static SOUTH_AUSTRALIAN_LEGISLATION_ACQUISITION: OfficialAcquisition =
    OfficialAcquisition::new(&south_australian_legislation::ADAPTER);
pub(crate) static TASMANIAN_LEGISLATION_ACQUISITION: OfficialAcquisition =
    OfficialAcquisition::new(&tasmanian_legislation::ADAPTER);

pub(crate) fn descriptors() -> Result<Vec<SourceDescriptor>> {
    adapters()
        .into_iter()
        .map(|adapter| {
            SourceDescriptor::new(adapter.source_id().parse()?, adapter.display_name())
                .map_err(Into::into)
        })
        .collect()
}

pub(crate) fn acquisition_for(source_id: &str) -> Option<&'static dyn SourceAcquisition> {
    match source_id {
        FEDERAL_COURT_SOURCE_ID => Some(&FEDERAL_COURT_ACQUISITION),
        HIGH_COURT_SOURCE_ID => Some(&HIGH_COURT_ACQUISITION),
        NSW_CASELAW_SOURCE_ID => Some(&NSW_CASELAW_ACQUISITION),
        NSW_LEGISLATION_SOURCE_ID => Some(&NSW_LEGISLATION_ACQUISITION),
        QUEENSLAND_LEGISLATION_SOURCE_ID => Some(&QUEENSLAND_LEGISLATION_ACQUISITION),
        WESTERN_AUSTRALIAN_LEGISLATION_SOURCE_ID => {
            Some(&WESTERN_AUSTRALIAN_LEGISLATION_ACQUISITION)
        }
        SOUTH_AUSTRALIAN_LEGISLATION_SOURCE_ID => Some(&SOUTH_AUSTRALIAN_LEGISLATION_ACQUISITION),
        TASMANIAN_LEGISLATION_SOURCE_ID => Some(&TASMANIAN_LEGISLATION_ACQUISITION),
        _ => None,
    }
}

fn adapters() -> Vec<&'static dyn OfficialAdapter> {
    vec![
        &federal_court::ADAPTER,
        &high_court::ADAPTER,
        &nsw_caselaw::ADAPTER,
        &nsw_legislation::ADAPTER,
        &queensland_legislation::ADAPTER,
        &western_australian_legislation::ADAPTER,
        &south_australian_legislation::ADAPTER,
        &tasmanian_legislation::ADAPTER,
    ]
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum RenditionKind {
    Html,
    Docx,
    Rtf,
    Pdf,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Rendition {
    pub(super) url: String,
    pub(super) kind: RenditionKind,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DiscoveredDocument {
    pub(super) native_id: String,
    pub(super) upstream_version: String,
    pub(super) title: String,
    pub(super) document_type: String,
    pub(super) date: Option<String>,
    pub(super) citation: Option<String>,
    pub(super) canonical_url: String,
    pub(super) renditions: Vec<Rendition>,
}

#[derive(Clone, Debug)]
pub(super) struct AcquiredDocument {
    pub(super) html: String,
    pub(super) assets: Vec<NormalizedAsset>,
    pub(super) date: Option<String>,
    pub(super) canonical_url: String,
}

pub(super) trait OfficialAdapter: Send + Sync {
    fn source_id(&self) -> &'static str;
    fn display_name(&self) -> &'static str;
    fn approved_hosts(&self) -> &'static [&'static str];
    fn rate_policy(&self) -> SourceRatePolicy;
    fn minimum_request_interval_ms(&self, _url: &Url) -> u64 {
        self.rate_policy().minimum_request_interval_ms
    }
    fn has_browser_transport(&self) -> bool {
        false
    }
    fn use_browser_transport(&self, _url: &Url) -> bool {
        false
    }
    fn normalization_revision(&self) -> Option<&'static str> {
        None
    }
    fn minimum_snapshot_retention_percent(&self) -> usize {
        50
    }
    fn validate_normalized_html(&self, _html: &str) -> Result<()> {
        Ok(())
    }
    fn discover(
        &self,
        client: &OfficialHttpClient,
        mode: SourceUpdateMode,
    ) -> Result<Vec<DiscoveredDocument>>;
    fn acquire(
        &self,
        client: &OfficialHttpClient,
        entry: &DiscoveredDocument,
    ) -> Result<Option<AcquiredDocument>>;
}

pub(crate) struct OfficialAcquisition {
    adapter: &'static dyn OfficialAdapter,
}

impl OfficialAcquisition {
    const fn new(adapter: &'static dyn OfficialAdapter) -> Self {
        Self { adapter }
    }
}

impl SourceAcquisition for OfficialAcquisition {
    fn rate_policy(&self) -> SourceRatePolicy {
        self.adapter.rate_policy()
    }

    fn inventory(&self, context: &SourceUpdateContext) -> Result<SourceInventoryFingerprint> {
        let state = load_state(self.adapter, &context.workspace)?;
        fingerprint_inventory(&state.inventory)
    }

    fn discover(&self, context: &SourceUpdateContext) -> Result<SourceDiscoveryBatch> {
        let relative = PathBuf::from(self.adapter.source_id()).join("discovery.json");
        let path = confined_path(&context.run_dir, &relative)?;
        if path.exists() {
            let bytes = read_bounded_file(&path, MAX_PLAN_BYTES)?;
            let mut plan: DiscoveryPlan = serde_json::from_slice(&bytes)
                .context("decoding resumable official-source discovery")?;
            if plan.schema_version != PLAN_SCHEMA_VERSION
                || plan.source_id != self.adapter.source_id()
                || plan.mode != context.mode
            {
                bail!(
                    "{} resumable discovery plan contract mismatch",
                    self.adapter.source_id()
                );
            }
            validate_discovery(self.adapter, &mut plan.documents)?;
            if let Ok(previous) = load_state(self.adapter, &context.workspace) {
                validate_snapshot_size(
                    self.adapter.source_id(),
                    plan.documents.len(),
                    previous.inventory.len(),
                    self.adapter.minimum_snapshot_retention_percent(),
                )?;
            }
            return Ok(SourceDiscoveryBatch {
                path,
                records: plan.documents.len(),
            });
        }
        let client = OfficialHttpClient::new(self.adapter)?;
        let mut documents = self.adapter.discover(&client, context.mode)?;
        validate_discovery(self.adapter, &mut documents)?;
        if let Ok(previous) = load_state(self.adapter, &context.workspace) {
            validate_snapshot_size(
                self.adapter.source_id(),
                documents.len(),
                previous.inventory.len(),
                self.adapter.minimum_snapshot_retention_percent(),
            )?;
        }
        let plan = DiscoveryPlan {
            schema_version: PLAN_SCHEMA_VERSION,
            source_id: self.adapter.source_id().to_owned(),
            mode: context.mode,
            discovered_at: Utc::now().to_rfc3339(),
            documents,
        };
        let bytes = serde_json::to_vec(&plan).context("serializing official-source discovery")?;
        if bytes.len() as u64 > MAX_PLAN_BYTES {
            bail!(
                "{} discovery plan exceeds its byte limit",
                self.adapter.source_id()
            );
        }
        atomic_write(&path, &bytes)?;
        Ok(SourceDiscoveryBatch {
            path,
            records: plan.documents.len(),
        })
    }

    fn fetch(
        &self,
        context: &SourceUpdateContext,
        discovery: &SourceDiscoveryBatch,
    ) -> Result<SourceFetchReport> {
        let expected = confined_path(
            &context.run_dir,
            &PathBuf::from(self.adapter.source_id()).join("discovery.json"),
        )?;
        if discovery.path != expected {
            bail!("{} discovery plan path changed", self.adapter.source_id());
        }
        let bytes = read_bounded_file(&discovery.path, MAX_PLAN_BYTES)?;
        let plan: DiscoveryPlan =
            serde_json::from_slice(&bytes).context("decoding official-source discovery")?;
        if plan.schema_version != PLAN_SCHEMA_VERSION
            || plan.source_id != self.adapter.source_id()
            || plan.mode != context.mode
            || plan.documents.len() != discovery.records
        {
            bail!(
                "{} discovery plan contract mismatch",
                self.adapter.source_id()
            );
        }
        let mut documents = plan.documents;
        validate_discovery(self.adapter, &mut documents)?;
        fetch_documents(self.adapter, &context.workspace, documents)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DiscoveryPlan {
    schema_version: u32,
    source_id: String,
    mode: SourceUpdateMode,
    discovered_at: String,
    documents: Vec<DiscoveredDocument>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct OfficialState {
    schema_version: u32,
    inventory: BTreeMap<String, InventoryEntry>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct InventoryEntry {
    native_id: String,
    upstream_version: String,
    canonical_url: String,
    content_hash: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StoredDocument<'a> {
    source: &'a str,
    native_id: &'a str,
    title: &'a str,
    document_type: &'a str,
    date: &'a Option<String>,
    citation: &'a Option<String>,
    canonical_url: &'a str,
    cleaned_html: &'a str,
    assets: Vec<StoredAsset>,
    content_hash: &'a str,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredDocumentOwned {
    source: String,
    native_id: String,
    title: String,
    document_type: String,
    date: Option<String>,
    citation: Option<String>,
    canonical_url: String,
    cleaned_html: String,
    assets: Vec<StoredAsset>,
    content_hash: String,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredAsset {
    asset_id: String,
    media_type: String,
    relative_path: String,
    size: usize,
    sha256: String,
}

fn validate_discovery(
    adapter: &dyn OfficialAdapter,
    documents: &mut [DiscoveredDocument],
) -> Result<()> {
    documents.sort_by(|left, right| left.native_id.cmp(&right.native_id));
    if documents.is_empty() {
        bail!(
            "{} discovery returned no authoritative documents",
            adapter.source_id()
        );
    }
    let source: SourceId = adapter.source_id().parse()?;
    let mut seen = BTreeSet::new();
    for document in documents.iter() {
        DocumentId::new(source.clone(), document.native_id.clone())?;
        validate_text("upstream version", &document.upstream_version, 32 * 1024)?;
        validate_text("title", &document.title, 128 * 1024)?;
        validate_text("document type", &document.document_type, 256)?;
        validate_date(document.date.as_deref())?;
        validate_official_url(adapter, &document.canonical_url)?;
        if document.renditions.is_empty() {
            bail!(
                "{} document {} has no rendition",
                adapter.source_id(),
                document.native_id
            );
        }
        for rendition in &document.renditions {
            validate_official_url(adapter, &rendition.url)?;
        }
        if !seen.insert(document.native_id.as_str()) {
            bail!(
                "{} discovery contains duplicate {}",
                adapter.source_id(),
                document.native_id
            );
        }
    }
    Ok(())
}

fn validate_snapshot_size(
    source_id: &str,
    current: usize,
    previous: usize,
    minimum_percent: usize,
) -> Result<()> {
    if !(1..=100).contains(&minimum_percent) {
        bail!("{source_id} has an invalid snapshot-retention policy");
    }
    if current.saturating_mul(100) < previous.saturating_mul(minimum_percent) {
        bail!(
            "{source_id} discovery shrank from {previous} to {current} documents; refusing a destructive partial snapshot"
        );
    }
    Ok(())
}

fn fetch_documents(
    adapter: &'static dyn OfficialAdapter,
    workspace: &Path,
    documents: Vec<DiscoveredDocument>,
) -> Result<SourceFetchReport> {
    let previous = load_state(adapter, workspace)?;
    let discovered_count = documents.len();
    let client = OfficialHttpClient::new(adapter)?;
    let thread_prefix = adapter.source_id().to_owned();
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(SOURCE_WORKER_CEILING)
        .thread_name(move |index| format!("{thread_prefix}-fetch-{index}"))
        .build()
        .context("building official-source fetch pool")?;
    let source: SourceId = adapter.source_id().parse()?;
    let progress = AtomicUsize::new(0);
    let mut results = pool.install(|| {
        documents
            .par_iter()
            .map(|entry| {
                let result = acquire_one(
                    adapter,
                    &client,
                    workspace,
                    &source,
                    entry,
                    previous.inventory.get(&entry.native_id),
                );
                let completed = progress.fetch_add(1, Ordering::Relaxed) + 1;
                if completed.is_multiple_of(1_000) {
                    eprintln!(
                        "legal-mcp source {}: processed {completed} discovered documents",
                        adapter.source_id()
                    );
                }
                result
            })
            .collect::<Result<Vec<_>>>()
    })?;
    results.sort_by(|left, right| left.0.cmp(&right.0));
    let mut inventory = BTreeMap::new();
    let mut completed = 0;
    let mut skipped = 0;
    for (native_id, entry, reused) in results {
        if entry.is_none() || reused {
            skipped += 1;
        } else {
            completed += 1;
        }
        if let Some(entry) = entry {
            inventory.insert(native_id, entry);
        }
    }
    if inventory.len().saturating_mul(100) < discovered_count.saturating_mul(99) {
        bail!(
            "{} acquisition found full text for only {} of {discovered_count} discovered records",
            adapter.source_id(),
            inventory.len()
        );
    }
    let next = OfficialState {
        schema_version: STATE_SCHEMA_VERSION,
        inventory,
    };
    // Staging cleanup is fallible, so complete it before the atomic state write.
    // Once state.json advances, the source update is committed and must not
    // subsequently report failure.
    clear_staging(adapter, workspace)?;
    commit_state(adapter, workspace, &next)?;
    Ok(SourceFetchReport {
        completed,
        failed: 0,
        skipped,
    })
}

fn acquire_one(
    adapter: &dyn OfficialAdapter,
    client: &OfficialHttpClient,
    workspace: &Path,
    source: &SourceId,
    entry: &DiscoveredDocument,
    committed: Option<&InventoryEntry>,
) -> Result<(String, Option<InventoryEntry>, bool)> {
    let source_revision = discovered_source_revision(adapter, entry)?;
    if let Some(committed) = committed {
        if reusable_entry(
            adapter,
            workspace,
            source,
            entry,
            &source_revision,
            committed,
        ) {
            return Ok((entry.native_id.clone(), Some(committed.clone()), true));
        }
    }
    if let Some(staged) = load_staging_entry(adapter, workspace, &entry.native_id)? {
        if reusable_entry(adapter, workspace, source, entry, &source_revision, &staged) {
            return Ok((entry.native_id.clone(), Some(staged), true));
        }
    }
    let Some(acquired) = adapter
        .acquire(client, entry)
        .with_context(|| format!("acquiring {} {}", adapter.source_id(), entry.native_id))?
    else {
        return Ok((entry.native_id.clone(), None, false));
    };
    validate_official_url(adapter, &acquired.canonical_url)?;
    let normalized = PreparedDocument {
        source: adapter.source_id().to_owned(),
        native_id: entry.native_id.clone(),
        title: entry.title.clone(),
        document_type: entry.document_type.clone(),
        date: acquired.date.or_else(|| entry.date.clone()),
        citation: entry.citation.clone(),
        canonical_url: acquired.canonical_url,
        cleaned_html: ensure_document_html(&entry.title, acquired.html)?,
        assets: acquired.assets,
    };
    adapter
        .validate_normalized_html(&normalized.cleaned_html)
        .with_context(|| {
            format!(
                "validating normalized {} {}",
                adapter.source_id(),
                entry.native_id
            )
        })?;
    let content_hash = normalized_content_hash(&normalized);
    persist_document(adapter, workspace, &normalized, &content_hash)?;
    let inventory = InventoryEntry {
        native_id: entry.native_id.clone(),
        upstream_version: source_revision,
        canonical_url: normalized.canonical_url,
        content_hash,
    };
    commit_staging_entry(adapter, workspace, &inventory)?;
    Ok((entry.native_id.clone(), Some(inventory), false))
}

fn reusable_entry(
    adapter: &dyn OfficialAdapter,
    workspace: &Path,
    source: &SourceId,
    discovered: &DiscoveredDocument,
    source_revision: &str,
    candidate: &InventoryEntry,
) -> bool {
    candidate.upstream_version == source_revision
        && candidate.canonical_url == discovered.canonical_url
        && load_inventory_document(adapter, workspace, source, candidate)
            .and_then(|document| adapter.validate_normalized_html(&document.html))
            .is_ok()
}

fn discovered_source_revision(
    adapter: &dyn OfficialAdapter,
    entry: &DiscoveredDocument,
) -> Result<String> {
    let bytes = serde_json::to_vec(entry).context("serializing discovered document contract")?;
    let upstream = format!("{}|{}", entry.upstream_version, sha256_bytes(&bytes));
    match adapter.normalization_revision() {
        Some(revision) => {
            validate_text("normalization revision", revision, 128)?;
            Ok(format!("normalizer:{revision}|{upstream}"))
        }
        None => Ok(upstream),
    }
}

struct PreparedDocument {
    source: String,
    native_id: String,
    title: String,
    document_type: String,
    date: Option<String>,
    citation: Option<String>,
    canonical_url: String,
    cleaned_html: String,
    assets: Vec<NormalizedAsset>,
}

fn normalized_content_hash(document: &PreparedDocument) -> String {
    let mut hasher = Sha256::new();
    for field in [
        document.source.as_bytes(),
        document.native_id.as_bytes(),
        document.title.as_bytes(),
        document.document_type.as_bytes(),
        document.date.as_deref().unwrap_or_default().as_bytes(),
        document.citation.as_deref().unwrap_or_default().as_bytes(),
        document.canonical_url.as_bytes(),
        document.cleaned_html.as_bytes(),
    ] {
        hasher.update((field.len() as u64).to_le_bytes());
        hasher.update(field);
    }
    let mut assets = document.assets.iter().collect::<Vec<_>>();
    assets.sort_by(|left, right| left.asset.cmp(&right.asset));
    for asset in assets {
        for field in [
            asset.asset.public_ref().as_bytes(),
            asset.media_type.as_bytes(),
            asset.sha256.as_bytes(),
            asset.data.as_slice(),
        ] {
            hasher.update((field.len() as u64).to_le_bytes());
            hasher.update(field);
        }
    }
    format!("{:x}", hasher.finalize())
}

fn persist_document(
    adapter: &dyn OfficialAdapter,
    workspace: &Path,
    document: &PreparedDocument,
    content_hash: &str,
) -> Result<()> {
    let root = source_root(adapter, workspace)?;
    let mut stored_assets = Vec::new();
    for asset in &document.assets {
        asset.validate()?;
        if asset.data.len() as u64 > MAX_ASSET_BYTES {
            bail!("{} asset exceeds its byte limit", adapter.source_id());
        }
        let digest = sha256_bytes(&asset.data);
        if digest != asset.sha256 {
            bail!(
                "{} asset digest changed before persistence",
                adapter.source_id()
            );
        }
        let relative = PathBuf::from("assets").join(&digest[..2]).join(&digest);
        write_immutable(&root, &relative, &asset.data)?;
        stored_assets.push(StoredAsset {
            asset_id: asset.asset.asset_id.clone(),
            media_type: asset.media_type.clone(),
            relative_path: path_to_slashes(&relative)?,
            size: asset.data.len(),
            sha256: digest,
        });
    }
    stored_assets.sort_by(|left, right| left.asset_id.cmp(&right.asset_id));
    let stored = StoredDocument {
        source: &document.source,
        native_id: &document.native_id,
        title: &document.title,
        document_type: &document.document_type,
        date: &document.date,
        citation: &document.citation,
        canonical_url: &document.canonical_url,
        cleaned_html: &document.cleaned_html,
        assets: stored_assets,
        content_hash,
    };
    let bytes = serde_json::to_vec(&stored).context("serializing normalized official document")?;
    let relative = PathBuf::from("documents")
        .join(&content_hash[..2])
        .join(format!("{content_hash}.json"));
    write_immutable(&root, &relative, &bytes)
}

pub(crate) fn normalized_document_results(
    source_id: &SourceId,
    workspace: &Path,
) -> Result<Box<dyn Iterator<Item = Result<NormalizedDocument>>>> {
    let adapter = adapters()
        .into_iter()
        .find(|adapter| adapter.source_id() == source_id.as_str())
        .ok_or_else(|| anyhow!("source `{source_id}` is not an official web adapter"))?;
    let state = load_state(adapter, workspace)?;
    if state.inventory.is_empty() {
        bail!(
            "{} workspace has no committed inventory",
            adapter.source_id()
        );
    }
    let link_map = state
        .inventory
        .values()
        .map(|entry| {
            let url = Url::parse(&entry.canonical_url)?.to_string();
            let document = DocumentId::new(source_id.clone(), entry.native_id.clone())?;
            Ok((url, document))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    let source_id = source_id.clone();
    let workspace = workspace.to_path_buf();
    Ok(Box::new(state.inventory.into_values().map(move |entry| {
        let mut document = load_inventory_document(adapter, &workspace, &source_id, &entry)?;
        document.html = rewrite_internal_document_links(
            &document.html,
            &document.inventory.canonical_url,
            &link_map,
        )?;
        document.validate()?;
        Ok(document)
    })))
}

#[cfg(test)]
pub(crate) fn seed_test_workspace(source: &SourceId, workspace: &Path) -> Result<()> {
    let adapter = adapters()
        .into_iter()
        .find(|adapter| adapter.source_id() == source.as_str())
        .ok_or_else(|| anyhow!("source `{source}` is not an official web adapter"))?;
    let canonical_url = format!("https://{}/fixture", adapter.approved_hosts()[0]);
    let document = PreparedDocument {
        source: source.as_str().to_owned(),
        native_id: "fixture".to_owned(),
        title: format!("{} fixture", adapter.display_name()),
        document_type: "fixture".to_owned(),
        date: Some("2026-01-01".to_owned()),
        citation: Some(format!("{} fixture", adapter.display_name())),
        canonical_url: canonical_url.clone(),
        cleaned_html: metadata_document(adapter.display_name()),
        assets: Vec::new(),
    };
    let content_hash = normalized_content_hash(&document);
    persist_document(adapter, workspace, &document, &content_hash)?;
    let inventory = InventoryEntry {
        native_id: document.native_id,
        upstream_version: "fixture-v1".to_owned(),
        canonical_url,
        content_hash,
    };
    commit_state(
        adapter,
        workspace,
        &OfficialState {
            schema_version: STATE_SCHEMA_VERSION,
            inventory: BTreeMap::from([(inventory.native_id.clone(), inventory)]),
        },
    )
}

fn load_inventory_document(
    adapter: &dyn OfficialAdapter,
    workspace: &Path,
    source: &SourceId,
    entry: &InventoryEntry,
) -> Result<NormalizedDocument> {
    validate_inventory_entry(adapter, entry)?;
    let root = source_root(adapter, workspace)?;
    let relative = PathBuf::from("documents")
        .join(&entry.content_hash[..2])
        .join(format!("{}.json", entry.content_hash));
    let path = confined_path(&root, &relative)?;
    let bytes = read_bounded_file(&path, MAX_DOCUMENT_BYTES)?;
    let stored: StoredDocumentOwned = serde_json::from_slice(&bytes).with_context(|| {
        format!(
            "decoding {} document {}",
            adapter.source_id(),
            entry.native_id
        )
    })?;
    if stored.source != adapter.source_id()
        || stored.native_id != entry.native_id
        || stored.content_hash != entry.content_hash
        || stored.canonical_url != entry.canonical_url
    {
        bail!(
            "{} stored document does not match inventory",
            entry.native_id
        );
    }
    let mut assets = Vec::new();
    for stored_asset in &stored.assets {
        let asset_path = confined_path(&root, Path::new(&stored_asset.relative_path))?;
        let data = read_bounded_file(&asset_path, MAX_ASSET_BYTES)?;
        if data.len() != stored_asset.size || sha256_bytes(&data) != stored_asset.sha256 {
            bail!(
                "{} asset {} failed integrity validation",
                adapter.source_id(),
                stored_asset.asset_id
            );
        }
        assets.push(NormalizedAsset::new(
            AssetRef::new(source.clone(), stored_asset.asset_id.clone())?,
            stored_asset.media_type.clone(),
            None,
            None,
            stored_asset.sha256.clone(),
            data,
        )?);
    }
    let prepared = PreparedDocument {
        source: stored.source.clone(),
        native_id: stored.native_id.clone(),
        title: stored.title.clone(),
        document_type: stored.document_type.clone(),
        date: stored.date.clone(),
        citation: stored.citation.clone(),
        canonical_url: stored.canonical_url.clone(),
        cleaned_html: stored.cleaned_html.clone(),
        assets: assets.clone(),
    };
    if normalized_content_hash(&prepared) != entry.content_hash {
        bail!(
            "{} document {} failed content integrity validation",
            adapter.source_id(),
            entry.native_id
        );
    }
    let inventory = SourceInventoryRecord::new(
        DocumentId::new(source.clone(), stored.native_id)?,
        Some(entry.upstream_version.clone()),
        stored.canonical_url,
        stored.document_type,
        stored.title,
        stored.date,
        path_to_slashes(&PathBuf::from(adapter.source_id()).join(relative))?,
        sha256_bytes(&bytes),
        bytes.len() as u64,
        NORMALIZED_MEDIA_TYPE,
    )?;
    NormalizedDocument::new(inventory, stored.cleaned_html, assets).with_context(|| {
        format!(
            "validating {} document {}",
            adapter.source_id(),
            entry.native_id
        )
    })
}

fn load_state(adapter: &dyn OfficialAdapter, workspace: &Path) -> Result<OfficialState> {
    let root = source_root(adapter, workspace)?;
    let path = confined_path(&root, Path::new("state.json"))?;
    if !path.exists() {
        return Ok(OfficialState {
            schema_version: STATE_SCHEMA_VERSION,
            inventory: BTreeMap::new(),
        });
    }
    let bytes = read_bounded_file(&path, MAX_STATE_BYTES)?;
    let state: OfficialState = serde_json::from_slice(&bytes)
        .with_context(|| format!("decoding {} state", adapter.source_id()))?;
    if state.schema_version != STATE_SCHEMA_VERSION {
        bail!("{} state schema is unsupported", adapter.source_id());
    }
    for (native_id, entry) in &state.inventory {
        if native_id != &entry.native_id {
            bail!("{} state inventory key mismatch", adapter.source_id());
        }
        validate_inventory_entry(adapter, entry)?;
    }
    Ok(state)
}

fn commit_state(
    adapter: &dyn OfficialAdapter,
    workspace: &Path,
    state: &OfficialState,
) -> Result<()> {
    let root = source_root(adapter, workspace)?;
    let bytes = serde_json::to_vec(state).context("serializing official-source state")?;
    if bytes.len() as u64 > MAX_STATE_BYTES {
        bail!("{} state exceeds its byte limit", adapter.source_id());
    }
    atomic_write(&confined_path(&root, Path::new("state.json"))?, &bytes)
}

fn load_staging_entry(
    adapter: &dyn OfficialAdapter,
    workspace: &Path,
    native_id: &str,
) -> Result<Option<InventoryEntry>> {
    let root = source_root(adapter, workspace)?;
    let relative = staging_relative_path(native_id);
    let path = confined_path(&root, &relative)?;
    if !path.exists() {
        return Ok(None);
    }
    let bytes = read_bounded_file(&path, 1024 * 1024)?;
    let entry: InventoryEntry = serde_json::from_slice(&bytes)
        .with_context(|| format!("decoding {} staged acquisition", adapter.source_id()))?;
    validate_inventory_entry(adapter, &entry)?;
    if entry.native_id != native_id {
        bail!(
            "{} staged acquisition identity mismatch",
            adapter.source_id()
        );
    }
    Ok(Some(entry))
}

fn commit_staging_entry(
    adapter: &dyn OfficialAdapter,
    workspace: &Path,
    entry: &InventoryEntry,
) -> Result<()> {
    validate_inventory_entry(adapter, entry)?;
    let root = source_root(adapter, workspace)?;
    let bytes = serde_json::to_vec(entry).context("serializing staged acquisition")?;
    atomic_write(
        &confined_path(&root, &staging_relative_path(&entry.native_id))?,
        &bytes,
    )
}

fn staging_relative_path(native_id: &str) -> PathBuf {
    let digest = sha256_bytes(native_id.as_bytes());
    PathBuf::from("staging")
        .join(&digest[..2])
        .join(format!("{digest}.json"))
}

fn clear_staging(adapter: &dyn OfficialAdapter, workspace: &Path) -> Result<()> {
    let root = source_root(adapter, workspace)?;
    let staging = confined_path(&root, Path::new("staging"))?;
    if !staging.exists() {
        return Ok(());
    }
    let metadata = fs::symlink_metadata(&staging)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "{} staging path is not a real directory",
            adapter.source_id()
        );
    }
    fs::remove_dir_all(&staging)
        .with_context(|| format!("removing {} completed staging data", adapter.source_id()))
}

fn validate_inventory_entry(adapter: &dyn OfficialAdapter, entry: &InventoryEntry) -> Result<()> {
    let source: SourceId = adapter.source_id().parse()?;
    DocumentId::new(source, entry.native_id.clone())?;
    validate_text("upstream version", &entry.upstream_version, 32 * 1024)?;
    validate_official_url(adapter, &entry.canonical_url)?;
    validate_sha256(&entry.content_hash)
}

fn fingerprint_inventory(
    inventory: &BTreeMap<String, InventoryEntry>,
) -> Result<SourceInventoryFingerprint> {
    let mut hasher = Sha256::new();
    for entry in inventory.values() {
        let bytes = serde_json::to_vec(entry).context("serializing inventory fingerprint")?;
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    }
    Ok(SourceInventoryFingerprint {
        records: inventory.len(),
        sha256: format!("{:x}", hasher.finalize()),
    })
}

pub(super) struct OfficialHttpClient {
    client: Client,
    adapter: &'static dyn OfficialAdapter,
    browser: Option<BrowserHttpTransport>,
    concurrency: AdaptiveConcurrency,
    next_request: Mutex<Instant>,
}

pub(super) struct HttpPayload {
    pub(super) final_url: Url,
    pub(super) status: StatusCode,
    pub(super) content_type: Option<String>,
    pub(super) bytes: Vec<u8>,
}

impl OfficialHttpClient {
    fn new(adapter: &'static dyn OfficialAdapter) -> Result<Self> {
        let policy = adapter.rate_policy();
        let timeout = Duration::from_secs(policy.request_timeout_seconds);
        let client = Client::builder()
            .no_proxy()
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::none())
            .cookie_store(true)
            .user_agent("australian-legal-mcp/official-source-adapter")
            .build()
            .context("building official-source HTTP client")?;
        let browser = adapter
            .has_browser_transport()
            .then(|| BrowserHttpTransport::new(MAX_DOCUMENT_BYTES))
            .transpose()?;
        Ok(Self {
            client,
            adapter,
            browser,
            concurrency: AdaptiveConcurrency::new(adapter.source_id()),
            next_request: Mutex::new(Instant::now()),
        })
    }

    pub(super) fn get(&self, url: &str, accept: &str, limit: u64) -> Result<HttpPayload> {
        let mut current = Url::parse(url).with_context(|| format!("parsing official URL {url}"))?;
        self.validate_url(&current)?;
        if self.adapter.use_browser_transport(&current) {
            return self.get_with_browser(current, limit);
        }
        let mut redirects = 0usize;
        'request: loop {
            let mut last_error = None;
            for attempt in 0..MAX_RATE_LIMIT_ATTEMPTS {
                let request = self.concurrency.acquire()?;
                let pacing_wait = self.wait_for_issue_slot(&current)?;
                let sent = self
                    .client
                    .get(current.clone())
                    .header(ACCEPT, accept)
                    .send();
                match sent {
                    Ok(response) => {
                        let status = response.status();
                        if status.is_redirection() {
                            if redirects >= MAX_REDIRECTS {
                                bail!("official request exceeded {MAX_REDIRECTS} redirects");
                            }
                            let location = response
                                .headers()
                                .get(LOCATION)
                                .and_then(|value| value.to_str().ok())
                                .ok_or_else(|| {
                                    anyhow!("official redirect has no valid Location header")
                                })?;
                            let next = current
                                .join(location)
                                .context("resolving official redirect")?;
                            request.finish(
                                current.as_str(),
                                Some(status.as_u16()),
                                0,
                                attempt + 1,
                                RequestOutcome::Success,
                                pacing_wait,
                                None,
                            );
                            self.validate_url(&next)?;
                            current = next;
                            redirects += 1;
                            continue 'request;
                        }

                        let attempt_limit = if status == StatusCode::TOO_MANY_REQUESTS {
                            MAX_RATE_LIMIT_ATTEMPTS
                        } else {
                            MAX_HTTP_ATTEMPTS
                        };
                        if retryable_http_status(status) && attempt + 1 < attempt_limit {
                            let delay = retry_after(&response).unwrap_or_else(|| {
                                if status == StatusCode::FORBIDDEN {
                                    Duration::from_secs(10 * (attempt as u64 + 1))
                                } else {
                                    retry_delay(attempt)
                                }
                            });
                            if delay > MAX_RETRY_DELAY {
                                bail!(
                                    "official Retry-After exceeds {} seconds",
                                    MAX_RETRY_DELAY.as_secs()
                                );
                            }
                            self.defer_requests(delay)?;
                            request.finish(
                                current.as_str(),
                                Some(status.as_u16()),
                                0,
                                attempt + 1,
                                retryable_status_outcome(status),
                                pacing_wait,
                                Some(delay),
                            );
                            last_error = Some(format!("HTTP {status}"));
                            continue;
                        }

                        let content_type = response
                            .headers()
                            .get(CONTENT_TYPE)
                            .and_then(|value| value.to_str().ok())
                            .map(|value| {
                                value
                                    .split(';')
                                    .next()
                                    .unwrap_or(value)
                                    .trim()
                                    .to_ascii_lowercase()
                            });
                        match read_response(response, limit) {
                            Ok(bytes) if is_official_antibot_challenge(&bytes) => {
                                let delay = retry_delay(attempt);
                                self.defer_requests(delay)?;
                                request.finish(
                                    current.as_str(),
                                    Some(status.as_u16()),
                                    bytes.len(),
                                    attempt + 1,
                                    RequestOutcome::Congestion,
                                    pacing_wait,
                                    Some(delay),
                                );
                                if attempt + 1 >= MAX_RATE_LIMIT_ATTEMPTS {
                                    bail!("official anti-bot challenge exhausted retries");
                                }
                                last_error = Some("official anti-bot challenge".to_owned());
                            }
                            Ok(bytes) => {
                                request.finish(
                                    current.as_str(),
                                    Some(status.as_u16()),
                                    bytes.len(),
                                    attempt + 1,
                                    if status.is_success() {
                                        RequestOutcome::Success
                                    } else {
                                        RequestOutcome::Neutral
                                    },
                                    pacing_wait,
                                    None,
                                );
                                return Ok(HttpPayload {
                                    final_url: current,
                                    status,
                                    content_type,
                                    bytes,
                                });
                            }
                            Err(error) => {
                                let retryable = retryable_response_read_error(&error);
                                if retryable && attempt + 1 < MAX_HTTP_ATTEMPTS {
                                    let delay = retry_delay(attempt);
                                    self.defer_requests(delay)?;
                                    request.finish(
                                        current.as_str(),
                                        Some(status.as_u16()),
                                        0,
                                        attempt + 1,
                                        RequestOutcome::Transient,
                                        pacing_wait,
                                        Some(delay),
                                    );
                                    last_error = Some(error.to_string());
                                    continue;
                                }
                                request.finish(
                                    current.as_str(),
                                    Some(status.as_u16()),
                                    0,
                                    attempt + 1,
                                    if retryable {
                                        RequestOutcome::Transient
                                    } else {
                                        RequestOutcome::Neutral
                                    },
                                    pacing_wait,
                                    None,
                                );
                                return Err(error);
                            }
                        }
                    }
                    Err(error) => {
                        let retryable =
                            error.is_timeout() || error.is_connect() || error.is_request();
                        if retryable && attempt + 1 < MAX_HTTP_ATTEMPTS {
                            let delay = retry_delay(attempt);
                            self.defer_requests(delay)?;
                            request.finish(
                                current.as_str(),
                                None,
                                0,
                                attempt + 1,
                                RequestOutcome::Transient,
                                pacing_wait,
                                Some(delay),
                            );
                            last_error = Some(error.to_string());
                            continue;
                        }
                        request.finish(
                            current.as_str(),
                            None,
                            0,
                            attempt + 1,
                            if retryable {
                                RequestOutcome::Transient
                            } else {
                                RequestOutcome::Neutral
                            },
                            pacing_wait,
                            None,
                        );
                        return Err(error).with_context(|| format!("requesting {current}"));
                    }
                }
            }
            bail!(
                "official request {current} exhausted retries: {}",
                last_error.unwrap_or_else(|| "unknown error".to_owned())
            );
        }
    }

    pub(super) fn get_required(&self, url: &str, accept: &str, limit: u64) -> Result<HttpPayload> {
        let payload = self.get(url, accept, limit)?;
        if !payload.status.is_success() {
            bail!(
                "official request {} returned HTTP {}",
                payload.final_url,
                payload.status
            );
        }
        Ok(payload)
    }

    fn get_with_browser(&self, url: Url, limit: u64) -> Result<HttpPayload> {
        let browser = self
            .browser
            .as_ref()
            .ok_or_else(|| anyhow!("browser transport was not initialized"))?;
        let mut last_error = None;
        for attempt in 0..MAX_RATE_LIMIT_ATTEMPTS {
            let request = self.concurrency.acquire()?;
            let pacing_wait = self.wait_for_issue_slot(&url)?;
            match browser.get(&url, limit) {
                Ok(response) => {
                    let status = StatusCode::from_u16(response.status)
                        .context("browser transport returned an invalid HTTP status")?;
                    let attempt_limit = if status == StatusCode::TOO_MANY_REQUESTS {
                        MAX_RATE_LIMIT_ATTEMPTS
                    } else {
                        MAX_HTTP_ATTEMPTS
                    };
                    if retryable_http_status(status) && attempt + 1 < attempt_limit {
                        let delay = if status == StatusCode::FORBIDDEN {
                            Duration::from_secs(10 * (attempt as u64 + 1))
                        } else {
                            retry_delay(attempt)
                        };
                        self.defer_requests(delay)?;
                        request.finish(
                            url.as_str(),
                            Some(status.as_u16()),
                            response.bytes.len(),
                            attempt + 1,
                            retryable_status_outcome(status),
                            pacing_wait,
                            Some(delay),
                        );
                        last_error = Some(format!("HTTP {status}"));
                        continue;
                    }
                    self.validate_url(&response.final_url)?;
                    request.finish(
                        url.as_str(),
                        Some(status.as_u16()),
                        response.bytes.len(),
                        attempt + 1,
                        if status.is_success() {
                            RequestOutcome::Success
                        } else {
                            RequestOutcome::Neutral
                        },
                        pacing_wait,
                        None,
                    );
                    return Ok(HttpPayload {
                        final_url: response.final_url,
                        status,
                        content_type: response.content_type,
                        bytes: response.bytes,
                    });
                }
                Err(error) => {
                    let delay = retry_delay(attempt);
                    let retryable = attempt + 1 < MAX_HTTP_ATTEMPTS;
                    if retryable {
                        self.defer_requests(delay)?;
                    }
                    request.finish(
                        url.as_str(),
                        None,
                        0,
                        attempt + 1,
                        RequestOutcome::Transient,
                        pacing_wait,
                        retryable.then_some(delay),
                    );
                    if !retryable {
                        return Err(error).context("browser source request failed");
                    }
                    last_error = Some(error.to_string());
                }
            }
        }
        bail!(
            "browser source request {url} exhausted retries: {}",
            last_error.unwrap_or_else(|| "unknown error".to_owned())
        )
    }

    fn wait_for_issue_slot(&self, url: &Url) -> Result<Duration> {
        let started = Instant::now();
        let minimum_interval = Duration::from_millis(self.adapter.minimum_request_interval_ms(url));
        let mut next = self
            .next_request
            .lock()
            .map_err(|_| anyhow!("official request pacing lock is poisoned"))?;
        let now = Instant::now();
        if now < *next {
            thread::sleep(*next - now);
        }
        *next = Instant::now() + minimum_interval;
        Ok(started.elapsed())
    }

    fn defer_requests(&self, delay: Duration) -> Result<()> {
        let mut next = self
            .next_request
            .lock()
            .map_err(|_| anyhow!("official request pacing lock is poisoned"))?;
        let candidate = Instant::now() + delay;
        if candidate > *next {
            *next = candidate;
        }
        Ok(())
    }

    fn validate_url(&self, url: &Url) -> Result<()> {
        if url.scheme() != "https"
            || !url.username().is_empty()
            || url.password().is_some()
            || url.port().is_some()
            || url.host_str().is_none()
        {
            bail!("unsafe official URL {url}");
        }
        let host = url.host_str().unwrap_or_default();
        if !self.adapter.approved_hosts().contains(&host) {
            bail!("official URL host `{host}` is not approved");
        }
        Ok(())
    }
}

fn read_response(response: Response, limit: u64) -> Result<Vec<u8>> {
    if response
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|size| size > limit)
    {
        bail!("official response exceeds its {limit}-byte limit");
    }
    let mut bytes = Vec::new();
    response
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .context("reading official response")?;
    if bytes.len() as u64 > limit {
        bail!("official response exceeds its {limit}-byte limit");
    }
    Ok(bytes)
}

fn retryable_response_read_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<reqwest::Error>().is_some_and(|error| {
            error.is_timeout() || error.is_connect() || error.is_request() || error.is_body()
        }) || cause.downcast_ref::<std::io::Error>().is_some_and(|error| {
            matches!(
                error.kind(),
                std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::WouldBlock
            )
        })
    })
}

fn retryable_http_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::TOO_MANY_REQUESTS
            | StatusCode::FORBIDDEN
            | StatusCode::INTERNAL_SERVER_ERROR
            | StatusCode::BAD_GATEWAY
            | StatusCode::SERVICE_UNAVAILABLE
            | StatusCode::GATEWAY_TIMEOUT
    )
}

fn retryable_status_outcome(status: StatusCode) -> RequestOutcome {
    if matches!(
        status,
        StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS
    ) {
        RequestOutcome::Congestion
    } else {
        RequestOutcome::Transient
    }
}

fn is_official_antibot_challenge(bytes: &[u8]) -> bool {
    bytes
        .windows(b"Enable JavaScript and cookies to continue".len())
        .any(|window| window == b"Enable JavaScript and cookies to continue")
}

fn retry_after(response: &Response) -> Option<Duration> {
    response
        .headers()
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs)
}

fn retry_delay(attempt: usize) -> Duration {
    Duration::from_millis(500 * (1u64 << attempt.min(7)))
}

pub(super) fn parallel_map<T, U, F>(
    concurrency: usize,
    items: Vec<T>,
    operation: F,
) -> Result<Vec<U>>
where
    T: Send,
    U: Send,
    F: Fn(T) -> Result<U> + Send + Sync,
{
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(concurrency)
        .build()
        .context("building source discovery pool")?;
    pool.install(|| items.into_par_iter().map(operation).collect())
}

fn scoped_thread_map<T, U, F>(concurrency: usize, items: Vec<T>, operation: F) -> Result<Vec<U>>
where
    T: Sync,
    U: Send,
    F: Fn(&T) -> Result<U> + Sync,
{
    let next = AtomicUsize::new(0);
    let (sender, receiver) = std::sync::mpsc::channel();
    let mut results = thread::scope(|scope| {
        for _ in 0..concurrency.min(items.len()) {
            let sender = sender.clone();
            let next = &next;
            let items = &items;
            let operation = &operation;
            scope.spawn(move || loop {
                let index = next.fetch_add(1, Ordering::Relaxed);
                let Some(item) = items.get(index) else {
                    break;
                };
                if sender.send((index, operation(item))).is_err() {
                    break;
                }
            });
        }
        drop(sender);
        receiver.into_iter().collect::<Vec<_>>()
    });
    if results.len() != items.len() {
        bail!("bounded thread workers ended before processing every item");
    }
    results.sort_by_key(|(index, _)| *index);
    results.into_iter().map(|(_, result)| result).collect()
}

pub(super) fn decode(bytes: &[u8], encoding: &'static Encoding) -> Result<String> {
    let (text, _, had_errors) = encoding.decode(bytes);
    if had_errors {
        bail!("official response is not valid {} text", encoding.name());
    }
    Ok(text.into_owned())
}

pub(super) fn decode_utf8(bytes: &[u8]) -> Result<String> {
    decode(bytes, UTF_8)
}

pub(super) fn decode_windows_1252(bytes: &[u8]) -> String {
    WINDOWS_1252.decode(bytes).0.into_owned()
}

pub(super) struct HtmlRules<'a> {
    pub(super) content_selector: &'a str,
    pub(super) drop_ids: &'a [&'a str],
    pub(super) drop_classes: &'a [&'a str],
    pub(super) heading_classes: &'a [&'a str],
    pub(super) preserve_same_document_fragments: bool,
    pub(super) repair_broken_links: bool,
}

pub(super) fn normalize_html(html: &str, base_url: &str, rules: HtmlRules<'_>) -> Result<String> {
    let parsed = Html::parse_document(html);
    let selector = Selector::parse(rules.content_selector).map_err(|_| {
        anyhow!(
            "invalid official content selector {}",
            rules.content_selector
        )
    })?;
    let root = parsed
        .select(&selector)
        .next()
        .ok_or_else(|| anyhow!("official document lacks {}", rules.content_selector))?;
    let base = Url::parse(base_url).context("parsing document base URL")?;
    let fragment_targets = collect_fragment_targets(root)?;
    let mut output = String::from("<article>");
    for child in root.children() {
        serialize_html_node(child, &base, &rules, None, &fragment_targets, &mut output)?;
    }
    output.push_str("</article>");
    ensure_nonempty_html(&output)?;
    Ok(output)
}

pub(crate) fn rewrite_internal_document_links(
    html: &str,
    base_url: &str,
    links: &BTreeMap<String, DocumentId>,
) -> Result<String> {
    let parsed = Html::parse_document(html);
    let selector =
        Selector::parse("article").map_err(|_| anyhow!("invalid normalized article selector"))?;
    let root = parsed
        .select(&selector)
        .next()
        .ok_or_else(|| anyhow!("normalized document lacks article root"))?;
    let base = Url::parse(base_url)?;
    let fragment_targets = collect_fragment_targets(root)?;
    let mut output = String::from("<article>");
    let rules = HtmlRules {
        content_selector: "article",
        drop_ids: &[],
        drop_classes: &[],
        heading_classes: &[],
        preserve_same_document_fragments: true,
        repair_broken_links: false,
    };
    for child in root.children() {
        serialize_html_node(
            child,
            &base,
            &rules,
            Some(links),
            &fragment_targets,
            &mut output,
        )?;
    }
    output.push_str("</article>");
    ensure_nonempty_html(&output)?;
    Ok(output)
}

fn collect_fragment_targets(root: scraper::ElementRef<'_>) -> Result<BTreeSet<String>> {
    let selector = Selector::parse("[id], a[name]")
        .map_err(|_| anyhow!("invalid official fragment-target selector"))?;
    Ok(root
        .select(&selector)
        .flat_map(|element| {
            [element.value().attr("id"), element.value().attr("name")]
                .into_iter()
                .flatten()
                .map(str::to_owned)
        })
        .collect())
}

enum NormalizedLinkTarget {
    Href(String),
    Document(String),
}

fn normalized_link_target(
    element: &Element,
    base: &Url,
    rules: &HtmlRules<'_>,
    links: Option<&BTreeMap<String, DocumentId>>,
    fragment_targets: &BTreeSet<String>,
) -> Result<Option<NormalizedLinkTarget>> {
    let Some(href) = element.attr("href") else {
        return Ok(None);
    };
    let Ok(mut url) = base.join(href) else {
        return Ok(None);
    };
    if rules.repair_broken_links
        && url.scheme() == "http"
        && base.scheme() == "https"
        && url.host_str() == base.host_str()
        && url.port().is_none()
    {
        url.set_scheme("https")
            .map_err(|_| anyhow!("upgrading official hyperlink to HTTPS"))?;
    }
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return Ok(None);
    }

    let fragment = url.fragment().map(str::to_owned);
    url.set_fragment(None);
    let mut document_url = base.clone();
    document_url.set_fragment(None);
    if rules.preserve_same_document_fragments && url == document_url {
        if let Some(fragment) = fragment {
            return Ok(
                (fragment_targets.contains(&fragment) || !rules.repair_broken_links)
                    .then(|| NormalizedLinkTarget::Href(format!("#{fragment}"))),
            );
        }
    }
    if let Some(document) = links.and_then(|links| links.get(url.as_str())) {
        return Ok(Some(NormalizedLinkTarget::Document(document.to_string())));
    }
    Ok(Some(NormalizedLinkTarget::Href(url.to_string())))
}

fn serialize_html_node(
    node: NodeRef<'_, Node>,
    base: &Url,
    rules: &HtmlRules<'_>,
    links: Option<&BTreeMap<String, DocumentId>>,
    fragment_targets: &BTreeSet<String>,
    output: &mut String,
) -> Result<()> {
    match node.value() {
        Node::Text(text) => escape_text(text, output),
        Node::Element(element) => {
            if rules.drop_ids.iter().any(|id| element.id() == Some(*id))
                || rules
                    .drop_classes
                    .iter()
                    .any(|class| element.has_class(class, CaseSensitivity::AsciiCaseInsensitive))
                || matches!(
                    element.name(),
                    "script" | "style" | "noscript" | "template" | "form" | "button" | "nav"
                )
            {
                return Ok(());
            }
            let mut tag = element.name();
            if tag == "blockquote"
                && rules
                    .heading_classes
                    .iter()
                    .any(|class| element.has_class(class, CaseSensitivity::AsciiCaseInsensitive))
            {
                tag = "h1";
            }
            let allowed = matches!(
                tag,
                "a" | "abbr"
                    | "article"
                    | "b"
                    | "blockquote"
                    | "br"
                    | "caption"
                    | "cite"
                    | "code"
                    | "dd"
                    | "del"
                    | "div"
                    | "dl"
                    | "dt"
                    | "em"
                    | "figcaption"
                    | "figure"
                    | "h1"
                    | "h2"
                    | "h3"
                    | "h4"
                    | "h5"
                    | "h6"
                    | "hr"
                    | "i"
                    | "img"
                    | "ins"
                    | "li"
                    | "ol"
                    | "p"
                    | "pre"
                    | "q"
                    | "s"
                    | "section"
                    | "small"
                    | "span"
                    | "strong"
                    | "sub"
                    | "sup"
                    | "table"
                    | "tbody"
                    | "td"
                    | "tfoot"
                    | "th"
                    | "thead"
                    | "tr"
                    | "u"
                    | "ul"
            );
            let link_target = (tag == "a")
                .then(|| normalized_link_target(element, base, rules, links, fragment_targets))
                .transpose()?
                .flatten();
            let emit_element = allowed
                && (tag != "a"
                    || link_target.is_some()
                    || element.attr("id").is_some()
                    || element.attr("name").is_some()
                    || !rules.repair_broken_links);
            if emit_element {
                output.push('<');
                output.push_str(tag);
                for name in ["id", "name", "colspan", "rowspan"] {
                    if let Some(value) = element.attr(name) {
                        output.push(' ');
                        output.push_str(name);
                        output.push_str("=\"");
                        escape_attribute(value, output);
                        output.push('"');
                    }
                }
                if tag == "img" {
                    for name in ["data-asset-ref", "data-media-type", "alt", "title"] {
                        if let Some(value) = element.attr(name) {
                            output.push(' ');
                            output.push_str(name);
                            output.push_str("=\"");
                            escape_attribute(value, output);
                            output.push('"');
                        }
                    }
                }
                match link_target {
                    Some(NormalizedLinkTarget::Href(ref href)) => {
                        output.push_str(" href=\"");
                        escape_attribute(href, output);
                        output.push('"');
                    }
                    Some(NormalizedLinkTarget::Document(ref document)) => {
                        output.push_str(" data-doc-id=\"");
                        escape_attribute(document, output);
                        output.push('"');
                    }
                    None => {}
                }
                output.push('>');
            }
            for child in node.children() {
                serialize_html_node(child, base, rules, links, fragment_targets, output)?;
            }
            if emit_element && !matches!(tag, "br" | "hr" | "img") {
                output.push_str("</");
                output.push_str(tag);
                output.push('>');
            }
        }
        _ => {
            for child in node.children() {
                serialize_html_node(child, base, rules, links, fragment_targets, output)?;
            }
        }
    }
    Ok(())
}

pub(super) fn normalize_text(text: &str) -> Result<String> {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut output = String::from("<article>");
    for block in text.split("\n\n") {
        let collapsed = block.split_whitespace().collect::<Vec<_>>().join(" ");
        if collapsed.is_empty() {
            continue;
        }
        output.push_str("<p>");
        escape_text(&collapsed, &mut output);
        output.push_str("</p>");
    }
    output.push_str("</article>");
    ensure_nonempty_html(&output)?;
    Ok(output)
}

pub(super) fn normalize_pdf(bytes: &[u8]) -> Result<String> {
    let direct_error = match extract_pdf_text(bytes) {
        Ok(extracted) => match normalize_extracted_pdf_text(&extracted) {
            Ok(html) => return Ok(html),
            Err(error) => error,
        },
        Err(error) => error,
    };
    let ocr = ocr_pdf(bytes)
        .with_context(|| format!("direct PDF text extraction failed: {direct_error:#}"))?;
    normalize_extracted_pdf_text(&ocr).context("PDF OCR produced no indexable text")
}

fn normalize_extracted_pdf_text(text: &str) -> Result<String> {
    if text
        .chars()
        .filter(|character| character.is_alphanumeric())
        .count()
        < MIN_PDF_TEXT_ALPHANUMERIC_CHARS
    {
        bail!("PDF extraction produced too little substantive text");
    }
    normalize_text(text)
}

fn extract_pdf_text(bytes: &[u8]) -> Result<String> {
    let temp = tempfile::tempdir().context("creating PDF text workspace")?;
    let input = temp.path().join("input.pdf");
    let output = temp.path().join("output.txt");
    fs::write(&input, bytes).context("writing PDF text input")?;
    let mut command = ProcessCommand::new("pdftotext");
    command
        .arg("-layout")
        .arg("-enc")
        .arg("UTF-8")
        .arg(&input)
        .arg(&output)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    run_command_with_timeout(command, Duration::from_secs(5 * 60), "pdftotext")?;
    let text = read_bounded_file(&output, 64 * 1024 * 1024)?;
    String::from_utf8(text).context("decoding official PDF text")
}

fn ocr_pdf(bytes: &[u8]) -> Result<String> {
    let _guard = OCR_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|_| anyhow!("PDF OCR lock is poisoned"))?;
    let temp = tempfile::tempdir().context("creating PDF OCR workspace")?;
    let input = temp.path().join("input.pdf");
    fs::write(&input, bytes).context("writing PDF OCR input")?;
    let prefix = temp.path().join("page");
    let mut render = ProcessCommand::new("pdftoppm");
    render
        .arg("-jpeg")
        .arg("-r")
        .arg("180")
        .arg(&input)
        .arg(&prefix)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    run_command_with_timeout(render, Duration::from_secs(10 * 60), "pdftoppm")?;
    let mut pages = fs::read_dir(temp.path())?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("jpg"))
        .collect::<Vec<_>>();
    pages.sort();
    if pages.is_empty() || pages.len() > 2_000 {
        bail!("PDF OCR produced an invalid page count");
    }
    let concurrency = thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .saturating_sub(1)
        .clamp(1, 8);
    let texts = scoped_thread_map(concurrency, pages, |page| {
        let output_base = page.with_extension("ocr");
        let mut command = ProcessCommand::new("tesseract");
        command
            .arg(page)
            .arg(&output_base)
            .arg("--psm")
            .arg("6")
            .arg("-l")
            .arg("eng")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        run_command_with_timeout(command, Duration::from_secs(5 * 60), "tesseract")?;
        let output_text = PathBuf::from(format!("{}.txt", output_base.display()));
        read_bounded_file(&output_text, 8 * 1024 * 1024)
            .and_then(|bytes| String::from_utf8(bytes).context("decoding OCR text"))
    })?;
    let text = texts.join("\n\n");
    if text.len() > 64 * 1024 * 1024 {
        bail!("PDF OCR text exceeds its byte limit");
    }
    Ok(text)
}

pub(crate) fn ocr_image_to_text(bytes: &[u8]) -> Result<String> {
    let _guard = OCR_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|_| anyhow!("OCR lock is poisoned"))?;
    let temp = tempfile::tempdir().context("creating image OCR workspace")?;
    let input = temp.path().join("input.image");
    let output_base = temp.path().join("output");
    fs::write(&input, bytes).context("writing image OCR input")?;
    let mut command = ProcessCommand::new("tesseract");
    command
        .arg(&input)
        .arg(&output_base)
        .arg("--psm")
        .arg("6")
        .arg("-l")
        .arg("eng")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    run_command_with_timeout(command, Duration::from_secs(5 * 60), "tesseract")?;
    let output_text = PathBuf::from(format!("{}.txt", output_base.display()));
    let text = String::from_utf8(read_bounded_file(&output_text, 8 * 1024 * 1024)?)
        .context("decoding image OCR text")?;
    if text
        .chars()
        .filter(|character| character.is_alphanumeric())
        .count()
        < 16
    {
        bail!("official document image yielded no usable OCR text");
    }
    Ok(text)
}

fn run_command_with_timeout(
    mut command: ProcessCommand,
    timeout: Duration,
    label: &str,
) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("starting required document conversion command `{label}`"))?;
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            if status.success() {
                return Ok(());
            }
            bail!("document conversion command `{label}` exited with {status}");
        }
        if started.elapsed() >= timeout {
            terminate_command_process_tree(&mut child);
            bail!("document conversion command `{label}` exceeded its timeout");
        }
        thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(target_os = "linux")]
fn sandboxed_soffice_command(workspace: &Path) -> Result<ProcessCommand> {
    let workspace = workspace
        .canonicalize()
        .context("canonicalizing LibreOffice sandbox workspace")?;
    if !Path::new("/usr/bin/bwrap").is_file() || !Path::new("/usr/bin/soffice").is_file() {
        bail!("sandboxed LibreOffice requires /usr/bin/bwrap and /usr/bin/soffice");
    }
    for path in ["/usr", "/etc/fonts", "/etc/ld.so.cache"] {
        if !Path::new(path).exists() {
            bail!("LibreOffice sandbox requires {path}");
        }
    }
    let sandbox_tmp = workspace.join("tmp");
    fs::create_dir(&sandbox_tmp).context("creating LibreOffice sandbox temp directory")?;
    let mut command = ProcessCommand::new("/usr/bin/bwrap");
    command
        .arg("--unshare-all")
        .arg("--die-with-parent")
        .arg("--new-session")
        .arg("--tmpfs")
        .arg("/")
        .arg("--dir")
        .arg("/usr")
        .arg("--ro-bind")
        .arg("/usr")
        .arg("/usr")
        .arg("--symlink")
        .arg("usr/bin")
        .arg("/bin")
        .arg("--symlink")
        .arg("usr/lib")
        .arg("/lib")
        .arg("--symlink")
        .arg("usr/lib64")
        .arg("/lib64")
        .arg("--dir")
        .arg("/etc")
        .arg("--dir")
        .arg("/etc/fonts")
        .arg("--ro-bind")
        .arg("/etc/fonts")
        .arg("/etc/fonts")
        .arg("--ro-bind")
        .arg("/etc/ld.so.cache")
        .arg("/etc/ld.so.cache")
        .arg("--dir")
        .arg("/tmp")
        .arg("--dir")
        .arg("/var")
        .arg("--dir")
        .arg("/var/cache")
        .arg("--dir")
        .arg("/var/cache/fontconfig")
        .arg("--dir")
        .arg(&workspace)
        .arg("--bind")
        .arg(&workspace)
        .arg(&workspace)
        .arg("--proc")
        .arg("/proc")
        .arg("--dev")
        .arg("/dev")
        .arg("--chdir")
        .arg(&workspace)
        .arg("--clearenv")
        .arg("--setenv")
        .arg("PATH")
        .arg("/usr/bin")
        .arg("--setenv")
        .arg("LANG")
        .arg("C.UTF-8")
        .arg("--setenv")
        .arg("SAL_USE_VCLPLUGIN")
        .arg("svp")
        .arg("--setenv")
        .arg("HOME")
        .arg(&workspace)
        .arg("--setenv")
        .arg("TMPDIR")
        .arg(&sandbox_tmp)
        .arg("--setenv")
        .arg("XDG_CONFIG_HOME")
        .arg(workspace.join("config"))
        .arg("--setenv")
        .arg("XDG_CACHE_HOME")
        .arg(workspace.join("cache"))
        .arg("--")
        .arg("/usr/bin/soffice");
    Ok(command)
}

#[cfg(not(target_os = "linux"))]
fn sandboxed_soffice_command(_workspace: &Path) -> Result<ProcessCommand> {
    bail!("sandboxed LibreOffice conversion is supported only on Linux")
}

#[cfg(unix)]
fn terminate_command_process_tree(child: &mut std::process::Child) {
    if let Ok(process_group) = i32::try_from(child.id()) {
        // SAFETY: run_command_with_timeout starts the child as the leader of a
        // new process group, so the negative PID targets only that command and
        // descendants such as LibreOffice's soffice.bin worker.
        unsafe {
            libc::kill(-process_group, libc::SIGKILL);
        }
    }
    child.kill().ok();
    child.wait().ok();
}

#[cfg(not(unix))]
fn terminate_command_process_tree(child: &mut std::process::Child) {
    child.kill().ok();
    child.wait().ok();
}

pub(super) fn normalize_rtf(bytes: &[u8], base_url: &str) -> Result<String> {
    if bytes.starts_with(&[0xd0, 0xcf, 0x11, 0xe0]) {
        return normalize_legacy_word(bytes);
    }
    let start = bytes
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    if !bytes
        .get(start..)
        .is_some_and(|bytes| bytes.starts_with(b"{\\rtf"))
    {
        bail!("official RTF rendition has an unrecognised file signature");
    }
    match normalize_rtf_with_unrtf(bytes, base_url) {
        Ok(html) => Ok(html),
        Err(unrtf_error) => render_office_document_to_pdf(bytes, "rtf")
            .with_context(|| format!("unrtf failed before LibreOffice fallback: {unrtf_error:#}")),
    }
}

fn normalize_rtf_with_unrtf(bytes: &[u8], base_url: &str) -> Result<String> {
    let temp = tempfile::tempdir().context("creating RTF conversion workspace")?;
    let input = temp.path().join("input.rtf");
    let output_path = temp.path().join("output.html");
    fs::write(&input, bytes).context("writing RTF conversion input")?;
    let output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&output_path)
        .context("creating RTF conversion output")?;
    let mut command = ProcessCommand::new("unrtf");
    command
        .arg("--html")
        .arg(&input)
        .current_dir(temp.path())
        .stdout(Stdio::from(output))
        .stderr(Stdio::null());
    run_command_with_timeout(command, Duration::from_secs(5 * 60), "unrtf")?;
    let output = read_bounded_file(&output_path, MAX_DOCUMENT_BYTES)?;
    let html = decode_utf8(&output).unwrap_or_else(|_| decode_windows_1252(&output));
    let normalized = normalize_html(
        &html,
        base_url,
        HtmlRules {
            content_selector: "body",
            drop_ids: &[],
            drop_classes: &[],
            heading_classes: &[],
            preserve_same_document_fragments: false,
            repair_broken_links: false,
        },
    )?;
    ensure_nonempty_html(&normalized)?;
    Ok(normalized)
}

pub(super) fn normalize_legacy_word(bytes: &[u8]) -> Result<String> {
    let temp = tempfile::tempdir().context("creating legacy Word workspace")?;
    let input = temp.path().join("input.doc");
    let output = temp.path().join("output.txt");
    fs::write(&input, bytes).context("writing legacy Word input")?;
    let output_file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&output)?;
    let mut command = ProcessCommand::new("antiword");
    command
        .arg(&input)
        .stdout(Stdio::from(output_file))
        .stderr(Stdio::null());
    run_command_with_timeout(command, Duration::from_secs(5 * 60), "antiword")?;
    let text_bytes = read_bounded_file(&output, 64 * 1024 * 1024)?;
    let text = decode_utf8(&text_bytes).unwrap_or_else(|_| decode_windows_1252(&text_bytes));
    let has_text = text.split_whitespace().any(|token| {
        token != "[pic]" && token.chars().any(|character| character.is_alphanumeric())
    });
    if has_text {
        return normalize_text(&text);
    }

    render_office_document_to_pdf(bytes, "doc")
}

fn render_office_document_to_pdf(bytes: &[u8], extension: &str) -> Result<String> {
    normalize_pdf(&render_office_document_to_pdf_bytes(bytes, extension)?)
}

pub(super) fn render_office_document_to_pdf_bytes(
    bytes: &[u8],
    extension: &str,
) -> Result<Vec<u8>> {
    let temp = tempfile::tempdir().context("creating office rendering workspace")?;
    let input = temp.path().join(format!("input.{extension}"));
    fs::write(&input, bytes).context("writing office rendering input")?;
    let profile = temp.path().join("libreoffice-profile");
    let mut command = sandboxed_soffice_command(temp.path())?;
    command
        .arg("--headless")
        .arg(format!(
            "-env:UserInstallation=file://{}",
            profile.display()
        ))
        .arg("--convert-to")
        .arg("pdf")
        .arg("--outdir")
        .arg(temp.path())
        .arg(&input)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    run_command_with_timeout(
        command,
        Duration::from_secs(15 * 60),
        "LibreOffice office document rendering",
    )?;
    let rendered = read_bounded_file(&temp.path().join("input.pdf"), MAX_DOCUMENT_BYTES)
        .context("reading LibreOffice-rendered office PDF")?;
    if !rendered.starts_with(b"%PDF") {
        bail!("LibreOffice office document rendering produced an invalid PDF");
    }
    Ok(rendered)
}

pub(super) fn convert_office_document_to_docx(bytes: &[u8], extension: &str) -> Result<Vec<u8>> {
    let temp = tempfile::tempdir().context("creating structured Word conversion workspace")?;
    let input = temp.path().join(format!("input.{extension}"));
    fs::write(&input, bytes).context("writing structured Word conversion input")?;
    let profile = temp.path().join("libreoffice-profile");
    let mut command = sandboxed_soffice_command(temp.path())?;
    command
        .arg("--headless")
        .arg(format!(
            "-env:UserInstallation=file://{}",
            profile.display()
        ))
        .arg("--convert-to")
        .arg("docx")
        .arg("--outdir")
        .arg(temp.path())
        .arg(&input)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    run_command_with_timeout(
        command,
        Duration::from_secs(15 * 60),
        "LibreOffice structured Word conversion",
    )?;
    let converted = read_bounded_file(&temp.path().join("input.docx"), MAX_DOCUMENT_BYTES)
        .context("reading LibreOffice-converted DOCX")?;
    if !converted.starts_with(b"PK\x03\x04") {
        bail!("LibreOffice structured Word conversion produced an invalid DOCX")
    }
    Ok(converted)
}

#[cfg(test)]
pub(super) fn metadata_document(title: &str) -> String {
    let mut html = String::from("<article><h1>");
    escape_text(title, &mut html);
    html.push_str("</h1></article>");
    html
}

fn ensure_document_html(_title: &str, html: String) -> Result<String> {
    ensure_nonempty_html(&html)?;
    Ok(html)
}

fn ensure_nonempty_html(html: &str) -> Result<()> {
    if html.len() > 64 * 1024 * 1024 {
        bail!("normalized HTML exceeds its byte limit");
    }
    let parsed = Html::parse_fragment(html);
    let visible_alphanumeric = parsed
        .root_element()
        .text()
        .flat_map(str::chars)
        .filter(|character| character.is_alphanumeric())
        .count();
    if visible_alphanumeric == 0 {
        bail!("normalized HTML has no alphanumeric source text");
    }
    Ok(())
}

fn escape_text(value: &str, output: &mut String) {
    for character in value.chars() {
        let Some(character) = canonical_source_character(character) else {
            continue;
        };
        match character {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            _ => output.push(character),
        }
    }
}

fn escape_attribute(value: &str, output: &mut String) {
    for character in value.chars() {
        let Some(character) = canonical_source_character(character) else {
            continue;
        };
        match character {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '"' => output.push_str("&quot;"),
            '\'' => output.push_str("&#39;"),
            _ => output.push(character),
        }
    }
}

pub(super) fn make_acquired_html(html: String, canonical_url: String) -> AcquiredDocument {
    AcquiredDocument {
        html,
        assets: Vec::new(),
        date: None,
        canonical_url,
    }
}

pub(super) fn make_acquired_text(html: String, canonical_url: String) -> AcquiredDocument {
    AcquiredDocument {
        html,
        assets: Vec::new(),
        date: None,
        canonical_url,
    }
}

pub(super) fn parse_date(value: &str, formats: &[&str]) -> Option<String> {
    for format in formats {
        if let Ok(date) = chrono::NaiveDate::parse_from_str(value.trim(), format) {
            return Some(date.format("%Y-%m-%d").to_string());
        }
    }
    None
}

pub(super) fn sha256_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn source_root(adapter: &dyn OfficialAdapter, workspace: &Path) -> Result<PathBuf> {
    match fs::symlink_metadata(workspace) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            bail!(
                "{} workspace root must be a real directory",
                adapter.source_id()
            )
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => fs::create_dir_all(workspace)
            .with_context(|| format!("creating {} workspace", adapter.source_id()))?,
        Err(error) => return Err(error).context("reading official-source workspace metadata"),
    }
    Ok(workspace.to_path_buf())
}

fn confined_path(root: &Path, relative: &Path) -> Result<PathBuf> {
    if relative.as_os_str().is_empty() || relative.is_absolute() {
        bail!("official source path must be nonempty and relative");
    }
    for component in relative.components() {
        if !matches!(component, Component::Normal(_)) {
            bail!(
                "official source path escapes its root: {}",
                relative.display()
            );
        }
    }
    let path = root.join(relative);
    if !path.starts_with(root) {
        bail!("official source path escapes its root");
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        let canonical_root = fs::canonicalize(root)?;
        let canonical_parent = fs::canonicalize(parent)?;
        if !canonical_parent.starts_with(canonical_root) {
            bail!("official source path parent escapes its root");
        }
    }
    Ok(path)
}

fn write_immutable(root: &Path, relative: &Path, bytes: &[u8]) -> Result<()> {
    let path = confined_path(root, relative)?;
    match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(mut file) => {
            file.write_all(bytes)?;
            file.sync_all()?;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let existing = read_bounded_file(&path, bytes.len() as u64 + 1)?;
            if existing == bytes {
                Ok(())
            } else {
                bail!(
                    "immutable official-source artifact collision at {}",
                    path.display()
                )
            }
        }
        Err(error) => Err(error).with_context(|| format!("creating {}", path.display())),
    }
}

fn read_bounded_file(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("reading metadata for {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > limit {
        bail!(
            "official-source file is invalid or oversized: {}",
            path.display()
        );
    }
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(limit + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        bail!(
            "official-source file exceeds its byte limit: {}",
            path.display()
        );
    }
    Ok(bytes)
}

fn path_to_slashes(path: &Path) -> Result<String> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => parts.push(
                value
                    .to_str()
                    .ok_or_else(|| anyhow!("official-source path is not UTF-8"))?,
            ),
            _ => bail!("official-source path is not confined"),
        }
    }
    Ok(parts.join("/"))
}

fn validate_official_url(adapter: &dyn OfficialAdapter, value: &str) -> Result<()> {
    let url = Url::parse(value).with_context(|| format!("parsing official URL {value}"))?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || !adapter
            .approved_hosts()
            .iter()
            .any(|host| url.host_str() == Some(*host))
    {
        bail!(
            "{} URL is not an approved official HTTPS URL",
            adapter.source_id()
        );
    }
    Ok(())
}

fn validate_text(label: &str, value: &str, maximum: usize) -> Result<()> {
    if value.is_empty()
        || value != value.trim()
        || value.len() > maximum
        || value.chars().any(char::is_control)
    {
        bail!("invalid {label}");
    }
    Ok(())
}

fn validate_date(value: Option<&str>) -> Result<()> {
    if let Some(value) = value {
        chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d")
            .with_context(|| format!("invalid source date {value}"))?;
    }
    Ok(())
}

fn validate_sha256(value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("invalid SHA-256 digest");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[cfg(unix)]
    #[test]
    fn command_timeout_terminates_descendant_processes() -> Result<()> {
        let temp = tempdir()?;
        let sentinel = temp.path().join("descendant-survived");
        let mut command = ProcessCommand::new("sh");
        command
            .arg("-c")
            .arg("(sleep 1; printf leaked > \"$SENTINEL\") & wait")
            .env("SENTINEL", &sentinel)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let error =
            run_command_with_timeout(command, Duration::from_millis(50), "process-tree fixture")
                .expect_err("the process-tree fixture must time out");
        assert!(error.to_string().contains("exceeded its timeout"));
        thread::sleep(Duration::from_millis(1_200));
        assert!(!sentinel.exists());
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn libreoffice_sandbox_clears_environment_and_does_not_mount_host_root() -> Result<()> {
        let temp = tempdir()?;
        let command = sandboxed_soffice_command(temp.path())?;
        let arguments = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(arguments.iter().any(|argument| argument == "--clearenv"));
        assert!(arguments.windows(3).any(|window| {
            window == ["--tmpfs".to_string(), "/".to_string(), "--dir".to_string()]
        }));
        assert!(!arguments.windows(3).any(|window| {
            window == ["--ro-bind".to_string(), "/".to_string(), "/".to_string()]
        }));
        assert!(!arguments.iter().any(|argument| argument == "/run/user"));
        Ok(())
    }

    struct TestAdapter;

    impl OfficialAdapter for TestAdapter {
        fn source_id(&self) -> &'static str {
            "test"
        }

        fn display_name(&self) -> &'static str {
            "Test"
        }

        fn approved_hosts(&self) -> &'static [&'static str] {
            &["example.gov.au"]
        }

        fn rate_policy(&self) -> SourceRatePolicy {
            SourceRatePolicy {
                minimum_request_interval_ms: 0,
                request_timeout_seconds: 1,
            }
        }

        fn discover(
            &self,
            _: &OfficialHttpClient,
            _: SourceUpdateMode,
        ) -> Result<Vec<DiscoveredDocument>> {
            unreachable!()
        }

        fn acquire(
            &self,
            _: &OfficialHttpClient,
            entry: &DiscoveredDocument,
        ) -> Result<Option<AcquiredDocument>> {
            if entry.native_id.starts_with("absent") {
                return Ok(None);
            }
            Ok(Some(make_acquired_html(
                metadata_document(&entry.title),
                entry.canonical_url.clone(),
            )))
        }
    }

    struct QualityAdapter;

    impl OfficialAdapter for QualityAdapter {
        fn source_id(&self) -> &'static str {
            "test"
        }

        fn display_name(&self) -> &'static str {
            "Quality test"
        }

        fn approved_hosts(&self) -> &'static [&'static str] {
            &["example.gov.au"]
        }

        fn rate_policy(&self) -> SourceRatePolicy {
            SourceRatePolicy {
                minimum_request_interval_ms: 0,
                request_timeout_seconds: 1,
            }
        }

        fn validate_normalized_html(&self, html: &str) -> Result<()> {
            if html.contains("Document quality") {
                bail!("stale normalized quality fixture")
            }
            Ok(())
        }

        fn discover(
            &self,
            _: &OfficialHttpClient,
            _: SourceUpdateMode,
        ) -> Result<Vec<DiscoveredDocument>> {
            unreachable!()
        }

        fn acquire(
            &self,
            _: &OfficialHttpClient,
            entry: &DiscoveredDocument,
        ) -> Result<Option<AcquiredDocument>> {
            Ok(Some(make_acquired_html(
                "<article><p>Repaired quality fixture</p></article>".to_owned(),
                entry.canonical_url.clone(),
            )))
        }
    }

    fn entry(native_id: &str, version: &str) -> DiscoveredDocument {
        let url = format!("https://example.gov.au/{native_id}");
        DiscoveredDocument {
            native_id: native_id.to_owned(),
            upstream_version: version.to_owned(),
            title: format!("Document {native_id}"),
            document_type: "decision".to_owned(),
            date: Some("2026-01-01".to_owned()),
            citation: Some(format!("Document {native_id}")),
            canonical_url: url.clone(),
            renditions: vec![Rendition {
                url,
                kind: RenditionKind::Html,
            }],
        }
    }

    #[test]
    fn html_normalization_is_structural_and_drops_hidden_source_chrome() -> Result<()> {
        let html = r#"<html><body><div id="content"><h1>A &amp; B</h1><script>bad()</script><p>Law <a href="/x">text</a> and <a href="http://example.gov.au/legacy">legacy</a></p><div class="hidden">omit</div></div></body></html>"#;
        let normalized = normalize_html(
            html,
            "https://example.gov.au/doc",
            HtmlRules {
                content_selector: "#content",
                drop_ids: &[],
                drop_classes: &["hidden"],
                heading_classes: &[],
                preserve_same_document_fragments: false,
                repair_broken_links: true,
            },
        )?;
        assert_eq!(
            normalized,
            "<article><h1>A &amp; B</h1><p>Law <a href=\"https://example.gov.au/x\">text</a> and <a href=\"https://example.gov.au/legacy\">legacy</a></p></article>"
        );
        Ok(())
    }

    #[test]
    fn malformed_binary_rendition_never_reaches_the_rtf_parser() {
        let result = std::panic::catch_unwind(|| {
            normalize_rtf(
                b"\x7f\xfe\x80PK\x03\x04not an RTF document",
                "https://example.gov.au",
            )
        });
        assert!(result.is_ok());
        assert!(result.expect("normalizer must not panic").is_err());
    }

    #[test]
    fn sparse_pdf_text_layer_requires_ocr() {
        assert!(normalize_extracted_pdf_text("- 3) 'l'IIE").is_err());
        assert!(normalize_extracted_pdf_text(&"judgment ".repeat(16)).is_ok());
    }

    #[test]
    fn normalized_document_requires_alphanumeric_source_text() {
        assert!(ensure_nonempty_html("<article>\u{fffd}</article>").is_err());
        assert!(ensure_nonempty_html("<article>1</article>").is_ok());
        let mut escaped = String::new();
        escape_text("A\u{98}\u{81}B", &mut escaped);
        assert_eq!(escaped, "A˜B");
    }

    #[test]
    fn response_body_retries_only_transient_read_failures() {
        let timeout = anyhow!(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "timed out",
        ));
        assert!(retryable_response_read_error(&timeout));
        assert!(!retryable_response_read_error(&anyhow!(
            "official response exceeds its byte limit"
        )));
        assert!(is_official_antibot_challenge(
            b"Enable JavaScript and cookies to continue"
        ));
        assert!(!is_official_antibot_challenge(b"official legal text"));
    }

    #[test]
    fn scoped_workers_complete_when_called_from_a_rayon_worker() -> Result<()> {
        let pool = rayon::ThreadPoolBuilder::new().num_threads(2).build()?;
        let values =
            pool.install(|| scoped_thread_map(2, vec![1usize, 2, 3, 4], |value| Ok(value * 2)))?;
        assert_eq!(values, vec![2, 4, 6, 8]);
        Ok(())
    }

    #[test]
    fn registered_official_links_become_source_qualified_document_references() -> Result<()> {
        let target = DocumentId::new("test".parse()?, "target")?;
        let links = BTreeMap::from([("https://example.gov.au/target".to_owned(), target.clone())]);
        let html = rewrite_internal_document_links(
            r#"<article><p>See <a href="/target#section-1">the target</a>.</p></article>"#,
            "https://example.gov.au/source",
            &links,
        )?;
        assert!(html.contains(&format!(r#"data-doc-id="{target}""#)));
        assert!(!html.contains("href="));
        Ok(())
    }

    #[test]
    fn internal_link_rewrite_preserves_typed_images_but_never_source_urls() -> Result<()> {
        let html = rewrite_internal_document_links(
            r#"<article><p>Formula <img data-asset-ref="frl:C2004A05138/sha256-formula" data-media-type="image/png" alt="x &amp; y" title="Formula" src="https://untrusted.invalid/formula.png"></p></article>"#,
            "https://www.legislation.gov.au/C2004A05138/latest/text",
            &BTreeMap::new(),
        )?;
        assert!(html.contains(r#"data-asset-ref="frl:C2004A05138/sha256-formula""#));
        assert!(html.contains(r#"data-media-type="image/png""#));
        assert!(html.contains(r#"alt="x &amp; y""#));
        assert!(html.contains(r#"title="Formula""#));
        assert!(!html.contains("src="));
        assert!(!html.contains("</img>"));
        Ok(())
    }

    #[test]
    fn discovery_rejects_duplicate_native_identities() {
        let document = entry("one", "v1");
        let mut documents = vec![document.clone(), document];
        assert!(validate_discovery(&TestAdapter, &mut documents).is_err());
    }

    #[test]
    fn authoritative_snapshot_rejects_catastrophic_shrinkage() {
        assert!(validate_snapshot_size("test", 49, 100, 50).is_err());
        assert!(validate_snapshot_size("test", 50, 100, 50).is_ok());
    }

    #[test]
    fn committed_state_reuses_documents_and_removes_absent_documents() -> Result<()> {
        let workspace = tempdir()?;
        let first = fetch_documents(
            &TestAdapter,
            workspace.path(),
            vec![entry("one", "v1"), entry("two", "v1")],
        )?;
        assert_eq!(first.completed, 2);
        assert!(workspace.path().join("state.json").is_file());
        assert!(!workspace.path().join("test").exists());
        assert_eq!(
            load_state(&TestAdapter, workspace.path())?.inventory.len(),
            2
        );

        let resumed = fetch_documents(
            &TestAdapter,
            workspace.path(),
            vec![entry("one", "v1"), entry("two", "v1")],
        )?;
        assert_eq!(resumed.skipped, 2);

        let reconciled = fetch_documents(&TestAdapter, workspace.path(), vec![entry("two", "v2")])?;
        assert_eq!(reconciled.completed, 1);
        let state = load_state(&TestAdapter, workspace.path())?;
        assert_eq!(state.inventory.keys().collect::<Vec<_>>(), vec!["two"]);
        let source: SourceId = "test".parse()?;
        let documents = state
            .inventory
            .values()
            .map(|entry| load_inventory_document(&TestAdapter, workspace.path(), &source, entry))
            .collect::<Result<Vec<_>>>()?;
        assert_eq!(documents.len(), 1);
        assert_eq!(documents[0].inventory.document.native_id, "two");
        Ok(())
    }

    #[test]
    fn current_adapter_quality_gate_reacquires_a_stale_committed_document() -> Result<()> {
        let workspace = tempdir()?;
        let document = entry("quality", "v1");
        let first = fetch_documents(&TestAdapter, workspace.path(), vec![document.clone()])?;
        assert_eq!((first.completed, first.skipped), (1, 0));

        let repaired = fetch_documents(&QualityAdapter, workspace.path(), vec![document])?;
        assert_eq!((repaired.completed, repaired.skipped), (1, 0));
        let state = load_state(&QualityAdapter, workspace.path())?;
        let candidate = state
            .inventory
            .get("quality")
            .ok_or_else(|| anyhow!("repaired quality fixture is missing"))?;
        let source: SourceId = "test".parse()?;
        let stored =
            load_inventory_document(&QualityAdapter, workspace.path(), &source, candidate)?;
        assert!(stored.html.contains("Repaired quality fixture"));
        assert!(!stored.html.contains("Document quality"));
        Ok(())
    }

    #[test]
    fn adapter_normalization_revision_invalidates_only_opted_in_sources() -> Result<()> {
        let document = entry("revision", "v1");
        let unchanged = discovered_source_revision(&TestAdapter, &document)?;
        let federal = discovered_source_revision(&federal_court::ADAPTER, &document)?;
        let south_australian =
            discovered_source_revision(&south_australian_legislation::ADAPTER, &document)?;
        let high_court = discovered_source_revision(&high_court::ADAPTER, &document)?;
        let western_australian =
            discovered_source_revision(&western_australian_legislation::ADAPTER, &document)?;
        assert!(!unchanged.starts_with("normalizer:"));
        assert!(federal.starts_with("normalizer:2|"));
        assert_ne!(unchanged, federal);
        assert!(south_australian.starts_with("normalizer:3|"));
        assert_ne!(unchanged, south_australian);
        assert!(high_court.starts_with("normalizer:1|"));
        assert!(western_australian.starts_with("normalizer:1|"));
        assert_ne!(unchanged, high_court);
        assert_ne!(unchanged, western_australian);
        Ok(())
    }

    #[test]
    fn verified_staging_resumes_before_state_commit() -> Result<()> {
        let workspace = tempdir()?;
        let client = OfficialHttpClient::new(&TestAdapter)?;
        let source: SourceId = "test".parse()?;
        let discovered = entry("one", "v1");
        let first = acquire_one(
            &TestAdapter,
            &client,
            workspace.path(),
            &source,
            &discovered,
            None,
        )?;
        assert!(!first.2);
        let resumed = acquire_one(
            &TestAdapter,
            &client,
            workspace.path(),
            &source,
            &discovered,
            None,
        )?;
        assert!(resumed.2);
        clear_staging(&TestAdapter, workspace.path())?;
        assert!(load_staging_entry(&TestAdapter, workspace.path(), "one")?.is_none());
        Ok(())
    }

    #[test]
    fn authoritative_404_can_omit_a_small_number_of_stale_index_records() -> Result<()> {
        let workspace = tempdir()?;
        let mut documents = (0..100)
            .map(|index| entry(&format!("document-{index}"), "v1"))
            .collect::<Vec<_>>();
        documents.push(entry("absent", "v1"));
        let report = fetch_documents(&TestAdapter, workspace.path(), documents)?;
        assert_eq!(report.completed, 100);
        assert_eq!(report.skipped, 1);
        let state = load_state(&TestAdapter, workspace.path())?;
        assert_eq!(state.inventory.len(), 100);
        assert!(!state.inventory.contains_key("absent"));
        Ok(())
    }

    #[test]
    fn unavailable_renditions_cannot_hide_a_broad_source_failure() -> Result<()> {
        let workspace = tempdir()?;
        let mut documents = (0..99)
            .map(|index| entry(&format!("document-{index}"), "v1"))
            .collect::<Vec<_>>();
        documents.push(entry("absent-one", "v1"));
        documents.push(entry("absent-two", "v1"));
        assert!(fetch_documents(&TestAdapter, workspace.path(), documents).is_err());
        assert!(!source_root(&TestAdapter, workspace.path())?
            .join("state.json")
            .exists());
        Ok(())
    }
}
