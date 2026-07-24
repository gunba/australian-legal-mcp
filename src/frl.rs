//! Federal Register of Legislation acquisition and normalization.

use crate::adaptive_http::{AdaptiveConcurrency, RequestOutcome, SOURCE_WORKER_CEILING};
use crate::legal_source::{SourceDescriptor, SourceId};
use crate::source_update::{
    SourceAcquisition, SourceDiscoveryBatch, SourceFetchReport, SourceInventoryFingerprint,
    SourceRatePolicy, SourceUpdateContext, SourceUpdateMode,
};
use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, NaiveDateTime, Timelike, Utc};
use legal_model::{AssetRef, DocumentId};
use legal_source_sdk::{NormalizedAsset, NormalizedDocument, SourceInventoryRecord};
use rayon::prelude::*;
use reqwest::blocking::{Client, Response};
use reqwest::header::{ACCEPT, CONTENT_LENGTH, RETRY_AFTER};
use reqwest::{StatusCode, Url};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{Cursor, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};
use zip::ZipArchive;

pub(crate) const FRL_SOURCE_ID: &str = "frl";
const FRL_DISPLAY_NAME: &str = "Federal Register of Legislation";
const FRL_API_BASE: &str = "https://api.prod.legislation.gov.au/v1/";
const FRL_PUBLIC_BASE: &str = "https://www.legislation.gov.au/";
const PAGE_SIZE: usize = 100;
const MAX_TITLE_PAGES: usize = 4_096;
const MAX_VERSION_PAGES: usize = 16_384;
const MAX_DOCUMENT_PAGES: usize = 64;
const MAX_JSON_BODY_BYTES: u64 = 16 * 1024 * 1024;
const MAX_RENDITION_BYTES: u64 = 128 * 1024 * 1024;
const MAX_STATE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 20_000;
// Official DOCX judgments can contain very large coordinate schedules.
const MAX_ARCHIVE_MEMBER_BYTES: u64 = 128 * 1024 * 1024;
const MAX_ARCHIVE_EXPANDED_BYTES: u64 = 256 * 1024 * 1024;
const MAX_XML_DEPTH: usize = 256;
const MAX_XML_NODES: usize = 4_000_000;
const MAX_DOCX_STYLE_REFERENCE_DEPTH: usize = 128;
const MIN_FULL_TEXT_ALPHANUMERIC_CHARS: usize = 128;
const OVERLAP_DAYS: i64 = 7;
const MAX_HTTP_ATTEMPTS: usize = 4;
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);
const STATE_SCHEMA_VERSION: u32 = 2;
const DISCOVERY_SCHEMA_VERSION: u32 = 1;
const STATE_RELATIVE_PATH: &str = "state.json";
const STAGING_DIR: &str = "staging";
const DISCOVERY_FILE_NAME: &str = "frl-discovery.json";

pub(crate) fn frl_descriptor() -> Result<SourceDescriptor> {
    Ok(SourceDescriptor::new(
        SourceId::new(FRL_SOURCE_ID)?,
        FRL_DISPLAY_NAME,
    )?)
}

#[derive(Debug)]
pub(crate) struct FrlAcquisition;

pub(crate) static FRL_ACQUISITION: FrlAcquisition = FrlAcquisition;

impl SourceAcquisition for FrlAcquisition {
    fn rate_policy(&self) -> SourceRatePolicy {
        SourceRatePolicy {
            minimum_request_interval_ms: 0,
            request_timeout_seconds: 30,
        }
    }

    fn inventory(&self, context: &SourceUpdateContext) -> Result<SourceInventoryFingerprint> {
        let state = load_state(&context.workspace)?;
        fingerprint_inventory(&state.inventory)
    }

    fn discover(&self, context: &SourceUpdateContext) -> Result<SourceDiscoveryBatch> {
        let api = HttpFrlApi::new(self.rate_policy())?;
        discover_to_run_dir(
            &api,
            &context.workspace,
            &context.run_dir,
            context.mode,
            Utc::now().naive_utc() + ChronoDuration::hours(14),
            PAGE_SIZE,
        )
    }

    fn fetch(
        &self,
        context: &SourceUpdateContext,
        discovery: &SourceDiscoveryBatch,
    ) -> Result<SourceFetchReport> {
        let api = HttpFrlApi::new(self.rate_policy())?;
        fetch_discovery(
            &api,
            &context.workspace,
            &context.run_dir,
            discovery,
            context.mode,
            PAGE_SIZE,
        )
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct FrlTitle {
    id: String,
    name: Option<String>,
    making_date: Option<String>,
    #[serde(deserialize_with = "deserialize_collection")]
    collection: String,
    #[serde(default, deserialize_with = "deserialize_sub_collection")]
    sub_collection: Option<String>,
    is_principal: bool,
    is_in_force: bool,
    #[serde(deserialize_with = "deserialize_status")]
    status: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct FrlVersion {
    title_id: String,
    start: String,
    retrospective_start: String,
    end: Option<String>,
    retrospective_end: Option<String>,
    is_current: bool,
    is_latest: bool,
    name: Option<String>,
    #[serde(deserialize_with = "deserialize_status")]
    status: String,
    register_id: Option<String>,
    registered_at: Option<String>,
    compilation_number: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct FrlRendition {
    title_id: String,
    start: String,
    retrospective_start: String,
    rectification_version_number: i64,
    #[serde(rename = "type", deserialize_with = "deserialize_document_type")]
    document_type: String,
    unique_type_number: i64,
    volume_number: i64,
    #[serde(deserialize_with = "deserialize_document_format")]
    format: String,
    compilation_number: Option<String>,
    register_id: Option<String>,
    #[serde(default, deserialize_with = "deserialize_document_version_type")]
    version_type: Option<String>,
    extension: Option<String>,
    mime_type: Option<String>,
    file_name: Option<String>,
    bytes: Option<String>,
    page_count: Option<i64>,
    size_in_bytes: Option<i64>,
    is_authorised: bool,
    name: Option<String>,
    contents: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct ODataPage<T> {
    value: Vec<T>,
}

#[derive(Debug, Deserialize)]
struct TitleIdResponse {
    id: String,
}

#[derive(Debug, Deserialize)]
struct OfficialTextResponse {
    #[serde(rename = "Contents")]
    contents: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ApiEnumValue {
    Name(String),
    Ordinal(i64),
}

fn required_enum<'de, D>(
    deserializer: D,
    values: &[(i64, &'static str)],
    label: &str,
) -> std::result::Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let value = ApiEnumValue::deserialize(deserializer)?;
    enum_name(value, values, label).map_err(serde::de::Error::custom)
}

fn optional_enum<'de, D>(
    deserializer: D,
    values: &[(i64, &'static str)],
    label: &str,
) -> std::result::Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<ApiEnumValue>::deserialize(deserializer)?
        .map(|value| enum_name(value, values, label))
        .transpose()
        .map_err(serde::de::Error::custom)
}

fn enum_name(value: ApiEnumValue, values: &[(i64, &'static str)], label: &str) -> Result<String> {
    match value {
        ApiEnumValue::Name(name) if !name.trim().is_empty() => Ok(name),
        ApiEnumValue::Name(_) => bail!("FRL {label} is empty"),
        ApiEnumValue::Ordinal(ordinal) => values
            .iter()
            .find_map(|(value, name)| (*value == ordinal).then(|| (*name).to_owned()))
            .ok_or_else(|| anyhow!("unknown FRL {label} ordinal {ordinal}")),
    }
}

fn deserialize_collection<'de, D>(deserializer: D) -> std::result::Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    required_enum(
        deserializer,
        &[
            (0, "Act"),
            (1, "LegislativeInstrument"),
            (2, "NotifiableInstrument"),
            (3, "AdministrativeArrangementsOrder"),
            (4, "Constitution"),
            (5, "ContinuedLaw"),
            (6, "Gazette"),
            (7, "PrerogativeInstrument"),
        ],
        "collection",
    )
}

fn deserialize_sub_collection<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    optional_enum(
        deserializer,
        &[
            (0, "Regulations"),
            (1, "CourtRules"),
            (2, "Rules"),
            (3, "ByLaws"),
        ],
        "sub-collection",
    )
}

fn deserialize_status<'de, D>(deserializer: D) -> std::result::Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    required_enum(
        deserializer,
        &[
            (0, "InForce"),
            (1, "Ceased"),
            (2, "Repealed"),
            (3, "NeverEffective"),
        ],
        "status",
    )
}

fn deserialize_document_type<'de, D>(deserializer: D) -> std::result::Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    required_enum(
        deserializer,
        &[
            (0, "Primary"),
            (1, "ES"),
            (2, "SupportingMaterial"),
            (3, "IncorporatedByReference"),
            (5, "SupplementaryES"),
        ],
        "document type",
    )
}

fn deserialize_document_format<'de, D>(deserializer: D) -> std::result::Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    required_enum(
        deserializer,
        &[(1, "Word"), (2, "Pdf"), (3, "Epub"), (4, "NameOnly")],
        "document format",
    )
}

fn deserialize_document_version_type<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    optional_enum(
        deserializer,
        &[
            (0, "Rectification"),
            (1, "Replacement"),
            (2, "RetrospectiveCompilation"),
        ],
        "document version type",
    )
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FrlVersionKey {
    title_id: String,
    start: String,
    retrospective_start: String,
}

impl FrlVersionKey {
    fn from_version(version: &FrlVersion) -> Result<Self> {
        Ok(Self {
            title_id: validate_native_id(&version.title_id)?.to_owned(),
            start: canonical_datetime(&version.start)?,
            retrospective_start: canonical_datetime(&version.retrospective_start)?,
        })
    }

    fn matches_rendition(&self, rendition: &FrlRendition) -> Result<bool> {
        Ok(self.title_id == rendition.title_id
            && self.start == canonical_datetime(&rendition.start)?
            && self.retrospective_start == canonical_datetime(&rendition.retrospective_start)?)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FrlCursor {
    pub(crate) registered_at: String,
    pub(crate) title_id: String,
    pub(crate) start: String,
    pub(crate) retrospective_start: String,
}

impl FrlCursor {
    fn from_version(version: &FrlVersion) -> Result<Option<Self>> {
        let Some(registered_at) = version.registered_at.as_deref() else {
            return Ok(None);
        };
        Ok(Some(Self {
            registered_at: canonical_datetime(registered_at)?,
            title_id: validate_native_id(&version.title_id)?.to_owned(),
            start: canonical_datetime(&version.start)?,
            retrospective_start: canonical_datetime(&version.retrospective_start)?,
        }))
    }

    fn validate(&self) -> Result<()> {
        if canonical_datetime(&self.registered_at)? != self.registered_at
            || canonical_datetime(&self.start)? != self.start
            || canonical_datetime(&self.retrospective_start)? != self.retrospective_start
        {
            bail!("FRL cursor contains a non-canonical datetime");
        }
        validate_native_id(&self.title_id)?;
        Ok(())
    }

    fn order_key(&self) -> Result<(NaiveDateTime, &str, NaiveDateTime, NaiveDateTime)> {
        Ok((
            parse_datetime(&self.registered_at)?,
            self.title_id.as_str(),
            parse_datetime(&self.start)?,
            parse_datetime(&self.retrospective_start)?,
        ))
    }
}

fn compare_cursors(left: &FrlCursor, right: &FrlCursor) -> Result<Ordering> {
    Ok(left.order_key()?.cmp(&right.order_key()?))
}

#[derive(Clone, Debug)]
struct VersionPageQuery {
    lower_bound: Option<String>,
    upper_bound: String,
    after: Option<FrlCursor>,
    top: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum FrlPayload {
    Epub(Vec<u8>),
    Docx(Vec<u8>),
    OfficialPdfText(String),
    OfficialMetadata,
}

trait FrlApi: Send + Sync {
    fn title_upper_bound(&self) -> Result<Option<String>>;

    fn titles_page(
        &self,
        upper_bound: &str,
        after: Option<&str>,
        top: usize,
    ) -> Result<Vec<FrlTitle>>;

    fn versions_page(&self, query: &VersionPageQuery) -> Result<Vec<FrlVersion>>;

    fn authoritative_version(
        &self,
        title_id: &str,
        upper_bound: &str,
    ) -> Result<Option<FrlVersion>>;

    fn documents_page(
        &self,
        version: &FrlVersionKey,
        after: Option<&RenditionKey>,
        top: usize,
    ) -> Result<Vec<FrlRendition>>;

    fn fetch_rendition(&self, rendition: &FrlRendition) -> Result<FrlPayload>;
}

struct HttpFrlApi {
    client: Client,
    base: Url,
    concurrency: AdaptiveConcurrency,
    minimum_interval: Duration,
    last_request: Mutex<Option<Instant>>,
}

#[derive(Clone, Copy)]
enum FrlHttpSurface {
    Api,
    PublicRendition,
}

impl FrlHttpSurface {
    fn validate(self, url: &Url) -> Result<()> {
        match self {
            Self::Api => validate_api_url(url),
            Self::PublicRendition => validate_public_rendition_url(url),
        }
    }
}

impl HttpFrlApi {
    fn new(policy: SourceRatePolicy) -> Result<Self> {
        let timeout = Duration::from_secs(policy.request_timeout_seconds);
        let client = Client::builder()
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("australian-legal-mcp/FRL-source-adapter")
            .build()
            .context("building FRL HTTP client")?;
        let base = Url::parse(FRL_API_BASE).context("parsing FRL API base URL")?;
        Ok(Self {
            client,
            base,
            concurrency: AdaptiveConcurrency::new(FRL_SOURCE_ID),
            minimum_interval: Duration::from_millis(policy.minimum_request_interval_ms),
            last_request: Mutex::new(None),
        })
    }

    fn entity_url(&self, entity: &str) -> Result<Url> {
        let url = self
            .base
            .join(entity)
            .with_context(|| format!("constructing FRL {entity} URL"))?;
        validate_api_url(&url)?;
        Ok(url)
    }

    fn wait_for_issue_slot(&self) -> Result<Duration> {
        let started = Instant::now();
        let mut last = self
            .last_request
            .lock()
            .map_err(|_| anyhow!("FRL request pacing lock is poisoned"))?;
        if let Some(previous) = *last {
            let elapsed = previous.elapsed();
            if elapsed < self.minimum_interval {
                thread::sleep(self.minimum_interval - elapsed);
            }
        }
        *last = Some(Instant::now());
        Ok(started.elapsed())
    }

    fn get_bounded(&self, url: Url, accept: &str, limit: u64) -> Result<Vec<u8>> {
        self.get_bounded_from(url, accept, limit, FrlHttpSurface::Api)
    }

    fn get_public_bounded(&self, url: Url, accept: &str, limit: u64) -> Result<Vec<u8>> {
        self.get_bounded_from(url, accept, limit, FrlHttpSurface::PublicRendition)
    }

    fn get_bounded_from(
        &self,
        url: Url,
        accept: &str,
        limit: u64,
        surface: FrlHttpSurface,
    ) -> Result<Vec<u8>> {
        surface.validate(&url)?;
        let mut last_error = None;
        for attempt in 0..MAX_HTTP_ATTEMPTS {
            let request = self.concurrency.acquire()?;
            let pacing_wait = self.wait_for_issue_slot()?;
            let response = self.client.get(url.clone()).header(ACCEPT, accept).send();
            match response {
                Ok(response) if response.status().is_success() => {
                    let status = response.status();
                    let result = read_bounded_response(response, limit)
                        .with_context(|| format!("reading FRL response from {url}"));
                    request.finish(
                        url.as_str(),
                        Some(status.as_u16()),
                        result.as_ref().map_or(0, Vec::len),
                        attempt + 1,
                        if result.is_ok() {
                            RequestOutcome::Success
                        } else {
                            RequestOutcome::Transient
                        },
                        pacing_wait,
                        None,
                    );
                    return result;
                }
                Ok(response) => {
                    let status = response.status();
                    let retry_after = retry_after_delay(&response);
                    let retryable = retryable_status(status);
                    let detail = bounded_error_detail(response).unwrap_or_default();
                    let message = if detail.is_empty() {
                        format!("FRL request {url} returned HTTP {status}")
                    } else {
                        format!("FRL request {url} returned HTTP {status}: {detail}")
                    };
                    if retryable && attempt + 1 < MAX_HTTP_ATTEMPTS {
                        let delay = retry_after.unwrap_or_else(|| retry_delay(attempt));
                        if delay > MAX_RETRY_DELAY {
                            bail!(
                                "{message}; Retry-After exceeds the bounded {}-second retry window",
                                MAX_RETRY_DELAY.as_secs()
                            );
                        }
                        request.finish(
                            url.as_str(),
                            Some(status.as_u16()),
                            0,
                            attempt + 1,
                            if status == StatusCode::TOO_MANY_REQUESTS {
                                RequestOutcome::Congestion
                            } else {
                                RequestOutcome::Transient
                            },
                            pacing_wait,
                            Some(delay),
                        );
                        thread::sleep(delay);
                        last_error = Some(message);
                        continue;
                    }
                    request.finish(
                        url.as_str(),
                        Some(status.as_u16()),
                        0,
                        attempt + 1,
                        if retryable {
                            if status == StatusCode::TOO_MANY_REQUESTS {
                                RequestOutcome::Congestion
                            } else {
                                RequestOutcome::Transient
                            }
                        } else {
                            RequestOutcome::Neutral
                        },
                        pacing_wait,
                        None,
                    );
                    bail!(message);
                }
                Err(error) => {
                    let retryable = error.is_timeout() || error.is_connect() || error.is_request();
                    let message = format!("FRL request {url} failed: {error}");
                    if retryable && attempt + 1 < MAX_HTTP_ATTEMPTS {
                        let delay = retry_delay(attempt);
                        request.finish(
                            url.as_str(),
                            None,
                            0,
                            attempt + 1,
                            RequestOutcome::Transient,
                            pacing_wait,
                            Some(delay),
                        );
                        thread::sleep(delay);
                        last_error = Some(message);
                        continue;
                    }
                    request.finish(
                        url.as_str(),
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
                    bail!(message);
                }
            }
        }
        bail!(
            "FRL request {url} exhausted retries: {}",
            last_error.unwrap_or_else(|| "unknown transport failure".to_owned())
        )
    }

    fn get_json<T: DeserializeOwned>(&self, url: Url, limit: u64) -> Result<T> {
        let bytes = self.get_bounded(url, "application/json", limit)?;
        serde_json::from_slice(&bytes).context("decoding FRL JSON response")
    }

    fn rendition_url(&self, rendition: &FrlRendition) -> Result<Url> {
        validate_native_id(&rendition.title_id)?;
        let start = canonical_datetime(&rendition.start)?;
        let retrospective_start = canonical_datetime(&rendition.retrospective_start)?;
        validate_odata_atom(&rendition.document_type, "document type")?;
        validate_odata_atom(&rendition.format, "document format")?;
        let segment = format!(
            "documents(titleid='{}',start={},retrospectivestart={},rectificationversionnumber={},type='{}',uniqueTypeNumber={},volumeNumber={},format='{}')",
            odata_string(&rendition.title_id),
            start,
            retrospective_start,
            rendition.rectification_version_number,
            odata_string(&rendition.document_type),
            rendition.unique_type_number,
            rendition.volume_number,
            odata_string(&rendition.format),
        );
        self.entity_url(&segment)
    }

    fn public_rendition_url(&self, rendition: &FrlRendition, kind: RenditionKind) -> Result<Url> {
        let title_id = validate_native_id(&rendition.title_id)?;
        let start = public_rendition_datetime(&rendition.start)?;
        let retrospective_start = public_rendition_datetime(&rendition.retrospective_start)?;
        let format = match kind {
            RenditionKind::Epub => "epub",
            RenditionKind::Docx => "word",
            RenditionKind::Pdf => bail!("FRL public PDF does not expose official extracted text"),
        };
        let url = Url::parse(&format!(
            "{FRL_PUBLIC_BASE}{title_id}/{start}/{retrospective_start}/text/original/{format}"
        ))
        .context("constructing exact FRL public rendition URL")?;
        validate_public_rendition_url(&url)?;
        Ok(url)
    }

    fn fetch_binary_rendition(
        &self,
        rendition: &FrlRendition,
        kind: RenditionKind,
        accept: &str,
    ) -> Result<Vec<u8>> {
        let api_url = self.rendition_url(rendition)?;
        match self.get_bounded(api_url, accept, MAX_RENDITION_BYTES) {
            Ok(bytes) => Ok(bytes),
            Err(api_error) => {
                let public_url = self.public_rendition_url(rendition, kind)?;
                self.get_public_bounded(public_url, accept, MAX_RENDITION_BYTES)
                    .with_context(|| {
                        format!(
                            "fetching the exact public FRL rendition after its API entity failed: {api_error:#}"
                        )
                    })
            }
        }
    }
}

impl FrlApi for HttpFrlApi {
    fn title_upper_bound(&self) -> Result<Option<String>> {
        let mut url = self.entity_url("Titles")?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("$top", "1");
            query.append_pair("$orderby", "id desc");
            query.append_pair("$select", "id");
        }
        let page = self.get_json::<ODataPage<TitleIdResponse>>(url, MAX_JSON_BODY_BYTES)?;
        page.value
            .into_iter()
            .next()
            .map(|title| Ok(validate_native_id(&title.id)?.to_owned()))
            .transpose()
    }

    fn titles_page(
        &self,
        upper_bound: &str,
        after: Option<&str>,
        top: usize,
    ) -> Result<Vec<FrlTitle>> {
        validate_native_id(upper_bound)?;
        let mut filters = vec![format!("id le '{}'", odata_string(upper_bound))];
        if let Some(after) = after {
            validate_native_id(after)?;
            filters.push(format!("id gt '{}'", odata_string(after)));
        }
        let mut url = self.entity_url("Titles")?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("$top", &top.to_string());
            query.append_pair("$orderby", "id");
            query.append_pair("$filter", &filters.join(" and "));
            query.append_pair(
                "$select",
                "id,name,makingDate,collection,subCollection,isPrincipal,isInForce,status",
            );
        }
        Ok(self
            .get_json::<ODataPage<FrlTitle>>(url, MAX_JSON_BODY_BYTES)?
            .value)
    }

    fn versions_page(&self, query_spec: &VersionPageQuery) -> Result<Vec<FrlVersion>> {
        let mut filters = vec![
            "registeredAt ne null".to_owned(),
            format!("registeredAt le {}", query_spec.upper_bound),
        ];
        if let Some(boundary) = query_spec.lower_bound.as_deref() {
            filters.push(format!("registeredAt ge {boundary}"));
        }
        if let Some(after) = query_spec.after.as_ref() {
            filters.push(version_after_filter(after));
        }
        let mut url = self.entity_url("Versions")?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("$top", &query_spec.top.to_string());
            query.append_pair("$orderby", "registeredAt,titleId,start,retrospectiveStart");
            query.append_pair("$filter", &filters.join(" and "));
            query.append_pair(
                "$select",
                "titleId,start,retrospectiveStart,end,retrospectiveEnd,isCurrent,isLatest,name,status,registerId,registeredAt,compilationNumber",
            );
        }
        Ok(self
            .get_json::<ODataPage<FrlVersion>>(url, MAX_JSON_BODY_BYTES)?
            .value)
    }

    fn authoritative_version(
        &self,
        title_id: &str,
        upper_bound: &str,
    ) -> Result<Option<FrlVersion>> {
        validate_native_id(title_id)?;
        canonical_datetime(upper_bound)?;
        let mut url = self.entity_url("Versions")?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("$top", &PAGE_SIZE.to_string());
            query.append_pair(
                "$orderby",
                "isCurrent desc,isLatest desc,start desc,retrospectiveStart desc,registeredAt desc",
            );
            query.append_pair(
                "$filter",
                &format!(
                    "titleId eq '{}' and registeredAt ne null and registeredAt le {}",
                    odata_string(title_id),
                    upper_bound
                ),
            );
            query.append_pair(
                "$select",
                "titleId,start,retrospectiveStart,end,retrospectiveEnd,isCurrent,isLatest,name,status,registerId,registeredAt,compilationNumber",
            );
        }
        let page = self.get_json::<ODataPage<FrlVersion>>(url, MAX_JSON_BODY_BYTES)?;
        let selected = select_versions_by_title(page.value)?;
        Ok(selected.into_iter().next())
    }

    fn documents_page(
        &self,
        version: &FrlVersionKey,
        after: Option<&RenditionKey>,
        top: usize,
    ) -> Result<Vec<FrlRendition>> {
        let mut filters = vec![format!(
            "titleId eq '{}' and start eq {} and retrospectiveStart eq {}",
            odata_string(&version.title_id),
            version.start,
            version.retrospective_start
        )];
        if let Some(after) = after {
            filters.push(rendition_after_filter(after)?);
        }
        let mut url = self.entity_url("Documents")?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("$top", &top.to_string());
            query.append_pair("$filter", &filters.join(" and "));
            query.append_pair(
                "$orderby",
                "rectificationVersionNumber,type,uniqueTypeNumber,volumeNumber,format",
            );
            query.append_pair(
                "$select",
                "titleId,start,retrospectiveStart,rectificationVersionNumber,type,uniqueTypeNumber,volumeNumber,format,compilationNumber,registerId,versionType,extension,mimeType,fileName,pageCount,sizeInBytes,isAuthorised,name,contents",
            );
        }
        Ok(self
            .get_json::<ODataPage<FrlRendition>>(url, MAX_JSON_BODY_BYTES)?
            .value)
    }

    fn fetch_rendition(&self, rendition: &FrlRendition) -> Result<FrlPayload> {
        let kind = rendition_kind(rendition).ok_or_else(|| {
            anyhow!(
                "FRL rendition {} {} has no supported official format",
                rendition.format,
                rendition.extension.as_deref().unwrap_or("")
            )
        })?;
        match kind {
            RenditionKind::Epub => Ok(FrlPayload::Epub(self.fetch_binary_rendition(
                rendition,
                kind,
                "application/epub+zip",
            )?)),
            RenditionKind::Docx => Ok(FrlPayload::Docx(self.fetch_binary_rendition(
                rendition,
                kind,
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            )?)),
            RenditionKind::Pdf => {
                let mut url = self.rendition_url(rendition)?;
                url.query_pairs_mut().append_pair("$select", "contents");
                let entity: OfficialTextResponse = self.get_json(url, MAX_RENDITION_BYTES)?;
                Ok(
                    match entity.contents.filter(|value| !value.trim().is_empty()) {
                        Some(text) => FrlPayload::OfficialPdfText(text),
                        None => FrlPayload::OfficialMetadata,
                    },
                )
            }
        }
    }
}

fn validate_api_url(url: &Url) -> Result<()> {
    if url.scheme() != "https"
        || url.host_str() != Some("api.prod.legislation.gov.au")
        || !url.path().starts_with("/v1/")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        bail!("refusing non-FRL API URL {url}");
    }
    Ok(())
}

fn validate_public_rendition_url(url: &Url) -> Result<()> {
    if url.scheme() != "https"
        || url.host_str() != Some("www.legislation.gov.au")
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("refusing non-FRL public rendition URL {url}");
    }
    let segments = url
        .path_segments()
        .ok_or_else(|| anyhow!("FRL public rendition URL has no path"))?
        .collect::<Vec<_>>();
    if segments.len() != 6
        || segments[3] != "text"
        || segments[4] != "original"
        || !matches!(segments[5], "epub" | "word")
    {
        bail!("refusing malformed FRL public rendition URL {url}");
    }
    validate_native_id(segments[0])?;
    for datetime in &segments[1..=2] {
        if public_rendition_datetime(datetime)? != *datetime {
            bail!("refusing non-canonical FRL public rendition URL {url}");
        }
    }
    Ok(())
}

fn read_bounded_response(mut response: Response, limit: u64) -> Result<Vec<u8>> {
    if response
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|length| length > limit)
    {
        bail!("FRL response exceeds the {limit}-byte body limit");
    }
    let mut bytes = Vec::new();
    response
        .by_ref()
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .context("reading bounded FRL response")?;
    if bytes.len() as u64 > limit {
        bail!("FRL response exceeds the {limit}-byte body limit");
    }
    Ok(bytes)
}

fn bounded_error_detail(response: Response) -> Result<String> {
    let bytes = read_bounded_response(response, 64 * 1024)?;
    let detail = String::from_utf8_lossy(&bytes)
        .chars()
        .filter(|character| !character.is_control() || character.is_ascii_whitespace())
        .take(1_024)
        .collect::<String>();
    Ok(detail.trim().to_owned())
}

fn retryable_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::TOO_MANY_REQUESTS
            | StatusCode::BAD_GATEWAY
            | StatusCode::SERVICE_UNAVAILABLE
            | StatusCode::GATEWAY_TIMEOUT
    )
}

fn retry_after_delay(response: &Response) -> Option<Duration> {
    let value = response.headers().get(RETRY_AFTER)?.to_str().ok()?.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let when = DateTime::parse_from_rfc2822(value)
        .ok()?
        .with_timezone(&Utc);
    (when - Utc::now()).to_std().ok()
}

fn retry_delay(attempt: usize) -> Duration {
    let multiplier = 1_u64 << attempt.min(6);
    let jitter_ms = ((attempt as u64 * 97) + 53) % 173;
    Duration::from_millis((250 * multiplier + jitter_ms).min(30_000))
}

fn odata_string(value: &str) -> String {
    value.replace('\'', "''")
}

fn version_after_filter(cursor: &FrlCursor) -> String {
    let registered_at = &cursor.registered_at;
    let title_id = odata_string(&cursor.title_id);
    let start = &cursor.start;
    let retrospective_start = &cursor.retrospective_start;
    format!(
        "(registeredAt gt {registered_at} or \
         (registeredAt eq {registered_at} and titleId gt '{title_id}') or \
         (registeredAt eq {registered_at} and titleId eq '{title_id}' and start gt {start}) or \
         (registeredAt eq {registered_at} and titleId eq '{title_id}' and start eq {start} and retrospectiveStart gt {retrospective_start}))"
    )
}

fn rendition_after_filter(key: &RenditionKey) -> Result<String> {
    validate_odata_atom(&key.document_type, "document type")?;
    validate_odata_atom(&key.format, "document format")?;
    let document_type = format!("Default.DocumentType'{}'", odata_string(&key.document_type));
    let format = format!("Default.DocumentFormatType'{}'", odata_string(&key.format));
    let rectification = key.rectification_version_number;
    let unique = key.unique_type_number;
    let volume = key.volume_number;
    Ok(format!(
        "(rectificationVersionNumber gt {rectification} or \
         (rectificationVersionNumber eq {rectification} and type gt {document_type}) or \
         (rectificationVersionNumber eq {rectification} and type eq {document_type} and uniqueTypeNumber gt {unique}) or \
         (rectificationVersionNumber eq {rectification} and type eq {document_type} and uniqueTypeNumber eq {unique} and volumeNumber gt {volume}) or \
         (rectificationVersionNumber eq {rectification} and type eq {document_type} and uniqueTypeNumber eq {unique} and volumeNumber eq {volume} and format gt {format}))"
    ))
}

fn validate_odata_atom<'a>(value: &'a str, label: &str) -> Result<&'a str> {
    if value.is_empty()
        || value.len() > 128
        || value
            .chars()
            .any(|character| character.is_control() || matches!(character, '/' | '\\'))
    {
        bail!("invalid FRL {label}");
    }
    Ok(value)
}

fn validate_native_id(value: &str) -> Result<&str> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        bail!("invalid FRL native id `{value}`");
    }
    Ok(value)
}

fn parse_datetime(value: &str) -> Result<NaiveDateTime> {
    const FORMATS: [&str; 2] = ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S"];
    FORMATS
        .iter()
        .find_map(|format| NaiveDateTime::parse_from_str(value, format).ok())
        .ok_or_else(|| anyhow!("invalid timezone-free FRL datetime `{value}`"))
}

fn canonical_datetime(value: &str) -> Result<String> {
    let parsed = parse_datetime(value)?;
    if parsed.nanosecond() % 100 != 0 {
        bail!("FRL datetime `{value}` exceeds the official 100-nanosecond precision");
    }
    Ok(format!(
        "{}.{:07}",
        parsed.format("%Y-%m-%dT%H:%M:%S"),
        parsed.nanosecond() / 100
    ))
}

fn public_rendition_datetime(value: &str) -> Result<String> {
    let parsed = parse_datetime(value)?;
    if parsed.nanosecond() % 1_000_000 != 0 {
        bail!("FRL public rendition datetime `{value}` exceeds millisecond precision");
    }
    Ok(format!(
        "{}.{:03}",
        parsed.format("%Y-%m-%dT%H:%M:%S"),
        parsed.nanosecond() / 1_000_000
    ))
}

fn format_datetime(value: NaiveDateTime) -> String {
    format!(
        "{}.{:07}",
        value.format("%Y-%m-%dT%H:%M:%S"),
        value.nanosecond() / 100
    )
}

fn overlap_boundary(cursor: Option<&FrlCursor>) -> Result<Option<String>> {
    cursor
        .map(|cursor| {
            let registered_at = parse_datetime(&cursor.registered_at)?;
            let boundary = registered_at
                .checked_sub_signed(ChronoDuration::days(OVERLAP_DAYS))
                .ok_or_else(|| anyhow!("FRL cursor cannot represent its overlap boundary"))?;
            Ok(format_datetime(boundary))
        })
        .transpose()
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct FrlState {
    schema_version: u32,
    cursor: Option<FrlCursor>,
    inventory: BTreeMap<String, FrlInventoryEntry>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct FrlInventoryEntry {
    native_id: String,
    upstream_version: FrlVersionKey,
    register_id: Option<String>,
    canonical_url: String,
    payload_hash: String,
    last_successful_cursor: FrlCursor,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct FrlDiscoveryPlan {
    authoritative_titles: Vec<FrlTitle>,
    versions: Vec<FrlVersion>,
    proposed_cursor: Option<FrlCursor>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct FrlDiscoveryEnvelope {
    schema_version: u32,
    source: String,
    mode: SourceUpdateMode,
    plan: FrlDiscoveryPlan,
}

struct VersionScan {
    versions: Vec<FrlVersion>,
    proposed_cursor: Option<FrlCursor>,
    upper_bound: String,
}

fn discover_to_run_dir(
    api: &dyn FrlApi,
    workspace: &Path,
    run_dir: &Path,
    mode: SourceUpdateMode,
    scan_started_at: NaiveDateTime,
    page_size: usize,
) -> Result<SourceDiscoveryBatch> {
    ensure_real_directory(workspace, "FRL workspace")?;
    ensure_real_directory(run_dir, "FRL run directory")?;
    let path = confined_path(run_dir, Path::new(DISCOVERY_FILE_NAME))?;
    if path.exists() {
        let bytes = read_bounded_file(&path, MAX_STATE_BYTES)?;
        let envelope: FrlDiscoveryEnvelope =
            serde_json::from_slice(&bytes).context("decoding saved FRL discovery plan")?;
        validate_discovery_envelope(&envelope, mode)?;
        return Ok(SourceDiscoveryBatch {
            path,
            records: envelope.plan.versions.len(),
        });
    }
    let state = load_state(workspace)?;
    let state_is_current = state.schema_version == STATE_SCHEMA_VERSION;
    let titles = scan_titles(api, page_size)?;
    let VersionScan {
        versions,
        proposed_cursor,
        upper_bound,
    } = scan_versions(
        api,
        if state_is_current {
            state.cursor.as_ref()
        } else {
            None
        },
        scan_started_at,
        page_size,
    )?;
    let title_ids = titles
        .iter()
        .map(|title| title.id.as_str())
        .collect::<BTreeSet<_>>();
    let mut selected_versions = resolve_authoritative_versions(
        api,
        versions
            .into_iter()
            .filter(|version| title_ids.contains(version.title_id.as_str())),
        !state_is_current || state.cursor.is_none(),
        &upper_bound,
    )?;
    let mut selected_title_ids = selected_versions
        .iter()
        .map(|version| version.title_id.clone())
        .collect::<BTreeSet<_>>();
    for title in &titles {
        if (state_is_current && state.inventory.contains_key(&title.id))
            || selected_title_ids.contains(&title.id)
        {
            continue;
        }
        let authoritative = api
            .authoritative_version(&title.id, &upper_bound)?
            .ok_or_else(|| {
                anyhow!(
                    "FRL authoritative title {} has no version at the scan boundary",
                    title.id
                )
            })?;
        selected_versions.push(canonicalize_version(&authoritative)?);
        selected_title_ids.insert(title.id.clone());
    }
    selected_versions.sort_by(|left, right| left.title_id.cmp(&right.title_id));
    let plan = FrlDiscoveryPlan {
        authoritative_titles: titles,
        versions: selected_versions,
        proposed_cursor,
    };
    validate_discovery_plan(&plan)?;
    let envelope = FrlDiscoveryEnvelope {
        schema_version: DISCOVERY_SCHEMA_VERSION,
        source: FRL_SOURCE_ID.to_owned(),
        mode,
        plan,
    };
    let mut bytes = serde_json::to_vec(&envelope).context("serializing FRL discovery plan")?;
    bytes.push(b'\n');
    if bytes.len() as u64 > MAX_STATE_BYTES {
        bail!("FRL discovery plan exceeds the bounded plan size");
    }
    atomic_write_confined(run_dir, Path::new(DISCOVERY_FILE_NAME), &bytes)?;
    Ok(SourceDiscoveryBatch {
        path,
        records: envelope.plan.versions.len(),
    })
}

fn scan_titles(api: &dyn FrlApi, page_size: usize) -> Result<Vec<FrlTitle>> {
    if page_size == 0 || page_size > PAGE_SIZE {
        bail!("invalid FRL title page size {page_size}");
    }
    let Some(upper_bound) = api.title_upper_bound()? else {
        bail!("FRL authoritative title scan returned no upper boundary");
    };
    validate_native_id(&upper_bound)?;
    let mut titles = BTreeMap::<String, FrlTitle>::new();
    let mut previous_id: Option<String> = None;
    for page_number in 0..MAX_TITLE_PAGES {
        let page = api.titles_page(&upper_bound, previous_id.as_deref(), page_size)?;
        if page.len() > page_size {
            bail!("FRL Titles returned more than the requested {page_size} rows");
        }
        let previous_page_boundary = previous_id.clone();
        for title in &page {
            validate_title(title)?;
            if title.id.as_str() > upper_bound.as_str() {
                bail!("FRL Titles returned an id beyond the fixed scan boundary");
            }
            if previous_id
                .as_deref()
                .is_some_and(|previous| title.id.as_str() < previous)
            {
                bail!("FRL Titles page is not ordered by id");
            }
            previous_id = Some(title.id.clone());
            if title.is_in_force
                && title.status == "InForce"
                && publishable_collection(&title.collection)
            {
                titles.insert(title.id.clone(), title.clone());
            }
        }
        if page.len() < page_size {
            if previous_id.as_deref() != Some(upper_bound.as_str()) {
                bail!("FRL Titles scan ended before its fixed upper boundary");
            }
            if titles.is_empty() {
                bail!("FRL authoritative title scan returned no in-force titles");
            }
            return Ok(titles.into_values().collect());
        }
        if previous_id == previous_page_boundary {
            bail!("FRL Titles keyset paging made no progress on page {page_number}");
        }
    }
    bail!("FRL Titles exceeded the bounded page limit")
}

fn validate_title(title: &FrlTitle) -> Result<()> {
    validate_native_id(&title.id)?;
    if title.collection.trim().is_empty() || title.collection.len() > 256 {
        bail!("FRL title {} has an invalid collection", title.id);
    }
    if title
        .name
        .as_deref()
        .is_some_and(|name| name.len() > 32 * 1024)
    {
        bail!("FRL title {} has an oversized name", title.id);
    }
    if let Some(date) = title.making_date.as_deref() {
        parse_datetime(date)?;
    }
    Ok(())
}

fn publishable_collection(collection: &str) -> bool {
    matches!(
        collection,
        "Constitution"
            | "Act"
            | "LegislativeInstrument"
            | "NotifiableInstrument"
            | "AdministrativeArrangementsOrder"
            | "PrerogativeInstrument"
            | "ContinuedLaw"
    )
}

fn scan_versions(
    api: &dyn FrlApi,
    previous_cursor: Option<&FrlCursor>,
    scan_started_at: NaiveDateTime,
    page_size: usize,
) -> Result<VersionScan> {
    if page_size == 0 || page_size > PAGE_SIZE {
        bail!("invalid FRL version page size {page_size}");
    }
    if let Some(cursor) = previous_cursor {
        cursor.validate()?;
    }
    let lower_bound = overlap_boundary(previous_cursor)?;
    let lower_bound_time = lower_bound.as_deref().map(parse_datetime).transpose()?;
    let ceiling = format_datetime(scan_started_at);
    let ceiling_cursor_time = parse_datetime(&ceiling)?;
    let mut versions = BTreeMap::<FrlVersionKey, (FrlCursor, FrlVersion)>::new();
    let mut maximum_cursor = previous_cursor.cloned();
    let mut previous_page_cursor: Option<FrlCursor> = None;
    for page_number in 0..MAX_VERSION_PAGES {
        let cursor_before_page = previous_page_cursor.clone();
        let page = api.versions_page(&VersionPageQuery {
            lower_bound: lower_bound.clone(),
            upper_bound: ceiling.clone(),
            after: cursor_before_page.clone(),
            top: page_size,
        })?;
        if page.len() > page_size {
            bail!("FRL Versions returned more than the requested {page_size} rows");
        }
        let mut reached_ceiling = false;
        for version in &page {
            let Some(cursor) = FrlCursor::from_version(version)? else {
                continue;
            };
            if previous_page_cursor.as_ref().is_some_and(|previous| {
                compare_cursors(&cursor, previous).is_ok_and(|order| order == Ordering::Less)
            }) {
                bail!(
                    "FRL Versions page is not ordered by registeredAt,titleId,start,retrospectiveStart"
                );
            }
            previous_page_cursor = Some(cursor.clone());
            let registered_at = parse_datetime(&cursor.registered_at)?;
            if registered_at > ceiling_cursor_time {
                reached_ceiling = true;
                break;
            }
            if lower_bound_time.is_some_and(|boundary| registered_at < boundary) {
                continue;
            }
            if maximum_cursor
                .as_ref()
                .map(|maximum| compare_cursors(&cursor, maximum))
                .transpose()?
                .is_none_or(|order| order == Ordering::Greater)
            {
                maximum_cursor = Some(cursor.clone());
            }
            let key = FrlVersionKey::from_version(version)?;
            match versions.get(&key) {
                Some((existing, _)) if compare_cursors(existing, &cursor)? == Ordering::Greater => {
                }
                _ => {
                    versions.insert(key, (cursor, canonicalize_version(version)?));
                }
            }
        }
        let page_progress = cursor_before_page
            .as_ref()
            .zip(previous_page_cursor.as_ref())
            .map(|(before, after)| compare_cursors(after, before))
            .transpose()?;
        if !page.is_empty() && page_progress.is_some_and(|order| order != Ordering::Greater) {
            bail!("FRL Versions keyset paging made no progress on page {page_number}");
        }
        if reached_ceiling || page.len() < page_size {
            return Ok(VersionScan {
                versions: versions.into_values().map(|(_, version)| version).collect(),
                proposed_cursor: maximum_cursor,
                upper_bound: ceiling,
            });
        }
    }
    bail!("FRL Versions exceeded the bounded page limit")
}

fn canonicalize_version(version: &FrlVersion) -> Result<FrlVersion> {
    let mut canonical = version.clone();
    canonical.title_id = validate_native_id(&version.title_id)?.to_owned();
    canonical.start = canonical_datetime(&version.start)?;
    canonical.retrospective_start = canonical_datetime(&version.retrospective_start)?;
    canonical.end = version.end.as_deref().map(canonical_datetime).transpose()?;
    canonical.retrospective_end = version
        .retrospective_end
        .as_deref()
        .map(canonical_datetime)
        .transpose()?;
    canonical.registered_at = version
        .registered_at
        .as_deref()
        .map(canonical_datetime)
        .transpose()?;
    Ok(canonical)
}

fn select_versions_by_title(
    versions: impl IntoIterator<Item = FrlVersion>,
) -> Result<Vec<FrlVersion>> {
    let mut selected = BTreeMap::<String, FrlVersion>::new();
    for version in versions {
        let candidate_cursor = FrlCursor::from_version(&version)?
            .ok_or_else(|| anyhow!("FRL version {} has no registration time", version.title_id))?;
        let replace = match selected.get(&version.title_id) {
            None => true,
            Some(existing) => {
                let existing_cursor = FrlCursor::from_version(existing)?.ok_or_else(|| {
                    anyhow!("FRL version {} has no registration time", existing.title_id)
                })?;
                let candidate_rank = (version.is_current, version.is_latest);
                let existing_rank = (existing.is_current, existing.is_latest);
                candidate_rank > existing_rank
                    || (candidate_rank == existing_rank
                        && compare_cursors(&candidate_cursor, &existing_cursor)?
                            == Ordering::Greater)
            }
        };
        if replace {
            selected.insert(version.title_id.clone(), version);
        }
    }
    Ok(selected.into_values().collect())
}

fn resolve_authoritative_versions(
    api: &dyn FrlApi,
    versions: impl IntoIterator<Item = FrlVersion>,
    full_history: bool,
    upper_bound: &str,
) -> Result<Vec<FrlVersion>> {
    let mut grouped = BTreeMap::<String, Vec<FrlVersion>>::new();
    for version in versions {
        grouped
            .entry(version.title_id.clone())
            .or_default()
            .push(version);
    }
    let mut resolved = Vec::with_capacity(grouped.len());
    for (title_id, candidates) in grouped {
        let selected = select_versions_by_title(candidates)?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("FRL title {title_id} has no selectable version"))?;
        if full_history || selected.is_current {
            resolved.push(selected);
        } else {
            let authoritative = api
                .authoritative_version(&title_id, upper_bound)?
                .ok_or_else(|| {
                    anyhow!(
                        "FRL title {title_id} has no authoritative version at the scan boundary"
                    )
                })?;
            resolved.push(canonicalize_version(&authoritative)?);
        }
    }
    Ok(resolved)
}

fn scan_documents(
    api: &dyn FrlApi,
    version: &FrlVersionKey,
    page_size: usize,
) -> Result<Vec<FrlRendition>> {
    if page_size == 0 || page_size > PAGE_SIZE {
        bail!("invalid FRL Documents page size {page_size}");
    }
    let mut documents = BTreeMap::<RenditionKey, FrlRendition>::new();
    let mut previous_key: Option<RenditionKey> = None;
    for page_number in 0..MAX_DOCUMENT_PAGES {
        let key_before_page = previous_key.clone();
        let page = api.documents_page(version, key_before_page.as_ref(), page_size)?;
        if page.len() > page_size {
            bail!("FRL Documents returned more than the requested {page_size} rows");
        }
        let before = documents.len();
        for rendition in &page {
            if !version.matches_rendition(rendition)? {
                bail!("FRL Documents returned a rendition outside the requested version");
            }
            let key = RenditionKey::from_rendition(rendition)?;
            if previous_key
                .as_ref()
                .is_some_and(|previous| key < *previous)
            {
                bail!("FRL Documents page is not ordered by its rendition key");
            }
            previous_key = Some(key.clone());
            documents.insert(key, rendition.clone());
        }
        if documents.len() == before && !page.is_empty() {
            bail!("FRL Documents keyset paging made no progress on page {page_number}");
        }
        if page.len() < page_size {
            return Ok(documents.into_values().collect());
        }
    }
    bail!("FRL Documents exceeded the bounded page limit")
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct RenditionKey {
    rectification_version_number: i64,
    document_type_ordinal: i64,
    document_type: String,
    unique_type_number: i64,
    volume_number: i64,
    format_ordinal: i64,
    format: String,
}

impl RenditionKey {
    fn from_rendition(rendition: &FrlRendition) -> Result<Self> {
        validate_odata_atom(&rendition.document_type, "document type")?;
        validate_odata_atom(&rendition.format, "document format")?;
        Ok(Self {
            rectification_version_number: rendition.rectification_version_number,
            document_type_ordinal: document_type_ordinal(&rendition.document_type)?,
            document_type: rendition.document_type.clone(),
            unique_type_number: rendition.unique_type_number,
            volume_number: rendition.volume_number,
            format_ordinal: document_format_ordinal(&rendition.format)?,
            format: rendition.format.clone(),
        })
    }
}

fn document_type_ordinal(value: &str) -> Result<i64> {
    match value {
        "Primary" => Ok(0),
        "ES" => Ok(1),
        "SupportingMaterial" => Ok(2),
        "IncorporatedByReference" => Ok(3),
        "SupplementaryES" => Ok(5),
        _ => bail!("unknown FRL document type `{value}`"),
    }
}

fn document_format_ordinal(value: &str) -> Result<i64> {
    match value {
        "Word" => Ok(1),
        "Pdf" => Ok(2),
        "Epub" => Ok(3),
        "NameOnly" => Ok(4),
        _ => bail!("unknown FRL document format `{value}`"),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
enum RenditionKind {
    Epub,
    Docx,
    Pdf,
}

fn rendition_kind(rendition: &FrlRendition) -> Option<RenditionKind> {
    let format = rendition.format.trim().to_ascii_lowercase();
    let extension = rendition
        .extension
        .as_deref()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if format == "epub" || extension == ".epub" {
        Some(RenditionKind::Epub)
    } else if (format == "word" || format == "docx") && extension == ".docx" {
        Some(RenditionKind::Docx)
    } else if format == "pdf" || extension == ".pdf" {
        Some(RenditionKind::Pdf)
    } else {
        None
    }
}

#[cfg(test)]
fn select_rendition(renditions: &[FrlRendition]) -> Result<FrlRendition> {
    rendition_candidates(renditions)?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("FRL rendition candidate selection returned no result"))
}

fn rendition_candidates(renditions: &[FrlRendition]) -> Result<Vec<FrlRendition>> {
    let mut candidates = renditions
        .iter()
        .filter(|rendition| rendition.document_type.eq_ignore_ascii_case("Primary"))
        .filter_map(|rendition| rendition_kind(rendition).map(|kind| (kind, rendition)))
        .collect::<Vec<_>>();
    candidates.sort_by(|(left_kind, left), (right_kind, right)| {
        left_kind
            .cmp(right_kind)
            .then_with(|| right.is_authorised.cmp(&left.is_authorised))
            .then_with(|| {
                right
                    .rectification_version_number
                    .cmp(&left.rectification_version_number)
            })
            .then_with(|| left.unique_type_number.cmp(&right.unique_type_number))
            .then_with(|| left.volume_number.cmp(&right.volume_number))
            .then_with(|| left.format.cmp(&right.format))
    });
    if candidates.is_empty() {
        let available = renditions
            .iter()
            .map(|rendition| {
                format!(
                    "{}/{}/{}",
                    rendition.document_type,
                    rendition.format,
                    rendition.extension.as_deref().unwrap_or("")
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        bail!("FRL version has no supported primary EPUB, DOCX, or PDF rendition; available: {available}");
    }
    Ok(candidates
        .into_iter()
        .map(|(_, rendition)| rendition.clone())
        .collect())
}

fn fetch_preferred_normalized_document(
    api: &dyn FrlApi,
    title: &FrlTitle,
    version: &FrlVersion,
    renditions: &[FrlRendition],
) -> Result<(FrlRendition, FrlNormalizedDocument)> {
    let candidates = rendition_candidates(renditions)?;
    let mut failures = Vec::new();
    for rendition in candidates {
        let result = api
            .fetch_rendition(&rendition)
            .with_context(|| {
                format!(
                    "fetching {} {}",
                    rendition.format,
                    rendition.extension.as_deref().unwrap_or("")
                )
            })
            .and_then(|payload| normalize_document(title, version, &rendition, payload));
        match result {
            Ok(document) => return Ok((rendition, document)),
            Err(error) => {
                if failures.len() < 8 {
                    failures.push(format!(
                        "{} {}: {error:#}",
                        rendition.format,
                        rendition.extension.as_deref().unwrap_or("")
                    ));
                }
            }
        }
    }
    bail!(
        "all supported official renditions failed for {}: {}",
        version.title_id,
        failures.join("; ")
    )
}

fn fetch_discovery(
    api: &dyn FrlApi,
    workspace: &Path,
    run_dir: &Path,
    discovery: &SourceDiscoveryBatch,
    mode: SourceUpdateMode,
    page_size: usize,
) -> Result<SourceFetchReport> {
    ensure_real_directory(workspace, "FRL workspace")?;
    ensure_real_directory(run_dir, "FRL run directory")?;
    let expected_path = confined_path(run_dir, Path::new(DISCOVERY_FILE_NAME))?;
    if discovery.path != expected_path {
        bail!("FRL discovery path is outside the run directory");
    }
    let metadata = fs::symlink_metadata(&discovery.path).with_context(|| {
        format!(
            "reading FRL discovery metadata {}",
            discovery.path.display()
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("FRL discovery plan must be a real file");
    }
    ensure_existing_path_within(run_dir, &discovery.path)?;
    let bytes = read_bounded_file(&discovery.path, MAX_STATE_BYTES)?;
    let envelope: FrlDiscoveryEnvelope =
        serde_json::from_slice(&bytes).context("decoding FRL discovery plan")?;
    validate_discovery_envelope(&envelope, mode)?;
    if envelope.plan.versions.len() != discovery.records {
        bail!("FRL discovery record count changed after discovery");
    }
    fetch_plan(api, workspace, envelope.plan, page_size)
}

fn fetch_plan(
    api: &dyn FrlApi,
    workspace: &Path,
    plan: FrlDiscoveryPlan,
    page_size: usize,
) -> Result<SourceFetchReport> {
    validate_discovery_plan(&plan)?;
    let previous_state = load_state(workspace)?;
    let authoritative_titles = plan
        .authoritative_titles
        .iter()
        .map(|title| (title.id.clone(), title.clone()))
        .collect::<BTreeMap<_, _>>();
    let authoritative_ids = authoritative_titles
        .keys()
        .cloned()
        .collect::<BTreeSet<_>>();
    if authoritative_ids.is_empty() {
        bail!("FRL authoritative discovery plan contains no in-force titles");
    }
    let mut reconciled_inventory = previous_state.inventory.clone();
    reconciled_inventory.retain(|native_id, _| authoritative_ids.contains(native_id));

    let versions_by_title = plan
        .versions
        .iter()
        .map(|version| (version.title_id.as_str(), version))
        .collect::<BTreeMap<_, _>>();
    let source: SourceId = FRL_SOURCE_ID.parse()?;
    let jobs = versions_by_title
        .into_iter()
        .map(|(title_id, version)| {
            authoritative_titles
                .get(title_id)
                .map(|title| (title_id, title, version))
                .ok_or_else(|| anyhow!("FRL version references absent title {title_id}"))
        })
        .collect::<Result<Vec<_>>>()?;
    let acquisition_context = FrlAcquisitionContext {
        api,
        workspace,
        source: &source,
        previous_inventory: &previous_state.inventory,
        allow_reuse: previous_state.schema_version == STATE_SCHEMA_VERSION,
        page_size,
    };
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(SOURCE_WORKER_CEILING)
        .thread_name(|index| format!("frl-fetch-{index}"))
        .build()
        .context("building FRL fetch pool")?;
    let mut acquisitions = pool.install(|| {
        jobs.par_iter()
            .map(|(title_id, title, version)| {
                acquire_planned_version(&acquisition_context, title_id, title, version)
            })
            .collect::<Result<Vec<_>>>()
    })?;
    acquisitions.sort_by(|left, right| left.0.cmp(&right.0));
    let mut completed = 0;
    let mut skipped = 0;
    for (title_id, entry, was_skipped) in acquisitions {
        if was_skipped {
            skipped += 1;
        } else {
            completed += 1;
        }
        reconciled_inventory.insert(title_id, entry);
    }
    let reconciled_ids = reconciled_inventory
        .keys()
        .cloned()
        .collect::<BTreeSet<_>>();
    if reconciled_ids != authoritative_ids {
        let missing = authoritative_ids
            .difference(&reconciled_ids)
            .take(10)
            .cloned()
            .collect::<Vec<_>>();
        bail!(
            "FRL reconciliation did not resolve every authoritative title; missing examples: {missing:?}"
        );
    }

    let next_cursor = match (&previous_state.cursor, &plan.proposed_cursor) {
        (Some(previous), Some(proposed))
            if compare_cursors(proposed, previous)? == Ordering::Less =>
        {
            Some(previous.clone())
        }
        (_, proposed) => proposed.clone().or_else(|| previous_state.cursor.clone()),
    };
    let next_state = FrlState {
        schema_version: STATE_SCHEMA_VERSION,
        cursor: next_cursor,
        inventory: reconciled_inventory,
    };
    commit_state(workspace, &next_state)?;
    clear_staging(workspace)?;
    Ok(SourceFetchReport {
        completed,
        failed: 0,
        skipped,
    })
}

struct FrlAcquisitionContext<'a> {
    api: &'a dyn FrlApi,
    workspace: &'a Path,
    source: &'a SourceId,
    previous_inventory: &'a BTreeMap<String, FrlInventoryEntry>,
    allow_reuse: bool,
    page_size: usize,
}

fn acquire_planned_version(
    context: &FrlAcquisitionContext<'_>,
    title_id: &str,
    title: &FrlTitle,
    version: &FrlVersion,
) -> Result<(String, FrlInventoryEntry, bool)> {
    let key = FrlVersionKey::from_version(version)?;
    let staged = match load_staging_entry(context.workspace, title_id) {
        Ok(staged) => staged,
        Err(error) => {
            eprintln!("FRL staged acquisition for {title_id} is unusable: {error:#}");
            None
        }
    };
    let reusable = context
        .allow_reuse
        .then(|| {
            staged
                .filter(|entry| entry.upstream_version == key)
                .or_else(|| {
                    context
                        .previous_inventory
                        .get(title_id)
                        .filter(|entry| entry.upstream_version == key)
                        .cloned()
                })
        })
        .flatten();
    if let Some(entry) = reusable {
        match load_inventory_document(context.workspace, context.source, title_id, &entry) {
            Ok(_) => return Ok((title_id.to_owned(), entry, true)),
            Err(error) => {
                eprintln!(
                    "FRL reusable acquisition for {title_id} failed validation and will be refreshed: {error:#}"
                );
            }
        }
    }
    let documents = scan_documents(context.api, &key, context.page_size)
        .with_context(|| format!("listing FRL renditions for {title_id}"))?;
    let (_rendition, document) =
        fetch_preferred_normalized_document(context.api, title, version, &documents)
            .with_context(|| format!("normalizing the authoritative FRL version for {title_id}"))?;
    let selected_version = version;
    let selected_key = key;
    let stored = persist_document(context.workspace, &document)?;
    let cursor = FrlCursor::from_version(selected_version)?.ok_or_else(|| {
        anyhow!(
            "FRL version {} has no registration time",
            selected_version.title_id
        )
    })?;
    let next_entry = FrlInventoryEntry {
        native_id: selected_version.title_id.clone(),
        upstream_version: selected_key,
        register_id: selected_version.register_id.clone(),
        canonical_url: document.canonical_url.clone(),
        payload_hash: stored.content_hash,
        last_successful_cursor: cursor,
    };
    commit_staging_entry(context.workspace, title_id, &next_entry)?;
    let unchanged = context.previous_inventory.get(title_id) == Some(&next_entry);
    Ok((title_id.to_owned(), next_entry, unchanged))
}

fn validate_discovery_envelope(
    envelope: &FrlDiscoveryEnvelope,
    mode: SourceUpdateMode,
) -> Result<()> {
    if envelope.schema_version != DISCOVERY_SCHEMA_VERSION {
        bail!("FRL discovery plan schema is unsupported");
    }
    if envelope.source != FRL_SOURCE_ID {
        bail!("FRL discovery plan source does not match");
    }
    if envelope.mode != mode {
        bail!("FRL discovery plan mode does not match");
    }
    validate_discovery_plan(&envelope.plan)
}

fn validate_discovery_plan(plan: &FrlDiscoveryPlan) -> Result<()> {
    let mut title_ids = BTreeSet::new();
    for title in &plan.authoritative_titles {
        validate_title(title)?;
        if !title_ids.insert(title.id.as_str()) {
            bail!("FRL discovery plan contains duplicate title {}", title.id);
        }
    }
    let mut version_titles = BTreeSet::new();
    for version in &plan.versions {
        canonicalize_version(version)?;
        if !title_ids.contains(version.title_id.as_str()) {
            bail!(
                "FRL discovery plan version references absent title {}",
                version.title_id
            );
        }
        if !version_titles.insert(version.title_id.as_str()) {
            bail!(
                "FRL discovery plan contains multiple selected versions for {}",
                version.title_id
            );
        }
    }
    if let Some(cursor) = &plan.proposed_cursor {
        cursor.validate()?;
    }
    Ok(())
}

fn load_state(workspace: &Path) -> Result<FrlState> {
    let path = confined_path(workspace, Path::new(STATE_RELATIVE_PATH))?;
    if !path.exists() {
        return Ok(FrlState {
            schema_version: STATE_SCHEMA_VERSION,
            ..FrlState::default()
        });
    }
    let metadata = fs::symlink_metadata(&path)
        .with_context(|| format!("reading FRL state metadata {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("FRL state must be a real file");
    }
    ensure_existing_path_within(workspace, &path)?;
    let bytes = read_bounded_file(&path, MAX_STATE_BYTES)?;
    let state: FrlState = serde_json::from_slice(&bytes).context("decoding FRL state")?;
    validate_state(&state)?;
    Ok(state)
}

fn validate_state(state: &FrlState) -> Result<()> {
    if !matches!(state.schema_version, 1 | STATE_SCHEMA_VERSION) {
        bail!(
            "unsupported FRL state schema version {}",
            state.schema_version
        );
    }
    if let Some(cursor) = &state.cursor {
        cursor.validate()?;
    }
    for (native_id, entry) in &state.inventory {
        validate_inventory_entry(native_id, entry)?;
    }
    Ok(())
}

fn validate_inventory_entry(native_id: &str, entry: &FrlInventoryEntry) -> Result<()> {
    validate_native_id(native_id)?;
    if entry.native_id != native_id || entry.upstream_version.title_id != native_id {
        bail!("FRL inventory key does not match its record");
    }
    if !is_lower_hex_sha256(&entry.payload_hash) {
        bail!("FRL inventory contains an invalid payload hash");
    }
    let canonical = Url::parse(&entry.canonical_url).context("parsing FRL canonical URL")?;
    if canonical.scheme() != "https" || canonical.host_str() != Some("www.legislation.gov.au") {
        bail!("FRL inventory contains a non-authoritative canonical URL");
    }
    entry.last_successful_cursor.validate()?;
    Ok(())
}

fn commit_state(workspace: &Path, state: &FrlState) -> Result<()> {
    validate_state(state)?;
    let mut bytes = serde_json::to_vec(state).context("serializing FRL state")?;
    bytes.push(b'\n');
    if bytes.len() as u64 > MAX_STATE_BYTES {
        bail!("FRL state exceeds the bounded state size");
    }
    atomic_write_confined(workspace, Path::new(STATE_RELATIVE_PATH), &bytes)
}

fn staging_relative_path(native_id: &str) -> Result<PathBuf> {
    validate_native_id(native_id)?;
    let digest = format!("{:x}", Sha256::digest(native_id.as_bytes()));
    Ok(PathBuf::from(STAGING_DIR)
        .join(&digest[..2])
        .join(format!("{digest}.json")))
}

fn load_staging_entry(workspace: &Path, native_id: &str) -> Result<Option<FrlInventoryEntry>> {
    let relative = staging_relative_path(native_id)?;
    let path = confined_path(workspace, &relative)?;
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("FRL staging entry must be a real file");
    }
    ensure_existing_path_within(workspace, &path)?;
    let bytes = read_bounded_file(&path, MAX_JSON_BODY_BYTES)?;
    let staged: FrlInventoryEntry =
        serde_json::from_slice(&bytes).context("decoding FRL staging entry")?;
    validate_inventory_entry(native_id, &staged)?;
    Ok(Some(staged))
}

fn commit_staging_entry(
    workspace: &Path,
    native_id: &str,
    entry: &FrlInventoryEntry,
) -> Result<()> {
    validate_inventory_entry(native_id, entry)?;
    let mut bytes = serde_json::to_vec(entry).context("serializing FRL staging entry")?;
    bytes.push(b'\n');
    atomic_write_confined(workspace, &staging_relative_path(native_id)?, &bytes)
}

fn clear_staging(workspace: &Path) -> Result<()> {
    let staging = confined_path(workspace, Path::new(STAGING_DIR))?;
    if !staging.exists() {
        return Ok(());
    }
    let metadata = fs::symlink_metadata(&staging)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("FRL staging path must be a real directory");
    }
    ensure_existing_path_within(workspace, &staging)?;
    fs::remove_dir_all(&staging).context("removing completed FRL staging data")
}

fn fingerprint_inventory(
    inventory: &BTreeMap<String, FrlInventoryEntry>,
) -> Result<SourceInventoryFingerprint> {
    let mut hasher = Sha256::new();
    for entry in inventory.values() {
        let bytes = serde_json::to_vec(entry).context("serializing FRL inventory record")?;
        hasher.update(&bytes);
        hasher.update(b"\n");
    }
    Ok(SourceInventoryFingerprint {
        records: inventory.len(),
        sha256: format!("{:x}", hasher.finalize()),
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FrlNormalizedAsset {
    asset_id: String,
    media_type: String,
    bytes: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FrlNormalizedDocument {
    source: String,
    native_id: String,
    title: String,
    document_type: String,
    date: Option<String>,
    citation: Option<String>,
    canonical_url: String,
    cleaned_html: String,
    assets: Vec<FrlNormalizedAsset>,
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

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
struct StoredAsset {
    asset_id: String,
    media_type: String,
    relative_path: String,
    size: usize,
    sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
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

#[derive(Clone, Debug)]
struct StoredDocumentResult {
    content_hash: String,
}

fn normalize_document(
    title: &FrlTitle,
    version: &FrlVersion,
    rendition: &FrlRendition,
    payload: FrlPayload,
) -> Result<FrlNormalizedDocument> {
    let display_title = title
        .name
        .as_deref()
        .or(version.name.as_deref())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("FRL title {} has no display name", title.id))?
        .to_owned();
    let (mut cleaned_html, mut assets) = match payload {
        FrlPayload::Epub(bytes) => normalize_epub(&bytes, &title.id)?,
        FrlPayload::Docx(bytes) => normalize_docx(&bytes, &title.id)?,
        FrlPayload::OfficialPdfText(text) => (normalize_official_pdf_text(&text)?, Vec::new()),
        FrlPayload::OfficialMetadata => {
            let mut html = String::from("<article><h1>");
            escape_text_into(&display_title, &mut html);
            html.push_str("</h1></article>");
            (html, Vec::new())
        }
    };
    append_low_text_image_ocr(&mut cleaned_html, &assets)?;
    assets.sort_by(|left, right| left.asset_id.cmp(&right.asset_id));
    assets.dedup_by(|left, right| left.asset_id == right.asset_id);
    let document_type = title
        .sub_collection
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(&title.collection)
        .trim()
        .to_owned();
    let date = title
        .making_date
        .as_deref()
        .map(|value| parse_datetime(value).map(|date| date.date().format("%Y-%m-%d").to_string()))
        .transpose()?;
    let canonical_url = format!("{FRL_PUBLIC_BASE}{}/latest/text", title.id);
    let mut document = FrlNormalizedDocument {
        source: FRL_SOURCE_ID.to_owned(),
        native_id: title.id.clone(),
        title: display_title,
        document_type,
        date,
        citation: version
            .register_id
            .clone()
            .or_else(|| rendition.register_id.clone()),
        canonical_url,
        cleaned_html,
        assets,
        content_hash: String::new(),
    };
    document.content_hash = normalized_content_hash(&document);
    Ok(document)
}

fn append_low_text_image_ocr(
    cleaned_html: &mut String,
    assets: &[FrlNormalizedAsset],
) -> Result<()> {
    let visible_characters = scraper::Html::parse_fragment(cleaned_html)
        .root_element()
        .text()
        .flat_map(str::chars)
        .filter(|character| character.is_alphanumeric())
        .count();
    if visible_characters >= MIN_FULL_TEXT_ALPHANUMERIC_CHARS {
        return Ok(());
    }
    let image_assets = assets
        .iter()
        .filter(|asset| asset.media_type.starts_with("image/"))
        .collect::<Vec<_>>();
    if image_assets.is_empty() {
        return Ok(());
    }
    let mut extracted = Vec::new();
    for asset in image_assets {
        if let Ok(text) = crate::official_sources::ocr_image_to_text(&asset.bytes) {
            extracted.push(text);
        }
    }
    if extracted.is_empty() {
        return Ok(());
    }
    let closing = cleaned_html
        .rfind("</article>")
        .ok_or_else(|| anyhow!("normalized FRL HTML has no article boundary"))?;
    let mut section = String::from("<section><h2>Text extracted from official document image</h2>");
    for text in extracted {
        for paragraph in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
            section.push_str("<p>");
            escape_text_into(paragraph, &mut section);
            section.push_str("</p>");
        }
    }
    section.push_str("</section>");
    cleaned_html.insert_str(closing, &section);
    Ok(())
}

fn normalized_content_hash(document: &FrlNormalizedDocument) -> String {
    let mut hasher = Sha256::new();
    for value in [
        document.source.as_str(),
        document.native_id.as_str(),
        document.title.as_str(),
        document.document_type.as_str(),
        document.date.as_deref().unwrap_or(""),
        document.citation.as_deref().unwrap_or(""),
        document.canonical_url.as_str(),
        document.cleaned_html.as_str(),
    ] {
        hasher.update((value.len() as u64).to_le_bytes());
        hasher.update(value.as_bytes());
    }
    for asset in &document.assets {
        hasher.update((asset.asset_id.len() as u64).to_le_bytes());
        hasher.update(asset.asset_id.as_bytes());
        hasher.update((asset.media_type.len() as u64).to_le_bytes());
        hasher.update(asset.media_type.as_bytes());
        hasher.update(Sha256::digest(&asset.bytes));
    }
    format!("{:x}", hasher.finalize())
}

fn persist_document(
    workspace: &Path,
    document: &FrlNormalizedDocument,
) -> Result<StoredDocumentResult> {
    if !is_lower_hex_sha256(&document.content_hash) {
        bail!("FRL normalized document has an invalid content hash");
    }
    let mut stored_assets = Vec::with_capacity(document.assets.len());
    for asset in &document.assets {
        let prefix = format!("{}/sha256-", document.native_id);
        if !is_lower_hex_sha256(asset.asset_id.strip_prefix(&prefix).unwrap_or("")) {
            bail!("FRL normalized asset has an invalid id");
        }
        let digest = format!("{:x}", Sha256::digest(&asset.bytes));
        let relative = PathBuf::from("assets").join(&digest[..2]).join(&digest);
        write_immutable_confined(workspace, &relative, &asset.bytes)?;
        stored_assets.push(StoredAsset {
            asset_id: asset.asset_id.clone(),
            media_type: asset.media_type.clone(),
            relative_path: path_to_slashes(&relative)?,
            size: asset.bytes.len(),
            sha256: digest,
        });
    }
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
        content_hash: &document.content_hash,
    };
    let mut bytes = serde_json::to_vec(&stored).context("serializing normalized FRL document")?;
    bytes.push(b'\n');
    let relative = PathBuf::from("documents")
        .join(&document.content_hash[..2])
        .join(format!("{}.json", document.content_hash));
    write_immutable_confined(workspace, &relative, &bytes)?;
    Ok(StoredDocumentResult {
        content_hash: document.content_hash.clone(),
    })
}

pub(crate) fn normalized_document_results(
    workspace: &Path,
) -> Result<Box<dyn Iterator<Item = Result<NormalizedDocument>>>> {
    ensure_real_directory(workspace, "FRL workspace")?;
    let state_path = confined_path(workspace, Path::new(STATE_RELATIVE_PATH))?;
    if !state_path.is_file() {
        bail!(
            "FRL workspace has no committed authoritative state at {}; run the FRL source update first",
            state_path.display()
        );
    }
    let state = load_state(workspace)?;
    if state.schema_version != STATE_SCHEMA_VERSION {
        bail!("FRL workspace uses a superseded normalizer; run a full FRL source update");
    }
    if state.inventory.is_empty() {
        bail!("FRL committed authoritative inventory is empty");
    }
    let source: SourceId = FRL_SOURCE_ID.parse()?;
    let link_map = state
        .inventory
        .values()
        .map(|entry| {
            Ok((
                Url::parse(&entry.canonical_url)?.to_string(),
                DocumentId::new(source.clone(), entry.native_id.clone())?,
            ))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    let workspace = workspace.to_path_buf();
    Ok(Box::new(state.inventory.into_iter().map(
        move |(native_id, entry)| {
            let mut document = load_inventory_document(&workspace, &source, &native_id, &entry)?;
            document.html = crate::official_sources::rewrite_internal_document_links(
                &document.html,
                &document.inventory.canonical_url,
                &link_map,
            )?;
            document.validate()?;
            Ok(document)
        },
    )))
}

#[cfg(test)]
pub(crate) fn load_normalized_documents(workspace: &Path) -> Result<Vec<NormalizedDocument>> {
    normalized_document_results(workspace)?.collect()
}

fn load_inventory_document(
    workspace: &Path,
    source: &SourceId,
    native_id: &str,
    entry: &FrlInventoryEntry,
) -> Result<NormalizedDocument> {
    validate_inventory_entry(native_id, entry)?;
    let relative = PathBuf::from("documents")
        .join(&entry.payload_hash[..2])
        .join(format!("{}.json", entry.payload_hash));
    let path = confined_path(workspace, &relative)?;
    ensure_existing_path_within(workspace, &path)?;
    let bytes = read_bounded_file(&path, MAX_STATE_BYTES)?;
    let stored: StoredDocumentOwned = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing normalized FRL document `{native_id}`"))?;
    if stored.source != FRL_SOURCE_ID
        || stored.native_id != native_id
        || stored.content_hash != entry.payload_hash
        || stored.canonical_url != entry.canonical_url
    {
        bail!("normalized FRL document `{native_id}` does not match its inventory entry");
    }
    let document_id = DocumentId::new(source.clone(), native_id.to_owned())?;
    let mut assets = Vec::with_capacity(stored.assets.len());
    let mut verified_assets = Vec::with_capacity(stored.assets.len());
    for stored_asset in &stored.assets {
        let asset_path = confined_path(workspace, Path::new(&stored_asset.relative_path))?;
        ensure_existing_path_within(workspace, &asset_path)?;
        let data = read_bounded_file(&asset_path, MAX_ARCHIVE_MEMBER_BYTES)?;
        if data.len() != stored_asset.size
            || format!("{:x}", Sha256::digest(&data)) != stored_asset.sha256
        {
            bail!(
                "normalized FRL asset `{}` failed integrity validation",
                stored_asset.asset_id
            );
        }
        verified_assets.push(FrlNormalizedAsset {
            asset_id: stored_asset.asset_id.clone(),
            media_type: stored_asset.media_type.clone(),
            bytes: data.clone(),
        });
        assets.push(NormalizedAsset::new(
            AssetRef::new(source.clone(), stored_asset.asset_id.clone())?,
            stored_asset.media_type.clone(),
            None,
            None,
            stored_asset.sha256.clone(),
            data,
        )?);
    }
    let reconstructed = FrlNormalizedDocument {
        source: stored.source.clone(),
        native_id: stored.native_id.clone(),
        title: stored.title.clone(),
        document_type: stored.document_type.clone(),
        date: stored.date.clone(),
        citation: stored.citation.clone(),
        canonical_url: stored.canonical_url.clone(),
        cleaned_html: stored.cleaned_html.clone(),
        assets: verified_assets,
        content_hash: String::new(),
    };
    if normalized_content_hash(&reconstructed) != entry.payload_hash {
        bail!("normalized FRL document `{native_id}` failed content-hash validation");
    }
    let upstream_version = entry.register_id.clone().or_else(|| {
        Some(format!(
            "{}|{}|{}",
            entry.upstream_version.title_id,
            entry.upstream_version.start,
            entry.upstream_version.retrospective_start
        ))
    });
    let inventory = SourceInventoryRecord::new(
        document_id,
        upstream_version,
        stored.canonical_url,
        stored.document_type,
        stored.title,
        stored.date,
        path_to_slashes(&relative)?,
        format!("{:x}", Sha256::digest(&bytes)),
        bytes.len() as u64,
        "application/vnd.australian-legal.normalized+json".to_string(),
    )?;
    NormalizedDocument::new(inventory, stored.cleaned_html, assets).map_err(Into::into)
}

fn normalize_official_pdf_text(text: &str) -> Result<String> {
    if text.len() as u64 > MAX_RENDITION_BYTES {
        bail!("FRL official PDF text exceeds the rendition limit");
    }
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let paragraphs = text
        .split("\n\n")
        .map(collapse_whitespace)
        .filter(|paragraph| !paragraph.is_empty())
        .collect::<Vec<_>>();
    if paragraphs.is_empty() {
        bail!("FRL PDF rendition supplies empty official extracted text");
    }
    let mut html = String::from("<article>");
    for paragraph in paragraphs {
        html.push_str("<p>");
        escape_text_into(&paragraph, &mut html);
        html.push_str("</p>");
    }
    html.push_str("</article>");
    Ok(html)
}

fn normalize_epub(bytes: &[u8], native_id: &str) -> Result<(String, Vec<FrlNormalizedAsset>)> {
    let archive = read_zip_archive(bytes, "EPUB")?;
    let container = archive_text(
        &archive,
        Path::new("META-INF/container.xml"),
        "EPUB container",
    )?;
    let container_xml = parse_xml(&container).context("parsing EPUB container.xml")?;
    let rootfile = descendants(&container_xml)
        .find(|node| local_name(&node.name) == "rootfile")
        .and_then(|node| attr_local(node, "full-path"))
        .ok_or_else(|| anyhow!("EPUB container has no rootfile"))?;
    let package_path = safe_archive_path(rootfile)?;
    let package = archive_text(&archive, &package_path, "EPUB package")?;
    let package_xml = parse_xml(&package).context("parsing EPUB package")?;
    let mut manifest = BTreeMap::<String, PackageItem>::new();
    for item in descendants(&package_xml).filter(|node| local_name(&node.name) == "item") {
        let Some(id) = attr_local(item, "id") else {
            continue;
        };
        let Some(href) = attr_local(item, "href") else {
            continue;
        };
        let path = resolve_archive_reference(&package_path, href)?;
        manifest.insert(
            id.to_owned(),
            PackageItem {
                path,
                media_type: attr_local(item, "media-type")
                    .unwrap_or("application/octet-stream")
                    .to_owned(),
            },
        );
    }
    let mut spine = Vec::new();
    for itemref in descendants(&package_xml).filter(|node| local_name(&node.name) == "itemref") {
        if let Some(item) = attr_local(itemref, "idref").and_then(|id| manifest.get(id)) {
            spine.push(item.path.clone());
        }
    }
    if spine.is_empty() {
        spine.extend(
            manifest
                .values()
                .filter(|item| {
                    matches!(
                        item.media_type.as_str(),
                        "application/xhtml+xml" | "text/html"
                    )
                })
                .map(|item| item.path.clone()),
        );
        spine.sort();
        spine.dedup();
    }
    if spine.is_empty() || spine.len() > MAX_ARCHIVE_ENTRIES {
        bail!("EPUB package has an invalid spine");
    }
    let spine_indices = spine
        .iter()
        .enumerate()
        .map(|(index, path)| (path.clone(), index + 1))
        .collect::<BTreeMap<_, _>>();
    let manifest_media = manifest
        .values()
        .map(|item| (item.path.clone(), item.media_type.clone()))
        .collect::<BTreeMap<_, _>>();
    let mut assets = AssetCollector::new(FRL_SOURCE_ID, native_id)?;
    let mut html = String::from("<article>");
    for (index, path) in spine.iter().enumerate() {
        let source = archive_text(&archive, path, "EPUB spine document")?;
        let document = parse_xml(&source)
            .with_context(|| format!("parsing EPUB spine document {}", path.display()))?;
        let body = descendants(&document)
            .find(|node| local_name(&node.name) == "body")
            .unwrap_or(&document);
        html.push_str(&format!("<section id=\"frl-part-{}\">", index + 1));
        let mut sink = HtmlSink::new(&mut html);
        for child in &body.children {
            render_epub_child(
                child,
                path,
                index + 1,
                &spine_indices,
                &manifest_media,
                &archive,
                &mut assets,
                &mut sink,
            )?;
        }
        sink.block_boundary();
        html.push_str("</section>");
    }
    html.push_str("</article>");
    Ok((html, assets.into_vec()))
}

#[derive(Clone, Debug)]
struct PackageItem {
    path: PathBuf,
    media_type: String,
}

#[allow(clippy::too_many_arguments)]
fn render_epub_child(
    child: &XmlChild,
    current_path: &Path,
    part_number: usize,
    spine_indices: &BTreeMap<PathBuf, usize>,
    manifest_media: &BTreeMap<PathBuf, String>,
    archive: &BTreeMap<PathBuf, Vec<u8>>,
    assets: &mut AssetCollector,
    sink: &mut HtmlSink<'_>,
) -> Result<()> {
    match child {
        XmlChild::Text(text) => sink.text(text),
        XmlChild::Node(node) => render_epub_node(
            node,
            current_path,
            part_number,
            spine_indices,
            manifest_media,
            archive,
            assets,
            sink,
        )?,
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn render_epub_node(
    node: &XmlNode,
    current_path: &Path,
    part_number: usize,
    spine_indices: &BTreeMap<PathBuf, usize>,
    manifest_media: &BTreeMap<PathBuf, String>,
    archive: &BTreeMap<PathBuf, Vec<u8>>,
    assets: &mut AssetCollector,
    sink: &mut HtmlSink<'_>,
) -> Result<()> {
    let tag = local_name(&node.name);
    if matches!(
        tag.as_str(),
        "script" | "style" | "form" | "iframe" | "object"
    ) {
        return Ok(());
    }
    if tag == "img" {
        let Some(src) = attr_local(node, "src") else {
            return Ok(());
        };
        let path = resolve_archive_reference(current_path, src)?;
        let bytes = archive
            .get(&path)
            .ok_or_else(|| anyhow!("EPUB image {} is absent", path.display()))?;
        let media_type = manifest_media
            .get(&path)
            .cloned()
            .unwrap_or_else(|| media_type_for_path(&path));
        let asset_ref = assets.insert(bytes.clone(), media_type)?;
        let mut attrs = vec![("data-asset-ref", asset_ref)];
        if let Some(alt) = attr_local(node, "alt").filter(|value| value.len() <= 2_048) {
            attrs.push(("alt", alt.to_owned()));
        }
        sink.empty("img", &attrs);
        return Ok(());
    }

    let allowed = matches!(
        tag.as_str(),
        "article"
            | "section"
            | "div"
            | "p"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "ol"
            | "ul"
            | "li"
            | "table"
            | "thead"
            | "tbody"
            | "tfoot"
            | "tr"
            | "th"
            | "td"
            | "blockquote"
            | "pre"
            | "code"
            | "em"
            | "strong"
            | "b"
            | "i"
            | "u"
            | "sup"
            | "sub"
            | "span"
            | "figure"
            | "figcaption"
            | "a"
    );
    if tag == "br" || tag == "hr" {
        sink.empty(&tag, &[]);
        return Ok(());
    }
    if !allowed {
        for child in &node.children {
            render_epub_child(
                child,
                current_path,
                part_number,
                spine_indices,
                manifest_media,
                archive,
                assets,
                sink,
            )?;
        }
        return Ok(());
    }

    let mut attributes = Vec::<(&str, String)>::new();
    if let Some(id) = attr_local(node, "id").and_then(safe_fragment) {
        attributes.push(("id", format!("frl-part-{part_number}-{id}")));
    }
    if tag == "a" {
        if let Some(href) = attr_local(node, "href") {
            if let Some(rewritten) =
                rewrite_epub_href(href, current_path, part_number, spine_indices)?
            {
                attributes.push(("href", rewritten));
            }
        }
    }
    if matches!(tag.as_str(), "td" | "th") {
        for name in ["colspan", "rowspan"] {
            if let Some(value) = attr_local(node, name)
                .filter(|value| value.parse::<u16>().is_ok_and(|number| number > 0))
            {
                attributes.push((name, value.to_owned()));
            }
        }
    }
    let block = is_block_tag(&tag);
    sink.open(&tag, &attributes, block);
    for child in &node.children {
        render_epub_child(
            child,
            current_path,
            part_number,
            spine_indices,
            manifest_media,
            archive,
            assets,
            sink,
        )?;
    }
    sink.close(&tag, block);
    Ok(())
}

fn rewrite_epub_href(
    href: &str,
    current_path: &Path,
    part_number: usize,
    spine_indices: &BTreeMap<PathBuf, usize>,
) -> Result<Option<String>> {
    let href = href.trim();
    if href.is_empty() {
        return Ok(None);
    }
    if let Ok(url) = Url::parse(href) {
        return if matches!(url.scheme(), "http" | "https") {
            Ok(Some(url.into()))
        } else {
            Ok(None)
        };
    }
    if let Some(fragment) = href.strip_prefix('#').and_then(safe_fragment) {
        return Ok(Some(format!("#frl-part-{part_number}-{fragment}")));
    }
    let (target, fragment) = href.split_once('#').unwrap_or((href, ""));
    let target_path = resolve_archive_reference(current_path, target)?;
    let Some(target_part) = spine_indices.get(&target_path) else {
        return Ok(None);
    };
    if fragment.is_empty() {
        Ok(Some(format!("#frl-part-{target_part}")))
    } else if let Some(fragment) = safe_fragment(fragment) {
        Ok(Some(format!("#frl-part-{target_part}-{fragment}")))
    } else {
        Ok(None)
    }
}

fn normalize_docx(bytes: &[u8], native_id: &str) -> Result<(String, Vec<FrlNormalizedAsset>)> {
    normalize_docx_with_source(bytes, FRL_SOURCE_ID, native_id)
}

pub(crate) fn normalize_docx_for_source(
    bytes: &[u8],
    source: &SourceId,
    native_id: &str,
) -> Result<(String, Vec<NormalizedAsset>)> {
    let (html, assets) = normalize_docx_with_source(bytes, source.as_str(), native_id)?;
    let assets = assets
        .into_iter()
        .map(|asset| {
            let sha256 = format!("{:x}", Sha256::digest(&asset.bytes));
            NormalizedAsset::new(
                AssetRef::new(source.clone(), asset.asset_id)?,
                asset.media_type,
                None,
                None,
                sha256,
                asset.bytes,
            )
            .map_err(Into::into)
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((html, assets))
}

#[derive(Clone, Copy, Debug)]
enum DocxNumberingSuffix {
    Tab,
    Space,
    Nothing,
}

#[derive(Clone, Debug)]
struct DocxNumberingLevel {
    start: i64,
    format: String,
    text: String,
    suffix: DocxNumberingSuffix,
    legal: bool,
    restart_after: Option<usize>,
}

#[derive(Clone, Debug, Default)]
struct DocxNumbering {
    instances: BTreeMap<i64, BTreeMap<usize, DocxNumberingLevel>>,
}

#[derive(Clone, Debug)]
struct DocxRawNumberingInstance {
    abstract_id: i64,
    node: XmlNode,
}

#[derive(Clone, Copy, Debug)]
struct DocxNumberingSetting {
    instance: Option<i64>,
    level: Option<usize>,
}

#[derive(Clone, Debug)]
struct DocxRawParagraphStyle {
    based_on: Option<String>,
    numbering: Option<DocxNumberingSetting>,
}

#[derive(Clone, Debug, Default)]
struct DocxParagraphStyles {
    default_style: Option<String>,
    numbering: BTreeMap<String, Option<(i64, usize)>>,
}

#[derive(Debug, Default)]
struct DocxNumberingState {
    counters: BTreeMap<i64, Vec<Option<i64>>>,
}

#[derive(Default)]
struct DocxNumberingLevelPatch {
    start: Option<i64>,
    format: Option<String>,
    text: Option<String>,
    suffix: Option<DocxNumberingSuffix>,
    legal: Option<bool>,
    restart_after: Option<Option<usize>>,
}

impl DocxNumberingLevelPatch {
    fn resolve(
        self,
        base: Option<&DocxNumberingLevel>,
        level: usize,
    ) -> Result<DocxNumberingLevel> {
        let format = self
            .format
            .or_else(|| base.map(|base| base.format.clone()))
            .unwrap_or_else(|| "decimal".to_owned());
        let text = self
            .text
            .or_else(|| base.map(|base| base.text.clone()))
            .or_else(|| format.eq_ignore_ascii_case("none").then(String::new))
            .ok_or_else(|| anyhow!("DOCX numbering level {level} has no level text"))?;
        Ok(DocxNumberingLevel {
            start: self
                .start
                .or_else(|| base.map(|base| base.start))
                .unwrap_or(1),
            format,
            text,
            suffix: self
                .suffix
                .or_else(|| base.map(|base| base.suffix))
                .unwrap_or(DocxNumberingSuffix::Tab),
            legal: self
                .legal
                .or_else(|| base.map(|base| base.legal))
                .unwrap_or(false),
            restart_after: self
                .restart_after
                .or_else(|| base.map(|base| base.restart_after))
                .unwrap_or_else(|| level.checked_sub(1)),
        })
    }
}

fn parse_docx_numbering(archive: &BTreeMap<PathBuf, Vec<u8>>) -> Result<DocxNumbering> {
    let path = Path::new("word/numbering.xml");
    let Some(bytes) = archive.get(path) else {
        return Ok(DocxNumbering::default());
    };
    let source = std::str::from_utf8(bytes).context("DOCX numbering is not UTF-8")?;
    let xml = parse_xml(source).context("parsing DOCX word/numbering.xml")?;
    let mut abstracts = BTreeMap::<i64, BTreeMap<usize, DocxNumberingLevel>>::new();
    let mut linked_abstracts = BTreeMap::<i64, String>::new();
    let mut abstract_ids = BTreeSet::new();
    for abstract_numbering in
        child_nodes(&xml).filter(|node| local_name(&node.name) == "abstractnum")
    {
        let id = parse_docx_nonnegative_integer(
            attr_local(abstract_numbering, "abstractnumid")
                .ok_or_else(|| anyhow!("DOCX abstract numbering has no id"))?,
            "abstract numbering id",
        )?;
        if !abstract_ids.insert(id) {
            bail!("DOCX numbering contains duplicate abstract id {id}");
        }
        let mut levels = BTreeMap::new();
        for level_node in
            child_nodes(abstract_numbering).filter(|node| local_name(&node.name) == "lvl")
        {
            let (level, patch) = parse_docx_numbering_level(level_node)?;
            let resolved = patch.resolve(None, level)?;
            if levels.insert(level, resolved).is_some() {
                bail!("DOCX abstract numbering {id} contains duplicate level {level}");
            }
        }
        if levels.is_empty() {
            let style = docx_numbering_style_link(abstract_numbering, "numstylelink")?
                .ok_or_else(|| anyhow!("DOCX abstract numbering {id} has no levels"))?;
            linked_abstracts.insert(id, style);
            continue;
        }
        abstracts.insert(id, levels);
    }

    let mut raw_instances = BTreeMap::new();
    for numbering in child_nodes(&xml).filter(|node| local_name(&node.name) == "num") {
        let id = parse_docx_nonnegative_integer(
            attr_local(numbering, "numid")
                .ok_or_else(|| anyhow!("DOCX numbering instance has no id"))?,
            "numbering instance id",
        )?;
        let abstract_id = child_nodes(numbering)
            .find(|node| local_name(&node.name) == "abstractnumid")
            .and_then(|node| attr_local(node, "val"))
            .ok_or_else(|| anyhow!("DOCX numbering instance {id} has no abstract id"))?;
        let abstract_id =
            parse_docx_nonnegative_integer(abstract_id, "numbering abstract reference")?;
        if raw_instances
            .insert(
                id,
                DocxRawNumberingInstance {
                    abstract_id,
                    node: numbering.clone(),
                },
            )
            .is_some()
        {
            bail!("DOCX numbering contains duplicate instance id {id}");
        }
    }

    let numbering_styles = parse_docx_numbering_style_ids(archive)?;
    let mut instances = BTreeMap::new();
    let mut resolving = BTreeSet::new();
    for id in raw_instances.keys().copied().collect::<Vec<_>>() {
        resolve_docx_numbering_instance(
            id,
            &raw_instances,
            &abstracts,
            &linked_abstracts,
            &numbering_styles,
            &mut resolving,
            &mut instances,
        )?;
    }
    Ok(DocxNumbering { instances })
}

fn resolve_docx_numbering_instance(
    id: i64,
    raw_instances: &BTreeMap<i64, DocxRawNumberingInstance>,
    abstracts: &BTreeMap<i64, BTreeMap<usize, DocxNumberingLevel>>,
    linked_abstracts: &BTreeMap<i64, String>,
    numbering_styles: &BTreeMap<String, i64>,
    resolving: &mut BTreeSet<i64>,
    instances: &mut BTreeMap<i64, BTreeMap<usize, DocxNumberingLevel>>,
) -> Result<BTreeMap<usize, DocxNumberingLevel>> {
    if let Some(levels) = instances.get(&id) {
        return Ok(levels.clone());
    }
    if resolving.len() >= MAX_DOCX_STYLE_REFERENCE_DEPTH {
        bail!("DOCX numbering style link exceeds its reference-depth bound");
    }
    if !resolving.insert(id) {
        bail!("DOCX numbering style link contains an instance cycle at {id}");
    }
    let raw = raw_instances
        .get(&id)
        .ok_or_else(|| anyhow!("DOCX numbering style refers to missing instance {id}"))?;
    let mut levels = if let Some(levels) = abstracts.get(&raw.abstract_id) {
        levels.clone()
    } else if let Some(style) = linked_abstracts.get(&raw.abstract_id) {
        let style_instance = numbering_styles.get(style).ok_or_else(|| {
            anyhow!(
                "DOCX abstract numbering {} links to missing numbering style {style}",
                raw.abstract_id
            )
        })?;
        resolve_docx_numbering_instance(
            *style_instance,
            raw_instances,
            abstracts,
            linked_abstracts,
            numbering_styles,
            resolving,
            instances,
        )?
    } else {
        bail!(
            "DOCX numbering instance {id} refers to missing abstract {}",
            raw.abstract_id
        );
    };
    apply_docx_numbering_overrides(id, &raw.node, &mut levels)?;
    resolving.remove(&id);
    instances.insert(id, levels.clone());
    Ok(levels)
}

fn apply_docx_numbering_overrides(
    id: i64,
    numbering: &XmlNode,
    levels: &mut BTreeMap<usize, DocxNumberingLevel>,
) -> Result<()> {
    let mut overridden = BTreeSet::new();
    for level_override in
        child_nodes(numbering).filter(|node| local_name(&node.name) == "lvloverride")
    {
        let level = parse_docx_level_index(
            attr_local(level_override, "ilvl")
                .ok_or_else(|| anyhow!("DOCX numbering override has no level"))?,
        )?;
        if !overridden.insert(level) {
            bail!("DOCX numbering instance {id} contains duplicate override {level}");
        }
        let mut resolved = levels
            .get(&level)
            .cloned()
            .ok_or_else(|| anyhow!("DOCX numbering override {level} has no abstract level"))?;
        if let Some(level_node) =
            child_nodes(level_override).find(|node| local_name(&node.name) == "lvl")
        {
            let (override_level, patch) = parse_docx_numbering_level(level_node)?;
            if override_level != level {
                bail!("DOCX numbering override level does not match its level definition");
            }
            resolved = patch.resolve(Some(&resolved), level)?;
        }
        if let Some(start) = child_nodes(level_override)
            .find(|node| local_name(&node.name) == "startoverride")
            .and_then(|node| attr_local(node, "val"))
        {
            resolved.start = parse_docx_start(start)?;
        }
        levels.insert(level, resolved);
    }
    Ok(())
}

fn docx_numbering_style_link(node: &XmlNode, name: &str) -> Result<Option<String>> {
    let Some(value) = child_nodes(node)
        .find(|child| local_name(&child.name) == name)
        .and_then(|child| attr_local(child, "val"))
    else {
        return Ok(None);
    };
    let value = value.trim();
    if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        bail!("DOCX numbering style link is invalid");
    }
    Ok(Some(value.to_owned()))
}

fn parse_docx_numbering_style_ids(
    archive: &BTreeMap<PathBuf, Vec<u8>>,
) -> Result<BTreeMap<String, i64>> {
    let Some(bytes) = archive.get(Path::new("word/styles.xml")) else {
        return Ok(BTreeMap::new());
    };
    let source = std::str::from_utf8(bytes).context("DOCX styles are not UTF-8")?;
    let xml = parse_xml(source).context("parsing DOCX word/styles.xml")?;
    let mut raw = BTreeMap::new();
    for style in child_nodes(&xml).filter(|node| {
        local_name(&node.name) == "style"
            && attr_local(node, "type").is_some_and(|value| value.eq_ignore_ascii_case("numbering"))
    }) {
        let id = parse_docx_style_id(
            attr_local(style, "styleid")
                .ok_or_else(|| anyhow!("DOCX numbering style has no id"))?,
        )?;
        let based_on = child_nodes(style)
            .find(|node| local_name(&node.name) == "basedon")
            .and_then(|node| attr_local(node, "val"))
            .map(parse_docx_style_id)
            .transpose()?;
        let numbering = child_nodes(style)
            .find(|node| local_name(&node.name) == "ppr")
            .and_then(|properties| {
                child_nodes(properties).find(|node| local_name(&node.name) == "numpr")
            })
            .map(parse_docx_numbering_setting)
            .transpose()?;
        if raw
            .insert(
                id.clone(),
                DocxRawParagraphStyle {
                    based_on,
                    numbering,
                },
            )
            .is_some()
        {
            bail!("DOCX styles contain duplicate numbering style {id}");
        }
    }
    let mut styles = BTreeMap::new();
    let mut resolving = BTreeSet::new();
    let mut resolved = BTreeMap::new();
    for id in raw.keys() {
        if let Some((instance, _)) =
            resolve_docx_paragraph_style(id, &raw, &mut resolving, &mut resolved)?
        {
            styles.insert(id.clone(), instance);
        }
    }
    Ok(styles)
}

fn parse_docx_style_id(value: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        bail!("DOCX style id is invalid");
    }
    Ok(value.to_owned())
}

fn parse_docx_numbering_setting(numbering: &XmlNode) -> Result<DocxNumberingSetting> {
    let instance = child_nodes(numbering)
        .find(|node| local_name(&node.name) == "numid")
        .and_then(|node| attr_local(node, "val"))
        .map(|value| parse_docx_nonnegative_integer(value, "numbering id"))
        .transpose()?;
    let level = child_nodes(numbering)
        .find(|node| local_name(&node.name) == "ilvl")
        .and_then(|node| attr_local(node, "val"))
        .map(parse_docx_level_index)
        .transpose()?;
    Ok(DocxNumberingSetting { instance, level })
}

fn resolve_docx_numbering_setting(
    setting: DocxNumberingSetting,
    inherited: Option<(i64, usize)>,
) -> Option<(i64, usize)> {
    if setting.instance == Some(0) {
        return None;
    }
    let instance = setting
        .instance
        .or_else(|| inherited.map(|(instance, _)| instance))?;
    let level = setting
        .level
        .or_else(|| inherited.map(|(_, level)| level))
        .unwrap_or(0);
    Some((instance, level))
}

fn parse_docx_paragraph_styles(
    archive: &BTreeMap<PathBuf, Vec<u8>>,
) -> Result<DocxParagraphStyles> {
    let Some(bytes) = archive.get(Path::new("word/styles.xml")) else {
        return Ok(DocxParagraphStyles::default());
    };
    let source = std::str::from_utf8(bytes).context("DOCX styles are not UTF-8")?;
    let xml = parse_xml(source).context("parsing DOCX word/styles.xml")?;
    let mut raw = BTreeMap::new();
    let mut default_style = None;
    for style in child_nodes(&xml).filter(|node| {
        local_name(&node.name) == "style"
            && attr_local(node, "type").is_some_and(|value| value.eq_ignore_ascii_case("paragraph"))
    }) {
        let id = parse_docx_style_id(
            attr_local(style, "styleid")
                .ok_or_else(|| anyhow!("DOCX paragraph style has no id"))?,
        )?;
        let based_on = child_nodes(style)
            .find(|node| local_name(&node.name) == "basedon")
            .and_then(|node| attr_local(node, "val"))
            .map(parse_docx_style_id)
            .transpose()?;
        let numbering = child_nodes(style)
            .find(|node| local_name(&node.name) == "ppr")
            .and_then(|properties| {
                child_nodes(properties).find(|node| local_name(&node.name) == "numpr")
            })
            .map(parse_docx_numbering_setting)
            .transpose()?;
        if attr_local(style, "default")
            .is_some_and(|value| matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "on"))
            && default_style.replace(id.clone()).is_some()
        {
            bail!("DOCX styles contain multiple default paragraph styles");
        }
        if raw
            .insert(
                id.clone(),
                DocxRawParagraphStyle {
                    based_on,
                    numbering,
                },
            )
            .is_some()
        {
            bail!("DOCX styles contain duplicate paragraph style {id}");
        }
    }

    let mut numbering = BTreeMap::new();
    let mut resolving = BTreeSet::new();
    for id in raw.keys().cloned().collect::<Vec<_>>() {
        resolve_docx_paragraph_style(&id, &raw, &mut resolving, &mut numbering)?;
    }
    Ok(DocxParagraphStyles {
        default_style,
        numbering,
    })
}

fn resolve_docx_paragraph_style(
    id: &str,
    raw: &BTreeMap<String, DocxRawParagraphStyle>,
    resolving: &mut BTreeSet<String>,
    resolved: &mut BTreeMap<String, Option<(i64, usize)>>,
) -> Result<Option<(i64, usize)>> {
    if let Some(numbering) = resolved.get(id) {
        return Ok(*numbering);
    }
    if resolving.len() >= MAX_DOCX_STYLE_REFERENCE_DEPTH {
        bail!("DOCX paragraph style inheritance exceeds its reference-depth bound");
    }
    if !resolving.insert(id.to_owned()) {
        bail!("DOCX paragraph style inheritance contains a cycle at {id}");
    }
    let style = raw
        .get(id)
        .ok_or_else(|| anyhow!("DOCX paragraph style inherits missing style {id}"))?;
    let inherited = match style.based_on.as_deref() {
        Some(base) => resolve_docx_paragraph_style(base, raw, resolving, resolved)?,
        None => None,
    };
    let numbering = match style.numbering {
        Some(setting) => resolve_docx_numbering_setting(setting, inherited),
        None => inherited,
    };
    resolving.remove(id);
    resolved.insert(id.to_owned(), numbering);
    Ok(numbering)
}

fn parse_docx_numbering_level(level: &XmlNode) -> Result<(usize, DocxNumberingLevelPatch)> {
    let index = parse_docx_level_index(
        attr_local(level, "ilvl").ok_or_else(|| anyhow!("DOCX numbering level has no index"))?,
    )?;
    let mut patch = DocxNumberingLevelPatch::default();
    for property in child_nodes(level) {
        match local_name(&property.name).as_str() {
            "start" => {
                patch.start = Some(parse_docx_start(
                    attr_local(property, "val")
                        .ok_or_else(|| anyhow!("DOCX numbering start has no value"))?,
                )?);
            }
            "numfmt" => {
                let format = attr_local(property, "val")
                    .ok_or_else(|| anyhow!("DOCX numbering format has no value"))?
                    .trim();
                if format.is_empty() || format.len() > 64 {
                    bail!("DOCX numbering format is invalid");
                }
                patch.format = Some(format.to_owned());
            }
            "lvltext" => {
                let text = attr_local(property, "val")
                    .ok_or_else(|| anyhow!("DOCX numbering level text has no value"))?;
                if text.len() > 256 || text.chars().any(char::is_control) {
                    bail!("DOCX numbering level text is invalid");
                }
                patch.text = Some(text.to_owned());
            }
            "suff" => {
                patch.suffix = Some(
                    match attr_local(property, "val")
                        .ok_or_else(|| anyhow!("DOCX numbering suffix has no value"))?
                        .to_ascii_lowercase()
                        .as_str()
                    {
                        "tab" => DocxNumberingSuffix::Tab,
                        "space" => DocxNumberingSuffix::Space,
                        "nothing" => DocxNumberingSuffix::Nothing,
                        value => bail!("unsupported DOCX numbering suffix {value}"),
                    },
                );
            }
            "islgl" => patch.legal = Some(parse_docx_on_off(attr_local(property, "val"))?),
            "lvlrestart" => {
                let value = parse_docx_nonnegative_integer(
                    attr_local(property, "val")
                        .ok_or_else(|| anyhow!("DOCX numbering restart has no value"))?,
                    "numbering restart",
                )?;
                patch.restart_after = Some(if value == 0 {
                    None
                } else {
                    let ancestor = usize::try_from(value - 1)?;
                    if ancestor >= index {
                        bail!("DOCX numbering restart must refer to a higher level");
                    }
                    Some(ancestor)
                });
            }
            _ => {}
        }
    }
    Ok((index, patch))
}

fn parse_docx_nonnegative_integer(value: &str, label: &str) -> Result<i64> {
    let value = value
        .parse::<i64>()
        .with_context(|| format!("DOCX {label} is not numeric"))?;
    if value < 0 {
        bail!("DOCX {label} is negative");
    }
    Ok(value)
}

fn parse_docx_level_index(value: &str) -> Result<usize> {
    let level = value
        .parse::<usize>()
        .context("DOCX numbering level is not numeric")?;
    if level > 8 {
        bail!("DOCX numbering level exceeds the OOXML level bound");
    }
    Ok(level)
}

fn parse_docx_start(value: &str) -> Result<i64> {
    let value = parse_docx_nonnegative_integer(value, "numbering start")?;
    if value > 1_000_000_000 {
        bail!("DOCX numbering start exceeds its bound");
    }
    Ok(value)
}

fn parse_docx_on_off(value: Option<&str>) -> Result<bool> {
    match value.map(str::to_ascii_lowercase).as_deref() {
        None | Some("1" | "true" | "on") => Ok(true),
        Some("0" | "false" | "off") => Ok(false),
        Some(value) => bail!("invalid DOCX on/off value {value}"),
    }
}

impl DocxNumbering {
    fn paragraph_marker(
        &self,
        paragraph: &XmlNode,
        styles: &DocxParagraphStyles,
        state: &mut DocxNumberingState,
    ) -> Result<Option<(String, DocxNumberingSuffix)>> {
        let Some((numbering_id, level_index)) = docx_paragraph_numbering(paragraph, styles)? else {
            return Ok(None);
        };
        let levels = self.instances.get(&numbering_id).ok_or_else(|| {
            anyhow!("DOCX paragraph refers to missing numbering instance {numbering_id}")
        })?;
        let level = levels.get(&level_index).ok_or_else(|| {
            anyhow!("DOCX numbering instance {numbering_id} has no level {level_index}")
        })?;
        let counters = state
            .counters
            .entry(numbering_id)
            .or_insert_with(|| vec![None; 9]);
        for (ancestor, counter) in counters.iter_mut().enumerate().take(level_index) {
            if counter.is_none() {
                if let Some(ancestor_level) = levels.get(&ancestor) {
                    *counter = Some(ancestor_level.start);
                }
            }
        }
        counters[level_index] = Some(match counters[level_index] {
            Some(value) => value
                .checked_add(1)
                .ok_or_else(|| anyhow!("DOCX numbering counter overflow"))?,
            None => level.start,
        });
        for (child_index, counter) in counters.iter_mut().enumerate().skip(level_index + 1) {
            if levels
                .get(&child_index)
                .is_some_and(|child| child.restart_after == Some(level_index))
            {
                *counter = None;
            }
        }
        let marker = render_docx_numbering_text(level, levels, counters)?;
        Ok(Some((marker, level.suffix)))
    }
}

fn docx_paragraph_numbering(
    paragraph: &XmlNode,
    styles: &DocxParagraphStyles,
) -> Result<Option<(i64, usize)>> {
    let properties = child_nodes(paragraph).find(|node| local_name(&node.name) == "ppr");
    if let Some(numbering) = properties.and_then(|properties| {
        child_nodes(properties).find(|node| local_name(&node.name) == "numpr")
    }) {
        let inherited = paragraph_or_default_style_numbering(properties, styles);
        return Ok(resolve_docx_numbering_setting(
            parse_docx_numbering_setting(numbering)?,
            inherited,
        ));
    }
    Ok(paragraph_or_default_style_numbering(properties, styles))
}

fn paragraph_style_numbering(
    properties: &XmlNode,
    styles: &DocxParagraphStyles,
) -> Option<Option<(i64, usize)>> {
    child_nodes(properties)
        .find(|node| local_name(&node.name) == "pstyle")
        .and_then(|node| attr_local(node, "val"))
        .map(|style| styles.numbering.get(style).copied().flatten())
}

fn paragraph_or_default_style_numbering(
    properties: Option<&XmlNode>,
    styles: &DocxParagraphStyles,
) -> Option<(i64, usize)> {
    match properties.and_then(|properties| paragraph_style_numbering(properties, styles)) {
        Some(numbering) => numbering,
        None => styles
            .default_style
            .as_deref()
            .and_then(|style| styles.numbering.get(style).copied().flatten()),
    }
}

fn render_docx_numbering_text(
    level: &DocxNumberingLevel,
    levels: &BTreeMap<usize, DocxNumberingLevel>,
    counters: &[Option<i64>],
) -> Result<String> {
    let mut output = String::new();
    let mut characters = level.text.chars().peekable();
    while let Some(character) = characters.next() {
        if character != '%' {
            output.push(character);
            continue;
        }
        match characters.peek().copied() {
            Some('%') => {
                characters.next();
                output.push('%');
            }
            Some(reference @ '1'..='9') => {
                characters.next();
                let index = reference as usize - '1' as usize;
                let value =
                    counters.get(index).copied().flatten().ok_or_else(|| {
                        anyhow!("DOCX numbering text refers to unset level {index}")
                    })?;
                let referenced_level = levels.get(&index).ok_or_else(|| {
                    anyhow!("DOCX numbering text refers to missing level {index}")
                })?;
                let format = if level.legal {
                    "decimal"
                } else {
                    &referenced_level.format
                };
                output.push_str(&format_docx_number(value, format));
            }
            _ => output.push('%'),
        }
    }
    Ok(output)
}

fn format_docx_number(value: i64, format: &str) -> String {
    match format.to_ascii_lowercase().as_str() {
        "none" => String::new(),
        "decimalzero" => format!("{value:02}"),
        "upperletter" => format_docx_letters(value, true),
        "lowerletter" => format_docx_letters(value, false),
        "upperroman" => format_docx_roman(value, true),
        "lowerroman" => format_docx_roman(value, false),
        "ordinal" | "ordinaltext" => format_docx_ordinal(value),
        "hex" => format!("{value:X}"),
        // Static bullet level text has no placeholder. For uncommon locale-
        // specific formats, a decimal counter is a stable source-derived
        // fallback that preserves the paragraph marker instead of dropping it.
        _ => value.to_string(),
    }
}

fn format_docx_letters(value: i64, uppercase: bool) -> String {
    if value <= 0 {
        return value.to_string();
    }
    let mut value = value as u64;
    let mut characters = Vec::new();
    while value > 0 {
        value -= 1;
        let character = (b'a' + (value % 26) as u8) as char;
        characters.push(if uppercase {
            character.to_ascii_uppercase()
        } else {
            character
        });
        value /= 26;
    }
    characters.into_iter().rev().collect()
}

fn format_docx_roman(value: i64, uppercase: bool) -> String {
    if !(1..=3_999).contains(&value) {
        return value.to_string();
    }
    let mut value = value;
    let mut output = String::new();
    for (number, marker) in [
        (1_000, "M"),
        (900, "CM"),
        (500, "D"),
        (400, "CD"),
        (100, "C"),
        (90, "XC"),
        (50, "L"),
        (40, "XL"),
        (10, "X"),
        (9, "IX"),
        (5, "V"),
        (4, "IV"),
        (1, "I"),
    ] {
        while value >= number {
            output.push_str(marker);
            value -= number;
        }
    }
    if uppercase {
        output
    } else {
        output.to_ascii_lowercase()
    }
}

fn format_docx_ordinal(value: i64) -> String {
    let suffix = if (11..=13).contains(&(value % 100)) {
        "th"
    } else {
        match value % 10 {
            1 => "st",
            2 => "nd",
            3 => "rd",
            _ => "th",
        }
    };
    format!("{value}{suffix}")
}

fn normalize_docx_with_source(
    bytes: &[u8],
    source_id: &str,
    native_id: &str,
) -> Result<(String, Vec<FrlNormalizedAsset>)> {
    let archive = read_zip_archive(bytes, "DOCX")?;
    let document_path = Path::new("word/document.xml");
    let document = archive_text(&archive, document_path, "DOCX document")?;
    let xml = parse_xml(&document).context("parsing DOCX word/document.xml")?;
    let relationships = parse_docx_relationships(&archive)?;
    let content_types = parse_docx_content_types(&archive)?;
    let numbering = parse_docx_numbering(&archive)?;
    let styles = parse_docx_paragraph_styles(&archive)?;
    let mut referenced_footnote_ids = BTreeSet::new();
    let mut referenced_footnotes = Vec::new();
    collect_visible_docx_footnote_references(
        &xml,
        &mut referenced_footnote_ids,
        &mut referenced_footnotes,
    )?;
    let footnote_markers = referenced_footnotes
        .iter()
        .enumerate()
        .map(|(index, id)| (*id, (index + 1).to_string()))
        .collect::<BTreeMap<_, _>>();
    let body = descendants(&xml)
        .find(|node| local_name(&node.name) == "body")
        .ok_or_else(|| anyhow!("DOCX document has no body"))?;
    let mut assets = AssetCollector::new(source_id, native_id)?;
    let mut html = String::from("<article>");
    let mut sink = HtmlSink::new(&mut html);
    let mut numbering_state = DocxNumberingState::default();
    let render_context = DocxRenderContext {
        relationships: &relationships,
        content_types: &content_types,
        archive: &archive,
        numbering: &numbering,
        styles: &styles,
        footnote_markers: &footnote_markers,
    };
    for child in &body.children {
        if let XmlChild::Node(node) = child {
            render_docx_block(
                node,
                &render_context,
                &mut numbering_state,
                &mut assets,
                &mut sink,
            )?;
        }
    }
    if !referenced_footnotes.is_empty() {
        render_docx_footnotes(
            &archive,
            &referenced_footnotes,
            &numbering,
            &styles,
            &footnote_markers,
            &mut assets,
            &mut sink,
        )?;
    }
    sink.block_boundary();
    html.push_str("</article>");
    Ok((html, assets.into_vec()))
}

fn collect_visible_docx_footnote_references(
    node: &XmlNode,
    seen: &mut BTreeSet<i64>,
    ordered: &mut Vec<i64>,
) -> Result<()> {
    if local_name(&node.name) == "del" {
        return Ok(());
    }
    if local_name(&node.name) == "footnotereference" {
        let id = attr_local(node, "id")
            .ok_or_else(|| anyhow!("DOCX footnote reference has no id"))?
            .parse::<i64>()
            .context("DOCX footnote reference id is not numeric")?;
        if id < 0 {
            bail!("DOCX document refers to reserved footnote id {id}");
        }
        if !seen.insert(id) {
            bail!("DOCX document contains duplicate footnote reference id {id}");
        }
        ordered.push(id);
    }
    for child in child_nodes(node) {
        collect_visible_docx_footnote_references(child, seen, ordered)?;
    }
    Ok(())
}

fn render_docx_footnotes(
    archive: &BTreeMap<PathBuf, Vec<u8>>,
    referenced: &[i64],
    numbering: &DocxNumbering,
    styles: &DocxParagraphStyles,
    footnote_markers: &BTreeMap<i64, String>,
    assets: &mut AssetCollector,
    sink: &mut HtmlSink<'_>,
) -> Result<()> {
    let content_types = parse_docx_content_types(archive)?;
    let footnotes = archive_text(archive, Path::new("word/footnotes.xml"), "DOCX footnotes")?;
    let xml = parse_xml(&footnotes).context("parsing DOCX word/footnotes.xml")?;
    let relationships = parse_docx_relationships_at(
        archive,
        Path::new("word/_rels/footnotes.xml.rels"),
        "DOCX footnote relationships",
    )?;
    let referenced_ids = referenced.iter().copied().collect::<BTreeSet<_>>();
    let mut by_id = BTreeMap::new();
    for footnote in descendants(&xml).filter(|node| local_name(&node.name) == "footnote") {
        let Some(id) = attr_local(footnote, "id") else {
            continue;
        };
        let id = id
            .parse::<i64>()
            .context("DOCX footnote id is not numeric")?;
        if referenced_ids.contains(&id) && by_id.insert(id, footnote).is_some() {
            bail!("DOCX footnotes contain duplicate id {id}");
        }
    }
    let missing = referenced
        .iter()
        .filter(|id| !by_id.contains_key(id))
        .copied()
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        bail!("DOCX footnotes are missing referenced ids {missing:?}");
    }

    sink.open("section", &[("id", "footnotes".to_owned())], true);
    sink.open("h2", &[], true);
    sink.text("Footnotes");
    sink.close("h2", true);
    sink.open("ol", &[], true);
    let mut numbering_state = DocxNumberingState::default();
    let render_context = DocxRenderContext {
        relationships: &relationships,
        content_types: &content_types,
        archive,
        numbering,
        styles,
        footnote_markers,
    };
    for id in referenced {
        let footnote = by_id
            .get(id)
            .copied()
            .ok_or_else(|| anyhow!("DOCX footnote {id} disappeared during rendering"))?;
        sink.open("li", &[("id", format!("footnote-{id}"))], true);
        sink.open("a", &[("href", format!("#footnote-reference-{id}"))], false);
        sink.close("a", false);
        for block in child_nodes(footnote) {
            render_docx_block(block, &render_context, &mut numbering_state, assets, sink)?;
        }
        sink.close("li", true);
    }
    sink.close("ol", true);
    sink.close("section", true);
    Ok(())
}

#[derive(Clone, Debug)]
struct DocxRelationship {
    target: String,
    external: bool,
}

fn parse_docx_relationships(
    archive: &BTreeMap<PathBuf, Vec<u8>>,
) -> Result<BTreeMap<String, DocxRelationship>> {
    parse_docx_relationships_at(
        archive,
        Path::new("word/_rels/document.xml.rels"),
        "DOCX relationships",
    )
}

fn parse_docx_relationships_at(
    archive: &BTreeMap<PathBuf, Vec<u8>>,
    path: &Path,
    label: &str,
) -> Result<BTreeMap<String, DocxRelationship>> {
    let Some(bytes) = archive.get(path) else {
        return Ok(BTreeMap::new());
    };
    let source = std::str::from_utf8(bytes).with_context(|| format!("{label} are not UTF-8"))?;
    let xml = parse_xml(source).with_context(|| format!("parsing {label}"))?;
    let mut relationships = BTreeMap::new();
    for node in descendants(&xml).filter(|node| local_name(&node.name) == "relationship") {
        let (Some(id), Some(target)) = (attr_local(node, "id"), attr_local(node, "target")) else {
            continue;
        };
        if relationships.contains_key(id) {
            bail!("DOCX relationships contain duplicate id {id}");
        }
        relationships.insert(
            id.to_owned(),
            DocxRelationship {
                target: target.to_owned(),
                external: attr_local(node, "targetmode")
                    .is_some_and(|value| value.eq_ignore_ascii_case("External")),
            },
        );
    }
    Ok(relationships)
}

fn parse_docx_content_types(
    archive: &BTreeMap<PathBuf, Vec<u8>>,
) -> Result<BTreeMap<String, String>> {
    let source = archive_text(
        archive,
        Path::new("[Content_Types].xml"),
        "DOCX content types",
    )?;
    let xml = parse_xml(&source).context("parsing DOCX content types")?;
    let mut defaults = BTreeMap::new();
    for node in descendants(&xml).filter(|node| local_name(&node.name) == "default") {
        if let (Some(extension), Some(content_type)) = (
            attr_local(node, "extension"),
            attr_local(node, "contenttype"),
        ) {
            defaults.insert(
                extension.trim_start_matches('.').to_ascii_lowercase(),
                content_type.to_owned(),
            );
        }
    }
    Ok(defaults)
}

struct DocxRenderContext<'a> {
    relationships: &'a BTreeMap<String, DocxRelationship>,
    content_types: &'a BTreeMap<String, String>,
    archive: &'a BTreeMap<PathBuf, Vec<u8>>,
    numbering: &'a DocxNumbering,
    styles: &'a DocxParagraphStyles,
    footnote_markers: &'a BTreeMap<i64, String>,
}

fn render_docx_block(
    node: &XmlNode,
    context: &DocxRenderContext<'_>,
    numbering_state: &mut DocxNumberingState,
    assets: &mut AssetCollector,
    sink: &mut HtmlSink<'_>,
) -> Result<()> {
    match local_name(&node.name).as_str() {
        "p" => render_docx_paragraph(node, context, numbering_state, assets, sink),
        "tbl" => {
            sink.open("table", &[], true);
            for row in child_nodes(node).filter(|child| local_name(&child.name) == "tr") {
                sink.open("tr", &[], true);
                for cell in child_nodes(row).filter(|child| local_name(&child.name) == "tc") {
                    sink.open("td", &[], true);
                    for block in child_nodes(cell) {
                        render_docx_block(block, context, numbering_state, assets, sink)?;
                    }
                    sink.close("td", true);
                }
                sink.close("tr", true);
            }
            sink.close("table", true);
            Ok(())
        }
        _ => {
            for child in child_nodes(node) {
                render_docx_block(child, context, numbering_state, assets, sink)?;
            }
            Ok(())
        }
    }
}

fn render_docx_paragraph(
    paragraph: &XmlNode,
    context: &DocxRenderContext<'_>,
    numbering_state: &mut DocxNumberingState,
    assets: &mut AssetCollector,
    sink: &mut HtmlSink<'_>,
) -> Result<()> {
    let style = descendants(paragraph)
        .find(|node| local_name(&node.name) == "pstyle")
        .and_then(|node| attr_local(node, "val"));
    let tag = heading_tag(style).unwrap_or("p");
    sink.open(tag, &[], true);
    if let Some((marker, suffix)) =
        context
            .numbering
            .paragraph_marker(paragraph, context.styles, numbering_state)?
    {
        if !marker.is_empty() {
            sink.text(&marker);
            if matches!(
                suffix,
                DocxNumberingSuffix::Tab | DocxNumberingSuffix::Space
            ) {
                sink.text(" ");
            }
        }
    }
    for child in &paragraph.children {
        render_docx_inline(child, context, assets, sink)?;
    }
    sink.close(tag, true);
    Ok(())
}

fn render_docx_inline(
    child: &XmlChild,
    context: &DocxRenderContext<'_>,
    assets: &mut AssetCollector,
    sink: &mut HtmlSink<'_>,
) -> Result<()> {
    let XmlChild::Node(node) = child else {
        return Ok(());
    };
    match local_name(&node.name).as_str() {
        "t" => {
            for text in &node.children {
                if let XmlChild::Text(text) = text {
                    sink.text(text);
                }
            }
        }
        "instrtext" | "fldchar" => return Ok(()),
        "footnotereference" => {
            let id = attr_local(node, "id")
                .ok_or_else(|| anyhow!("DOCX footnote reference has no id"))?
                .parse::<i64>()
                .context("DOCX footnote reference id is not numeric")?;
            if id < 0 {
                bail!("DOCX document refers to reserved footnote id {id}");
            }
            sink.open("sup", &[], false);
            sink.open(
                "a",
                &[
                    ("id", format!("footnote-reference-{id}")),
                    ("href", format!("#footnote-{id}")),
                ],
                false,
            );
            let marker = context
                .footnote_markers
                .get(&id)
                .ok_or_else(|| anyhow!("DOCX footnote reference {id} has no display marker"))?;
            sink.text(marker);
            sink.close("a", false);
            sink.close("sup", false);
        }
        "tab" => sink.text("\t"),
        "br" | "cr" => sink.empty("br", &[]),
        "del" => return Ok(()),
        "r" => {
            let properties = child_nodes(node).find(|child| local_name(&child.name) == "rpr");
            let bold = properties.is_some_and(|properties| has_child(properties, "b"));
            let italic = properties.is_some_and(|properties| has_child(properties, "i"));
            let underline = properties.is_some_and(|properties| has_child(properties, "u"));
            let vertical = properties
                .and_then(|properties| {
                    child_nodes(properties).find(|child| local_name(&child.name) == "vertalign")
                })
                .and_then(|node| attr_local(node, "val"));
            if bold {
                sink.open("strong", &[], false);
            }
            if italic {
                sink.open("em", &[], false);
            }
            if underline {
                sink.open("u", &[], false);
            }
            if vertical == Some("superscript") {
                sink.open("sup", &[], false);
            } else if vertical == Some("subscript") {
                sink.open("sub", &[], false);
            }
            for run_child in &node.children {
                if let XmlChild::Node(run_node) = run_child {
                    if local_name(&run_node.name) == "rpr" {
                        continue;
                    }
                }
                render_docx_inline(run_child, context, assets, sink)?;
            }
            if vertical == Some("superscript") {
                sink.close("sup", false);
            } else if vertical == Some("subscript") {
                sink.close("sub", false);
            }
            if underline {
                sink.close("u", false);
            }
            if italic {
                sink.close("em", false);
            }
            if bold {
                sink.close("strong", false);
            }
        }
        "hyperlink" => {
            let href = attr_local(node, "id")
                .and_then(|id| context.relationships.get(id))
                .filter(|relationship| relationship.external)
                .and_then(|relationship| Url::parse(&relationship.target).ok())
                .filter(|url| matches!(url.scheme(), "http" | "https"))
                .map(String::from);
            if let Some(href) = href {
                sink.open("a", &[("href", href)], false);
            }
            for hyperlink_child in &node.children {
                render_docx_inline(hyperlink_child, context, assets, sink)?;
            }
            if attr_local(node, "id")
                .and_then(|id| context.relationships.get(id))
                .is_some_and(|relationship| {
                    relationship.external
                        && Url::parse(&relationship.target)
                            .is_ok_and(|url| matches!(url.scheme(), "http" | "https"))
                })
            {
                sink.close("a", false);
            }
        }
        "blip" => {
            let Some(relationship) = attr_local(node, "embed")
                .and_then(|id| context.relationships.get(id))
                .filter(|relationship| !relationship.external)
            else {
                return Ok(());
            };
            let path =
                resolve_archive_reference(Path::new("word/document.xml"), &relationship.target)?;
            let bytes = context
                .archive
                .get(&path)
                .ok_or_else(|| anyhow!("DOCX image {} is absent", path.display()))?;
            let extension = path
                .extension()
                .and_then(|value| value.to_str())
                .unwrap_or_default()
                .to_ascii_lowercase();
            let media_type = context
                .content_types
                .get(&extension)
                .cloned()
                .unwrap_or_else(|| media_type_for_path(&path));
            let asset_ref = assets.insert(bytes.clone(), media_type)?;
            sink.empty("img", &[("data-asset-ref", asset_ref)]);
        }
        _ => {
            for nested in &node.children {
                render_docx_inline(nested, context, assets, sink)?;
            }
        }
    }
    Ok(())
}

fn heading_tag(style: Option<&str>) -> Option<&'static str> {
    let style = style?.trim().to_ascii_lowercase().replace(' ', "");
    if style == "title" {
        return Some("h1");
    }
    let level = style
        .strip_prefix("heading")?
        .parse::<usize>()
        .ok()?
        .clamp(1, 6);
    Some(match level {
        1 => "h1",
        2 => "h2",
        3 => "h3",
        4 => "h4",
        5 => "h5",
        _ => "h6",
    })
}

struct AssetCollector {
    source_id: SourceId,
    native_id: String,
    assets: BTreeMap<String, FrlNormalizedAsset>,
}

impl AssetCollector {
    fn new(source_id: &str, native_id: &str) -> Result<Self> {
        let source = SourceId::new(source_id)?;
        DocumentId::new(source.clone(), native_id.to_owned())?;
        Ok(Self {
            source_id: source,
            native_id: native_id.to_owned(),
            assets: BTreeMap::new(),
        })
    }

    fn insert(&mut self, bytes: Vec<u8>, media_type: String) -> Result<String> {
        if bytes.len() as u64 > MAX_ARCHIVE_MEMBER_BYTES {
            bail!("FRL normalized asset exceeds the member limit");
        }
        let media_type = media_type.trim().to_ascii_lowercase();
        if !media_type.starts_with("image/") {
            bail!("FRL retained asset has non-image media type {media_type}");
        }
        let mut hasher = Sha256::new();
        hasher.update((media_type.len() as u64).to_le_bytes());
        hasher.update(media_type.as_bytes());
        hasher.update(&bytes);
        let hash = format!("{:x}", hasher.finalize());
        let asset_id = format!("{}/sha256-{hash}", self.native_id);
        self.assets
            .entry(asset_id.clone())
            .or_insert(FrlNormalizedAsset {
                asset_id: asset_id.clone(),
                media_type,
                bytes,
            });
        Ok(AssetRef::new(self.source_id.clone(), asset_id)?.public_ref())
    }

    fn into_vec(self) -> Vec<FrlNormalizedAsset> {
        self.assets.into_values().collect()
    }
}

fn media_type_for_path(path: &Path) -> String {
    match path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        _ => "application/octet-stream",
    }
    .to_owned()
}

struct HtmlSink<'a> {
    output: &'a mut String,
    pending_space: bool,
    has_inline_text: bool,
}

impl<'a> HtmlSink<'a> {
    fn new(output: &'a mut String) -> Self {
        Self {
            output,
            pending_space: false,
            has_inline_text: false,
        }
    }

    fn block_boundary(&mut self) {
        self.pending_space = false;
        self.has_inline_text = false;
    }

    fn text(&mut self, text: &str) {
        for character in text.chars() {
            if character.is_whitespace() {
                self.pending_space |= self.has_inline_text;
                continue;
            }
            if self.pending_space && self.has_inline_text {
                self.output.push(' ');
            }
            self.pending_space = false;
            escape_char_into(character, self.output);
            self.has_inline_text = true;
        }
    }

    fn open(&mut self, tag: &str, attributes: &[(&str, String)], block: bool) {
        if block {
            self.block_boundary();
        } else if self.pending_space && self.has_inline_text {
            self.output.push(' ');
            self.pending_space = false;
        }
        self.output.push('<');
        self.output.push_str(tag);
        for (name, value) in attributes {
            self.output.push(' ');
            self.output.push_str(name);
            self.output.push_str("=\"");
            escape_attribute_into(value, self.output);
            self.output.push('"');
        }
        self.output.push('>');
    }

    fn close(&mut self, tag: &str, block: bool) {
        if block {
            self.pending_space = false;
        }
        self.output.push_str("</");
        self.output.push_str(tag);
        self.output.push('>');
        if block {
            self.block_boundary();
        }
    }

    fn empty(&mut self, tag: &str, attributes: &[(&str, String)]) {
        self.open(tag, attributes, false);
        self.pending_space = false;
    }
}

fn is_block_tag(tag: &str) -> bool {
    matches!(
        tag,
        "article"
            | "section"
            | "div"
            | "p"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "ol"
            | "ul"
            | "li"
            | "table"
            | "thead"
            | "tbody"
            | "tfoot"
            | "tr"
            | "th"
            | "td"
            | "blockquote"
            | "pre"
            | "figure"
            | "figcaption"
    )
}

fn escape_text_into(value: &str, output: &mut String) {
    for character in value.chars() {
        escape_char_into(character, output);
    }
}

fn escape_char_into(character: char, output: &mut String) {
    match character {
        '&' => output.push_str("&amp;"),
        '<' => output.push_str("&lt;"),
        '>' => output.push_str("&gt;"),
        _ if character.is_control() && !character.is_ascii_whitespace() => {}
        _ => output.push(character),
    }
}

fn escape_attribute_into(value: &str, output: &mut String) {
    for character in value.chars() {
        match character {
            '"' => output.push_str("&quot;"),
            '\'' => output.push_str("&#39;"),
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            _ if character.is_control() => {}
            _ => output.push(character),
        }
    }
}

fn collapse_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn safe_fragment(value: &str) -> Option<&str> {
    (!value.is_empty()
        && value.len() <= 256
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | ':')
        }))
    .then_some(value)
}

fn read_zip_archive(bytes: &[u8], label: &str) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
    if bytes.len() as u64 > MAX_RENDITION_BYTES {
        bail!("{label} archive exceeds the rendition limit");
    }
    let mut archive =
        ZipArchive::new(Cursor::new(bytes)).with_context(|| format!("opening {label} archive"))?;
    if archive.len() > MAX_ARCHIVE_ENTRIES {
        bail!("{label} archive exceeds the entry limit");
    }
    let mut expanded = 0_u64;
    let mut files = BTreeMap::new();
    for index in 0..archive.len() {
        let mut file = archive
            .by_index(index)
            .with_context(|| format!("reading {label} archive entry {index}"))?;
        let raw_name = file.name().to_owned();
        let path = safe_archive_path(&raw_name)
            .with_context(|| format!("validating {label} archive path {raw_name}"))?;
        if file.is_dir() {
            continue;
        }
        if file.size() > MAX_ARCHIVE_MEMBER_BYTES {
            bail!(
                "{label} archive member {} exceeds the member limit",
                path.display()
            );
        }
        expanded = expanded
            .checked_add(file.size())
            .ok_or_else(|| anyhow!("{label} expanded size overflow"))?;
        if expanded > MAX_ARCHIVE_EXPANDED_BYTES {
            bail!("{label} archive exceeds the expanded size limit");
        }
        let mut content = Vec::with_capacity(file.size() as usize);
        file.by_ref()
            .take(MAX_ARCHIVE_MEMBER_BYTES + 1)
            .read_to_end(&mut content)
            .with_context(|| format!("reading {label} archive member {}", path.display()))?;
        if content.len() as u64 > MAX_ARCHIVE_MEMBER_BYTES {
            bail!(
                "{label} archive member {} exceeds the member limit",
                path.display()
            );
        }
        if files.insert(path.clone(), content).is_some() {
            bail!("{label} archive contains duplicate path {}", path.display());
        }
    }
    Ok(files)
}

fn archive_text(archive: &BTreeMap<PathBuf, Vec<u8>>, path: &Path, label: &str) -> Result<String> {
    let bytes = archive
        .get(path)
        .ok_or_else(|| anyhow!("{label} {} is absent", path.display()))?;
    let text = std::str::from_utf8(bytes)
        .with_context(|| format!("{label} {} is not UTF-8", path.display()))?;
    Ok(text.trim_start_matches('\u{feff}').to_owned())
}

fn safe_archive_path(value: &str) -> Result<PathBuf> {
    if value.is_empty()
        || value.as_bytes().contains(&0)
        || value.contains('\\')
        || value.starts_with('/')
    {
        bail!("unsafe archive path `{value}`");
    }
    let decoded = percent_decode(value)?;
    if decoded.as_bytes().contains(&0)
        || decoded.contains('\\')
        || decoded.starts_with('/')
        || decoded.contains(':')
    {
        bail!("unsafe archive path `{value}`");
    }
    let path = Path::new(&decoded);
    let mut safe = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(component) if !component.is_empty() => safe.push(component),
            Component::CurDir => {}
            _ => bail!("unsafe archive path `{value}`"),
        }
    }
    if safe.as_os_str().is_empty() {
        bail!("empty archive path");
    }
    Ok(safe)
}

fn resolve_archive_reference(base_file: &Path, reference: &str) -> Result<PathBuf> {
    let reference = reference.split(['#', '?']).next().unwrap_or_default();
    if reference.is_empty() {
        return Ok(base_file.to_path_buf());
    }
    if reference.contains('\\') || reference.starts_with('/') || Url::parse(reference).is_ok() {
        bail!("unsafe archive reference `{reference}`");
    }
    let decoded = percent_decode(reference)?;
    let mut components = base_file
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_os_string()),
            _ => None,
        })
        .collect::<Vec<_>>();
    for segment in decoded.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if components.pop().is_none() {
                    bail!("archive reference escapes its root");
                }
            }
            value => {
                if value.as_bytes().contains(&0) || value.contains([':', '\\']) {
                    bail!("unsafe archive reference segment");
                }
                components.push(value.into());
            }
        }
    }
    if components.is_empty() {
        bail!("archive reference resolves to an empty path");
    }
    Ok(components.into_iter().collect())
}

fn percent_decode(value: &str) -> Result<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                bail!("invalid percent escape in archive path");
            }
            let high = hex_value(bytes[index + 1])?;
            let low = hex_value(bytes[index + 2])?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).context("archive path is not UTF-8")
}

fn hex_value(value: u8) -> Result<u8> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        b'A'..=b'F' => Ok(value - b'A' + 10),
        _ => bail!("invalid hexadecimal digit"),
    }
}

#[derive(Clone, Debug)]
struct XmlNode {
    name: String,
    attributes: BTreeMap<String, String>,
    children: Vec<XmlChild>,
}

#[derive(Clone, Debug)]
enum XmlChild {
    Node(XmlNode),
    Text(String),
}

fn parse_xml(source: &str) -> Result<XmlNode> {
    let source = source.trim_start_matches('\u{feff}');
    let mut stack = vec![XmlNode {
        name: "__root__".to_owned(),
        attributes: BTreeMap::new(),
        children: Vec::new(),
    }];
    let mut nodes = 1_usize;
    let mut position = 0_usize;
    while position < source.len() {
        let Some(relative) = source[position..].find('<') else {
            push_xml_text(&mut stack, &source[position..])?;
            break;
        };
        let start = position + relative;
        push_xml_text(&mut stack, &source[position..start])?;
        if source[start..].starts_with("<!--") {
            let end = source[start + 4..]
                .find("-->")
                .ok_or_else(|| anyhow!("unterminated XML comment"))?
                + start
                + 7;
            position = end;
            continue;
        }
        if source[start..].starts_with("<![CDATA[") {
            let content_start = start + 9;
            let content_end = source[content_start..]
                .find("]]>")
                .ok_or_else(|| anyhow!("unterminated XML CDATA"))?
                + content_start;
            stack
                .last_mut()
                .ok_or_else(|| anyhow!("XML parser lost its document root"))?
                .children
                .push(XmlChild::Text(
                    source[content_start..content_end].to_owned(),
                ));
            position = content_end + 3;
            continue;
        }
        if source[start..].starts_with("<?") {
            let end = source[start + 2..]
                .find("?>")
                .ok_or_else(|| anyhow!("unterminated XML processing instruction"))?
                + start
                + 4;
            position = end;
            continue;
        }
        if source[start..].starts_with("<!") {
            let end = find_xml_tag_end(source, start + 2)?;
            position = end + 1;
            continue;
        }
        let end = find_xml_tag_end(source, start + 1)?;
        let body = source[start + 1..end].trim();
        if let Some(close_name) = body.strip_prefix('/') {
            let close_name = close_name.trim();
            if close_name.is_empty() || close_name.chars().any(char::is_whitespace) {
                bail!("invalid XML closing tag");
            }
            if stack.len() <= 1 {
                bail!("unexpected XML closing tag {close_name}");
            }
            let node = stack
                .pop()
                .ok_or_else(|| anyhow!("XML parser lost its closing element"))?;
            if node.name != close_name {
                bail!("mismatched XML closing tag {close_name} for {}", node.name);
            }
            stack
                .last_mut()
                .ok_or_else(|| anyhow!("XML parser lost its parent element"))?
                .children
                .push(XmlChild::Node(node));
        } else {
            let self_closing = body.ends_with('/');
            let body = body.strip_suffix('/').unwrap_or(body).trim_end();
            let (name, attributes) = parse_xml_start_tag(body)?;
            nodes += 1;
            if nodes > MAX_XML_NODES {
                bail!("XML document exceeds the node limit");
            }
            let node = XmlNode {
                name,
                attributes,
                children: Vec::new(),
            };
            if self_closing {
                stack
                    .last_mut()
                    .ok_or_else(|| anyhow!("XML parser lost its parent element"))?
                    .children
                    .push(XmlChild::Node(node));
            } else {
                if stack.len() >= MAX_XML_DEPTH {
                    bail!("XML document exceeds the depth limit");
                }
                stack.push(node);
            }
        }
        position = end + 1;
    }
    if stack.len() != 1 {
        let element = stack
            .last()
            .map(|node| node.name.as_str())
            .unwrap_or("unknown");
        bail!("unclosed XML element {element}");
    }
    let root = stack
        .pop()
        .ok_or_else(|| anyhow!("XML document lost its root"))?;
    let mut elements = root.children.into_iter().filter_map(|child| match child {
        XmlChild::Node(node) => Some(node),
        XmlChild::Text(text) if text.trim().is_empty() => None,
        XmlChild::Text(_) => None,
    });
    let document = elements
        .next()
        .ok_or_else(|| anyhow!("XML document has no root element"))?;
    if elements.next().is_some() {
        bail!("XML document has multiple root elements");
    }
    Ok(document)
}

fn push_xml_text(stack: &mut [XmlNode], text: &str) -> Result<()> {
    if text.is_empty() {
        return Ok(());
    }
    let decoded = decode_xml_entities(text)?;
    if !decoded.is_empty() {
        stack
            .last_mut()
            .ok_or_else(|| anyhow!("XML parser lost its document root"))?
            .children
            .push(XmlChild::Text(decoded));
    }
    Ok(())
}

fn find_xml_tag_end(source: &str, mut position: usize) -> Result<usize> {
    let bytes = source.as_bytes();
    let mut quote = None;
    while position < bytes.len() {
        match (bytes[position], quote) {
            (b'\'' | b'"', None) => quote = Some(bytes[position]),
            (value, Some(expected)) if value == expected => quote = None,
            (b'>', None) => return Ok(position),
            _ => {}
        }
        position += 1;
    }
    bail!("unterminated XML tag")
}

fn parse_xml_start_tag(body: &str) -> Result<(String, BTreeMap<String, String>)> {
    let bytes = body.as_bytes();
    let mut position = 0;
    skip_ascii_whitespace(bytes, &mut position);
    let name_start = position;
    while position < bytes.len() && is_xml_name_byte(bytes[position]) {
        position += 1;
    }
    if position == name_start {
        bail!("XML start tag has no name");
    }
    let name = body[name_start..position].to_owned();
    let mut attributes = BTreeMap::new();
    loop {
        skip_ascii_whitespace(bytes, &mut position);
        if position == bytes.len() {
            break;
        }
        let attribute_start = position;
        while position < bytes.len() && is_xml_name_byte(bytes[position]) {
            position += 1;
        }
        if position == attribute_start {
            bail!("invalid XML attribute in {name}");
        }
        let attribute = body[attribute_start..position].to_owned();
        skip_ascii_whitespace(bytes, &mut position);
        if bytes.get(position) != Some(&b'=') {
            bail!("XML attribute {attribute} has no value");
        }
        position += 1;
        skip_ascii_whitespace(bytes, &mut position);
        let quote = *bytes
            .get(position)
            .filter(|value| matches!(value, b'\'' | b'"'))
            .ok_or_else(|| anyhow!("XML attribute {attribute} is not quoted"))?;
        position += 1;
        let value_start = position;
        while position < bytes.len() && bytes[position] != quote {
            position += 1;
        }
        if position == bytes.len() {
            bail!("unterminated XML attribute {attribute}");
        }
        let value = decode_xml_entities(&body[value_start..position])?;
        position += 1;
        if attributes.insert(attribute.clone(), value).is_some() {
            bail!("duplicate XML attribute {attribute}");
        }
    }
    Ok((name, attributes))
}

fn skip_ascii_whitespace(bytes: &[u8], position: &mut usize) {
    while bytes.get(*position).is_some_and(u8::is_ascii_whitespace) {
        *position += 1;
    }
}

fn is_xml_name_byte(value: u8) -> bool {
    value.is_ascii_alphanumeric() || matches!(value, b':' | b'_' | b'-' | b'.')
}

fn decode_xml_entities(value: &str) -> Result<String> {
    let mut output = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(index) = rest.find('&') {
        output.push_str(&rest[..index]);
        let entity_start = index + 1;
        let entity_end = rest[entity_start..]
            .find(';')
            .ok_or_else(|| anyhow!("unterminated XML entity"))?
            + entity_start;
        let entity = &rest[entity_start..entity_end];
        let character = match entity {
            "amp" => '&',
            "lt" => '<',
            "gt" => '>',
            "quot" => '"',
            "apos" => '\'',
            "nbsp" => '\u{00a0}',
            "ndash" => '\u{2013}',
            "mdash" => '\u{2014}',
            "lsquo" => '\u{2018}',
            "rsquo" => '\u{2019}',
            "ldquo" => '\u{201c}',
            "rdquo" => '\u{201d}',
            _ if entity.starts_with("#x") || entity.starts_with("#X") => {
                let code = u32::from_str_radix(&entity[2..], 16)
                    .context("invalid hexadecimal XML entity")?;
                char::from_u32(code).ok_or_else(|| anyhow!("invalid XML scalar entity"))?
            }
            _ if entity.starts_with('#') => {
                let code = entity[1..]
                    .parse::<u32>()
                    .context("invalid decimal XML entity")?;
                char::from_u32(code).ok_or_else(|| anyhow!("invalid XML scalar entity"))?
            }
            _ => bail!("unsupported XML entity &{entity};"),
        };
        if !character.is_control() || character.is_ascii_whitespace() {
            output.push(character);
        }
        rest = &rest[entity_end + 1..];
    }
    output.push_str(rest);
    Ok(output)
}

fn local_name(name: &str) -> String {
    name.rsplit(':').next().unwrap_or(name).to_ascii_lowercase()
}

fn attr_local<'a>(node: &'a XmlNode, name: &str) -> Option<&'a str> {
    node.attributes
        .iter()
        .find(|(attribute, _)| local_name(attribute) == name.to_ascii_lowercase())
        .map(|(_, value)| value.as_str())
}

fn descendants(root: &XmlNode) -> Descendants<'_> {
    Descendants { stack: vec![root] }
}

struct Descendants<'a> {
    stack: Vec<&'a XmlNode>,
}

impl<'a> Iterator for Descendants<'a> {
    type Item = &'a XmlNode;

    fn next(&mut self) -> Option<Self::Item> {
        let node = self.stack.pop()?;
        for child in node.children.iter().rev() {
            if let XmlChild::Node(child) = child {
                self.stack.push(child);
            }
        }
        Some(node)
    }
}

fn child_nodes(node: &XmlNode) -> impl Iterator<Item = &XmlNode> {
    node.children.iter().filter_map(|child| match child {
        XmlChild::Node(node) => Some(node),
        XmlChild::Text(_) => None,
    })
}

fn has_child(node: &XmlNode, name: &str) -> bool {
    child_nodes(node).any(|child| local_name(&child.name) == name)
}

fn ensure_real_directory(path: &Path, label: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("reading {label} metadata {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("{label} must be a real directory: {}", path.display());
    }
    Ok(())
}

fn confined_path(root: &Path, relative: &Path) -> Result<PathBuf> {
    if !root.is_absolute() {
        bail!("FRL path root must be absolute: {}", root.display());
    }
    if relative.as_os_str().is_empty() || relative.is_absolute() {
        bail!("FRL relative path is invalid: {}", relative.display());
    }
    for component in relative.components() {
        if !matches!(component, Component::Normal(_)) {
            bail!("FRL path escapes its root: {}", relative.display());
        }
    }
    let path = root.join(relative);
    if !path.starts_with(root) {
        bail!("FRL path escapes its root: {}", relative.display());
    }
    Ok(path)
}

fn prepare_confined_parent(root: &Path, relative: &Path) -> Result<PathBuf> {
    ensure_real_directory(root, "FRL path root")?;
    let path = confined_path(root, relative)?;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("FRL path has no parent"))?;
    let relative_parent = parent
        .strip_prefix(root)
        .map_err(|_| anyhow!("FRL path parent escapes its root"))?;
    let mut current = root.to_path_buf();
    for component in relative_parent.components() {
        let Component::Normal(component) = component else {
            bail!("FRL path parent is not confined");
        };
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                bail!(
                    "FRL path component is not a real directory: {}",
                    current.display()
                )
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if let Err(create_error) = fs::create_dir(&current) {
                    if create_error.kind() != std::io::ErrorKind::AlreadyExists {
                        return Err(create_error).with_context(|| {
                            format!("creating FRL directory {}", current.display())
                        });
                    }
                    let metadata = fs::symlink_metadata(&current)?;
                    if metadata.file_type().is_symlink() || !metadata.is_dir() {
                        bail!(
                            "FRL path component is not a real directory: {}",
                            current.display()
                        );
                    }
                }
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("reading FRL directory {}", current.display()))
            }
        }
    }
    ensure_canonical_parent_within(root, &path)?;
    if let Ok(metadata) = fs::symlink_metadata(&path) {
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!("FRL destination is not a real file: {}", path.display());
        }
    }
    Ok(path)
}

fn ensure_canonical_parent_within(root: &Path, path: &Path) -> Result<()> {
    let canonical_root = fs::canonicalize(root)
        .with_context(|| format!("canonicalizing FRL root {}", root.display()))?;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("FRL confined path has no parent"))?;
    let canonical_parent = fs::canonicalize(parent)
        .with_context(|| format!("canonicalizing FRL parent {}", parent.display()))?;
    if !canonical_parent.starts_with(&canonical_root) {
        bail!("FRL path parent escapes its canonical root");
    }
    Ok(())
}

fn ensure_existing_path_within(root: &Path, path: &Path) -> Result<()> {
    let canonical_root = fs::canonicalize(root)
        .with_context(|| format!("canonicalizing FRL root {}", root.display()))?;
    let canonical_path = fs::canonicalize(path)
        .with_context(|| format!("canonicalizing FRL path {}", path.display()))?;
    if !canonical_path.starts_with(&canonical_root) {
        bail!("FRL path escapes its canonical root");
    }
    Ok(())
}

fn atomic_write_confined(root: &Path, relative: &Path, bytes: &[u8]) -> Result<()> {
    let destination = prepare_confined_parent(root, relative)?;
    crate::config::atomic_write(&destination, bytes)
        .with_context(|| format!("writing confined FRL file {}", destination.display()))
}

fn write_immutable_confined(root: &Path, relative: &Path, bytes: &[u8]) -> Result<()> {
    let destination = prepare_confined_parent(root, relative)?;
    match fs::symlink_metadata(&destination) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                bail!("FRL immutable destination is not a real file");
            }
            let existing = read_bounded_file(&destination, bytes.len() as u64 + 1)?;
            if existing != bytes {
                bail!(
                    "FRL immutable content address collision at {}",
                    destination.display()
                );
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            atomic_write_confined(root, relative, bytes)
        }
        Err(error) => {
            Err(error).with_context(|| format!("reading FRL destination {}", destination.display()))
        }
    }
}

fn read_bounded_file(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let expected = fs::symlink_metadata(path)
        .with_context(|| format!("reading {} metadata", path.display()))?;
    if expected.file_type().is_symlink() || !expected.is_file() {
        bail!("{} must be a real file", path.display());
    }
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("reading {} metadata", path.display()))?;
    ensure_same_open_file(&expected, &metadata, path)?;
    if metadata.len() > limit {
        bail!("{} exceeds the {limit}-byte limit", path.display());
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading {}", path.display()))?;
    if bytes.len() as u64 > limit {
        bail!("{} exceeds the {limit}-byte limit", path.display());
    }
    Ok(bytes)
}

#[cfg(unix)]
fn ensure_same_open_file(
    expected: &fs::Metadata,
    opened: &fs::Metadata,
    path: &Path,
) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    if expected.dev() != opened.dev() || expected.ino() != opened.ino() {
        bail!("{} changed while it was being opened", path.display());
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_same_open_file(
    _expected: &fs::Metadata,
    _opened: &fs::Metadata,
    _path: &Path,
) -> Result<()> {
    Ok(())
}

fn path_to_slashes(path: &Path) -> Result<String> {
    let mut parts = Vec::new();
    for component in path.components() {
        let Component::Normal(component) = component else {
            bail!("FRL stored path is not relative");
        };
        parts.push(
            component
                .to_str()
                .ok_or_else(|| anyhow!("FRL stored path is not UTF-8"))?,
        );
    }
    Ok(parts.join("/"))
}

fn is_lower_hex_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use tempfile::tempdir;

    struct FixtureEmbeddings;

    impl crate::pipeline::EmbeddingProvider for FixtureEmbeddings {
        fn model_id(&self) -> &str {
            crate::EMBEDDING_MODEL_ID
        }

        fn count_tokens(&self, text: &str) -> Result<usize> {
            Ok(text.split_whitespace().count().max(1))
        }

        fn encode(&self, texts: &[String]) -> Result<Vec<[i8; crate::EMBEDDING_DIM]>> {
            Ok(texts
                .iter()
                .map(|text| {
                    let mut vector = [0_i8; crate::EMBEDDING_DIM];
                    let digest = Sha256::digest(text.as_bytes());
                    for (index, value) in vector.iter_mut().enumerate() {
                        *value = digest[index % digest.len()] as i8;
                    }
                    vector
                })
                .collect())
        }
    }

    const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/frl");

    #[derive(Default)]
    struct FakeApi {
        title_upper_bound: Option<String>,
        title_pages: Mutex<VecDeque<Vec<FrlTitle>>>,
        version_pages: Mutex<VecDeque<Vec<FrlVersion>>>,
        authoritative_versions: BTreeMap<String, FrlVersion>,
        document_pages: Mutex<BTreeMap<String, VecDeque<Vec<FrlRendition>>>>,
        payloads: BTreeMap<String, std::result::Result<FrlPayload, String>>,
        title_queries: Mutex<Vec<(String, Option<String>, usize)>>,
        version_queries: Mutex<Vec<VersionPageQuery>>,
        fetch_delay: Duration,
        active_fetches: AtomicUsize,
        max_active_fetches: AtomicUsize,
    }

    impl FakeApi {
        fn with_title_pages(mut self, pages: Vec<Vec<FrlTitle>>) -> Self {
            self.title_upper_bound = pages.iter().flatten().map(|title| title.id.clone()).max();
            self.title_pages = Mutex::new(pages.into());
            self
        }

        fn with_version_pages(mut self, pages: Vec<Vec<FrlVersion>>) -> Self {
            self.version_pages = Mutex::new(pages.into());
            self
        }

        fn with_authoritative_version(mut self, version: FrlVersion) -> Self {
            self.authoritative_versions
                .insert(version.title_id.clone(), version);
            self
        }

        fn with_document_pages(
            mut self,
            title_id: &str,
            pages: Vec<Vec<FrlRendition>>,
        ) -> Result<Self> {
            self.document_pages
                .get_mut()
                .map_err(|_| anyhow!("fixture document-page lock is poisoned"))
                .map(|documents| documents.insert(title_id.to_owned(), pages.into()))?;
            Ok(self)
        }

        fn with_payload(
            mut self,
            format: &str,
            payload: std::result::Result<FrlPayload, String>,
        ) -> Self {
            self.payloads.insert(format.to_owned(), payload);
            self
        }

        fn with_fetch_delay(mut self, delay: Duration) -> Self {
            self.fetch_delay = delay;
            self
        }
    }

    impl FrlApi for FakeApi {
        fn title_upper_bound(&self) -> Result<Option<String>> {
            Ok(self.title_upper_bound.clone())
        }

        fn titles_page(
            &self,
            upper_bound: &str,
            after: Option<&str>,
            top: usize,
        ) -> Result<Vec<FrlTitle>> {
            self.title_queries
                .lock()
                .map_err(|_| anyhow!("fixture title-query lock is poisoned"))?
                .push((upper_bound.to_owned(), after.map(str::to_owned), top));
            Ok(self
                .title_pages
                .lock()
                .map_err(|_| anyhow!("fixture title-page lock is poisoned"))?
                .pop_front()
                .unwrap_or_default())
        }

        fn versions_page(&self, query: &VersionPageQuery) -> Result<Vec<FrlVersion>> {
            self.version_queries
                .lock()
                .map_err(|_| anyhow!("fixture version-query lock is poisoned"))?
                .push(query.clone());
            Ok(self
                .version_pages
                .lock()
                .map_err(|_| anyhow!("fixture version-page lock is poisoned"))?
                .pop_front()
                .unwrap_or_default())
        }

        fn authoritative_version(
            &self,
            title_id: &str,
            _upper_bound: &str,
        ) -> Result<Option<FrlVersion>> {
            Ok(self.authoritative_versions.get(title_id).cloned())
        }

        fn documents_page(
            &self,
            version: &FrlVersionKey,
            _after: Option<&RenditionKey>,
            _top: usize,
        ) -> Result<Vec<FrlRendition>> {
            let mut pages = self
                .document_pages
                .lock()
                .map_err(|_| anyhow!("fixture document-page lock is poisoned"))?;
            Ok(pages
                .get_mut(&version.title_id)
                .and_then(VecDeque::pop_front)
                .unwrap_or_default())
        }

        fn fetch_rendition(&self, rendition: &FrlRendition) -> Result<FrlPayload> {
            let active = self.active_fetches.fetch_add(1, AtomicOrdering::SeqCst) + 1;
            self.max_active_fetches
                .fetch_max(active, AtomicOrdering::SeqCst);
            thread::sleep(self.fetch_delay);
            let result = match self.payloads.get(&rendition.format).cloned() {
                Some(Ok(payload)) => Ok(payload),
                Some(Err(message)) => Err(anyhow!(message)),
                None => Err(anyhow!("fixture has no payload for {}", rendition.format)),
            };
            self.active_fetches.fetch_sub(1, AtomicOrdering::SeqCst);
            result
        }
    }

    fn fixture_json<T: DeserializeOwned>(name: &str) -> Result<T> {
        let path = Path::new(FIXTURES).join(name);
        let bytes =
            fs::read(&path).with_context(|| format!("reading FRL fixture {}", path.display()))?;
        serde_json::from_slice(&bytes)
            .with_context(|| format!("decoding FRL fixture {}", path.display()))
    }

    #[derive(Deserialize)]
    struct EnumFixture {
        title: FrlTitle,
        rendition: FrlRendition,
    }

    fn title(id: &str, name: &str) -> FrlTitle {
        FrlTitle {
            id: id.to_owned(),
            name: Some(name.to_owned()),
            making_date: Some("2024-01-01T00:00:00".to_owned()),
            collection: "Act".to_owned(),
            sub_collection: None,
            is_principal: true,
            is_in_force: true,
            status: "InForce".to_owned(),
        }
    }

    fn cursor(value: &str, title_id: &str) -> Result<FrlCursor> {
        Ok(FrlCursor {
            registered_at: canonical_datetime(value)?,
            title_id: title_id.to_owned(),
            start: canonical_datetime("2024-01-01T00:00:00")?,
            retrospective_start: canonical_datetime("2024-01-01T00:00:00")?,
        })
    }

    fn version(title_id: &str, registered_at: &str) -> FrlVersion {
        FrlVersion {
            title_id: title_id.to_owned(),
            start: "2024-01-01T00:00:00".to_owned(),
            retrospective_start: "2024-01-01T00:00:00".to_owned(),
            end: None,
            retrospective_end: None,
            is_current: true,
            is_latest: true,
            name: Some(format!("Title {title_id}")),
            status: "InForce".to_owned(),
            register_id: Some(format!("F{title_id}")),
            registered_at: Some(registered_at.to_owned()),
            compilation_number: Some("1".to_owned()),
        }
    }

    fn rendition(title_id: &str, format: &str, extension: &str) -> Result<FrlRendition> {
        Ok(FrlRendition {
            title_id: title_id.to_owned(),
            start: canonical_datetime("2024-01-01T00:00:00")?,
            retrospective_start: canonical_datetime("2024-01-01T00:00:00")?,
            rectification_version_number: 0,
            document_type: "Primary".to_owned(),
            unique_type_number: 0,
            volume_number: 0,
            format: format.to_owned(),
            compilation_number: Some("1".to_owned()),
            register_id: Some(format!("F{title_id}")),
            version_type: Some("Compilation".to_owned()),
            extension: Some(extension.to_owned()),
            mime_type: None,
            file_name: None,
            bytes: None,
            page_count: None,
            size_in_bytes: None,
            is_authorised: true,
            name: None,
            contents: None,
        })
    }

    #[test]
    fn descriptor_and_policy_are_stable() -> Result<()> {
        let descriptor = frl_descriptor()?;
        assert_eq!(descriptor.id.as_str(), FRL_SOURCE_ID);
        assert_eq!(descriptor.display_name, FRL_DISPLAY_NAME);
        assert_eq!(PAGE_SIZE, 100);
        assert_eq!(
            FRL_ACQUISITION.rate_policy(),
            SourceRatePolicy {
                minimum_request_interval_ms: 0,
                request_timeout_seconds: 30,
            }
        );
        Ok(())
    }

    #[test]
    fn public_rendition_url_is_bound_to_the_exact_authoritative_version() -> Result<()> {
        let api = HttpFrlApi::new(FRL_ACQUISITION.rate_policy())?;
        let mut candidate = rendition("F2019L00134", "Epub", ".epub")?;
        candidate.start = canonical_datetime("2026-03-25T13:50:00")?;
        candidate.retrospective_start = candidate.start.clone();
        let url = api.public_rendition_url(&candidate, RenditionKind::Epub)?;
        assert_eq!(
            url.as_str(),
            "https://www.legislation.gov.au/F2019L00134/2026-03-25T13:50:00.000/2026-03-25T13:50:00.000/text/original/epub"
        );
        validate_public_rendition_url(&url)?;

        candidate.start = canonical_datetime("2026-03-25T13:50:00.0000001")?;
        assert!(api
            .public_rendition_url(&candidate, RenditionKind::Epub)
            .is_err());
        assert!(validate_public_rendition_url(&Url::parse(
            "https://www.legislation.gov.au/F2019L00134/latest/text"
        )?)
        .is_err());
        assert!(validate_public_rendition_url(&Url::parse(
            "https://www.legislation.gov.au/F2019L00134/2026-03-25T13:50:00.000/2026-03-25T13:50:00.000/text/original/epub?version=older"
        )?)
        .is_err());
        Ok(())
    }

    #[test]
    #[ignore = "requires the live official FRL API and public rendition host"]
    fn exact_public_epub_recovers_an_unavailable_api_entity() -> Result<()> {
        let api = HttpFrlApi::new(FRL_ACQUISITION.rate_policy())?;
        let mut candidate = rendition("F2019L00134", "Epub", ".epub")?;
        candidate.start = canonical_datetime("2026-03-25T13:50:00")?;
        candidate.retrospective_start = candidate.start.clone();
        candidate.compilation_number = Some("5".to_owned());
        candidate.register_id = Some("F2026C00250".to_owned());
        candidate.version_type = Some("Rectification".to_owned());
        candidate.is_authorised = false;
        let FrlPayload::Epub(bytes) = api.fetch_rendition(&candidate)? else {
            bail!("exact FRL public EPUB fallback returned the wrong payload kind");
        };
        assert!(bytes.starts_with(b"PK"));
        let (html, _) = normalize_epub(&bytes, "F2019L00134")?;
        assert!(html.contains("CASA 09/19"));
        Ok(())
    }

    #[test]
    fn saved_discovery_plan_is_validated_before_reuse() -> Result<()> {
        let workspace = tempdir()?;
        let run_dir = tempdir()?;
        let selected_version = version("A0001", "2024-01-16T00:00:00");
        let envelope = FrlDiscoveryEnvelope {
            schema_version: DISCOVERY_SCHEMA_VERSION,
            source: FRL_SOURCE_ID.to_owned(),
            mode: SourceUpdateMode::Full,
            plan: FrlDiscoveryPlan {
                authoritative_titles: vec![title("A0001", "One")],
                versions: vec![selected_version.clone()],
                proposed_cursor: FrlCursor::from_version(&selected_version)?,
            },
        };
        fs::write(
            run_dir.path().join(DISCOVERY_FILE_NAME),
            serde_json::to_vec(&envelope)?,
        )?;
        let discovery = discover_to_run_dir(
            &FakeApi::default(),
            workspace.path(),
            run_dir.path(),
            SourceUpdateMode::Full,
            Utc::now().naive_utc(),
            10,
        )?;
        assert_eq!(discovery.records, 1);
        let error = discover_to_run_dir(
            &FakeApi::default(),
            workspace.path(),
            run_dir.path(),
            SourceUpdateMode::Incremental,
            Utc::now().naive_utc(),
            10,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("mode does not match"));
        Ok(())
    }

    #[test]
    fn authoritative_title_paging_is_ordered_and_deduplicated() -> Result<()> {
        let pages: Vec<Vec<FrlTitle>> = fixture_json("titles-pages.json")?;
        let api = FakeApi::default().with_title_pages(vec![
            pages[0].clone(),
            pages[1].clone(),
            Vec::new(),
        ]);
        let titles = scan_titles(&api, 2)?;
        assert_eq!(
            titles
                .iter()
                .map(|title| title.id.as_str())
                .collect::<Vec<_>>(),
            ["A0001", "A0002", "A0003"]
        );
        let queries = api
            .title_queries
            .lock()
            .map_err(|_| anyhow!("fixture title-query lock is poisoned"))?;
        assert_eq!(queries[0], ("A0003".to_owned(), None, 2));
        assert_eq!(queries[1].1.as_deref(), Some("A0002"));
        assert_eq!(queries[2].1.as_deref(), Some("A0003"));
        Ok(())
    }

    #[test]
    fn authoritative_title_inventory_contains_only_in_force_titles() -> Result<()> {
        let current = title("A0001", "Current Act");
        let mut repealed = title("A0002", "Repealed Act");
        repealed.is_in_force = false;
        repealed.status = "Repealed".to_string();
        let api = FakeApi::default().with_title_pages(vec![vec![current.clone(), repealed]]);
        let titles = scan_titles(&api, 2)?;
        assert_eq!(titles, vec![current]);
        Ok(())
    }

    #[test]
    fn authoritative_title_scan_rejects_an_empty_upstream_snapshot() {
        let error = scan_titles(&FakeApi::default(), PAGE_SIZE)
            .expect_err("an empty authoritative response must not delete the source");
        assert!(error.to_string().contains("no upper boundary"));
    }

    #[test]
    fn version_paging_keeps_registration_ties_across_pages() -> Result<()> {
        let pages: Vec<Vec<FrlVersion>> = fixture_json("versions-tied-pages.json")?;
        let api = FakeApi::default().with_version_pages(vec![
            pages[0].clone(),
            pages[1].clone(),
            Vec::new(),
        ]);
        let scan = scan_versions(&api, None, parse_datetime("2024-02-01T00:00:00")?, 2)?;
        assert_eq!(scan.versions.len(), 3);
        assert_eq!(
            scan.proposed_cursor
                .as_ref()
                .map(|cursor| cursor.title_id.as_str()),
            Some("A0003")
        );
        let queries = api
            .version_queries
            .lock()
            .map_err(|_| anyhow!("fixture version-query lock is poisoned"))?;
        assert!(queries[0].after.is_none());
        assert_eq!(
            queries[1]
                .after
                .as_ref()
                .map(|cursor| cursor.title_id.as_str()),
            Some("A0002")
        );
        assert_eq!(queries[0].upper_bound, queries[1].upper_bound);
        Ok(())
    }

    #[test]
    fn overlap_boundary_is_inclusive_and_client_bounded() -> Result<()> {
        let rows: Vec<FrlVersion> = fixture_json("versions-overlap.json")?;
        let api = FakeApi::default().with_version_pages(vec![rows]);
        let previous = cursor("2024-01-15T00:00:00", "A0001")?;
        let scan = scan_versions(
            &api,
            Some(&previous),
            parse_datetime("2024-01-20T00:00:00")?,
            10,
        )?;
        assert_eq!(
            scan.versions
                .iter()
                .map(|version| version.title_id.as_str())
                .collect::<Vec<_>>(),
            ["A0002", "A0003"]
        );
        let queries = api
            .version_queries
            .lock()
            .map_err(|_| anyhow!("fixture version-query lock is poisoned"))?;
        assert_eq!(
            queries
                .first()
                .and_then(|query| query.lower_bound.as_deref()),
            Some("2024-01-08T00:00:00.0000000")
        );
        Ok(())
    }

    #[test]
    fn overlap_and_page_duplicates_are_idempotent() -> Result<()> {
        let pages: Vec<Vec<FrlVersion>> = fixture_json("versions-tied-pages.json")?;
        let api = FakeApi::default().with_version_pages(vec![
            pages[0].clone(),
            pages[1].clone(),
            Vec::new(),
        ]);
        let scan = scan_versions(&api, None, parse_datetime("2024-02-01T00:00:00")?, 2)?;
        let keys = scan
            .versions
            .iter()
            .map(FrlVersionKey::from_version)
            .collect::<Result<BTreeSet<_>>>()?;
        assert_eq!(keys.len(), scan.versions.len());
        Ok(())
    }

    #[test]
    fn historical_overlap_resolves_the_authoritative_title_version() -> Result<()> {
        let mut historical = version("A0001", "2024-01-16T00:00:00");
        historical.is_current = false;
        historical.is_latest = true;
        historical.start = "2023-01-01T00:00:00".to_owned();
        historical.retrospective_start = historical.start.clone();

        let current = version("A0001", "2024-01-10T00:00:00");
        let api = FakeApi::default().with_authoritative_version(current.clone());
        let resolved = resolve_authoritative_versions(
            &api,
            vec![historical],
            false,
            "2024-02-01T00:00:00.0000000",
        )?;
        assert_eq!(resolved.len(), 1);
        assert_eq!(
            FrlVersionKey::from_version(&resolved[0])?,
            FrlVersionKey::from_version(&current)?
        );
        Ok(())
    }

    #[test]
    fn official_enum_ordinals_normalize_to_contract_names() -> Result<()> {
        let fixture: EnumFixture = fixture_json("enum-ordinals.json")?;
        assert_eq!(fixture.title.collection, "LegislativeInstrument");
        assert_eq!(fixture.title.sub_collection.as_deref(), Some("Regulations"));
        assert_eq!(fixture.title.status, "Repealed");
        assert_eq!(fixture.rendition.document_type, "Primary");
        assert_eq!(fixture.rendition.format, "Epub");
        assert_eq!(
            fixture.rendition.version_type.as_deref(),
            Some("Replacement")
        );
        let word = RenditionKey::from_rendition(&rendition("A0001", "Word", ".docx")?)?;
        let pdf = RenditionKey::from_rendition(&rendition("A0001", "Pdf", ".pdf")?)?;
        let epub = RenditionKey::from_rendition(&rendition("A0001", "Epub", ".epub")?)?;
        assert!(word < pdf && pdf < epub);
        Ok(())
    }

    #[test]
    fn authoritative_reconciliation_deletes_absent_inventory_records() -> Result<()> {
        let workspace = tempdir()?;
        let old_cursor = cursor("2024-01-15T00:00:00", "A0002")?;
        let mut inventory = BTreeMap::new();
        for id in ["A0001", "A0002"] {
            inventory.insert(
                id.to_owned(),
                FrlInventoryEntry {
                    native_id: id.to_owned(),
                    upstream_version: FrlVersionKey {
                        title_id: id.to_owned(),
                        start: canonical_datetime("2024-01-01T00:00:00")?,
                        retrospective_start: canonical_datetime("2024-01-01T00:00:00")?,
                    },
                    register_id: Some(format!("F{id}")),
                    canonical_url: format!("{FRL_PUBLIC_BASE}{id}/latest/text"),
                    payload_hash: "a".repeat(64),
                    last_successful_cursor: old_cursor.clone(),
                },
            );
        }
        commit_state(
            workspace.path(),
            &FrlState {
                schema_version: STATE_SCHEMA_VERSION,
                cursor: Some(old_cursor.clone()),
                inventory,
            },
        )?;
        let report = fetch_plan(
            &FakeApi::default(),
            workspace.path(),
            FrlDiscoveryPlan {
                authoritative_titles: vec![title("A0001", "One")],
                versions: Vec::new(),
                proposed_cursor: Some(old_cursor),
            },
            10,
        )?;
        assert_eq!(report.failed, 0);
        let state = load_state(workspace.path())?;
        assert_eq!(
            state
                .inventory
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["A0001"]
        );
        Ok(())
    }

    #[test]
    fn cursor_and_inventory_do_not_advance_when_fetch_fails() -> Result<()> {
        let workspace = tempdir()?;
        let old_cursor = cursor("2024-01-15T00:00:00", "A0001")?;
        commit_state(
            workspace.path(),
            &FrlState {
                schema_version: STATE_SCHEMA_VERSION,
                cursor: Some(old_cursor.clone()),
                inventory: BTreeMap::new(),
            },
        )?;
        let next_version = version("A0001", "2024-01-16T00:00:00");
        let api = FakeApi::default()
            .with_document_pages("A0001", vec![vec![rendition("A0001", "Epub", ".epub")?]])?
            .with_payload("Epub", Err("fixture fetch failure".to_owned()));
        let result = fetch_plan(
            &api,
            workspace.path(),
            FrlDiscoveryPlan {
                authoritative_titles: vec![title("A0001", "One")],
                versions: vec![next_version.clone()],
                proposed_cursor: FrlCursor::from_version(&next_version)?,
            },
            10,
        );
        let error = match result {
            Err(error) => error,
            Ok(_) => bail!("fixture fetch unexpectedly succeeded"),
        };
        assert!(format!("{error:#}").contains("all supported official renditions failed"));
        let state = load_state(workspace.path())?;
        assert_eq!(state.cursor, Some(old_cursor));
        assert!(state.inventory.is_empty());
        Ok(())
    }

    #[test]
    fn failed_full_update_resumes_from_verified_staging() -> Result<()> {
        let workspace = tempdir()?;
        let first_version = version("A0001", "2024-01-16T00:00:00");
        let second_version = version("A0002", "2024-01-17T00:00:00");
        let plan = FrlDiscoveryPlan {
            authoritative_titles: vec![title("A0001", "One"), title("A0002", "Two")],
            versions: vec![first_version.clone(), second_version.clone()],
            proposed_cursor: FrlCursor::from_version(&second_version)?,
        };
        let first_api = FakeApi::default()
            .with_document_pages("A0001", vec![vec![rendition("A0001", "Epub", ".epub")?]])?
            .with_document_pages("A0002", vec![vec![rendition("A0002", "Word", ".docx")?]])?
            .with_payload(
                "Epub",
                Ok(FrlPayload::Epub(fs::read(
                    Path::new(FIXTURES).join("sample.epub"),
                )?)),
            )
            .with_payload("Word", Err("fixture interruption".to_owned()));
        assert!(fetch_plan(&first_api, workspace.path(), plan.clone(), 10).is_err());
        assert!(load_state(workspace.path())?.inventory.is_empty());
        assert!(load_staging_entry(workspace.path(), "A0001")?.is_some());

        let resumed_api = FakeApi::default()
            .with_document_pages("A0002", vec![vec![rendition("A0002", "Word", ".docx")?]])?
            .with_payload(
                "Word",
                Ok(FrlPayload::Docx(fs::read(
                    Path::new(FIXTURES).join("sample.docx"),
                )?)),
            );
        let report = fetch_plan(&resumed_api, workspace.path(), plan, 10)?;
        assert_eq!(report.completed, 1);
        assert_eq!(report.skipped, 1);
        assert_eq!(load_state(workspace.path())?.inventory.len(), 2);
        Ok(())
    }

    #[test]
    fn rendition_acquisition_uses_the_source_concurrency_policy() -> Result<()> {
        let workspace = tempdir()?;
        let first_version = version("A0001", "2024-01-16T00:00:00");
        let second_version = version("A0002", "2024-01-17T00:00:00");
        let api = FakeApi::default()
            .with_document_pages("A0001", vec![vec![rendition("A0001", "Epub", ".epub")?]])?
            .with_document_pages("A0002", vec![vec![rendition("A0002", "Word", ".docx")?]])?
            .with_payload(
                "Epub",
                Ok(FrlPayload::Epub(fs::read(
                    Path::new(FIXTURES).join("sample.epub"),
                )?)),
            )
            .with_payload(
                "Word",
                Ok(FrlPayload::Docx(fs::read(
                    Path::new(FIXTURES).join("sample.docx"),
                )?)),
            )
            .with_fetch_delay(Duration::from_millis(50));
        fetch_plan(
            &api,
            workspace.path(),
            FrlDiscoveryPlan {
                authoritative_titles: vec![title("A0001", "One"), title("A0002", "Two")],
                versions: vec![first_version, second_version.clone()],
                proposed_cursor: FrlCursor::from_version(&second_version)?,
            },
            10,
        )?;
        let observed = api.max_active_fetches.load(AtomicOrdering::SeqCst);
        assert_eq!(observed, 2);
        assert!(observed <= SOURCE_WORKER_CEILING);
        Ok(())
    }

    #[test]
    fn rendition_preference_is_epub_then_docx_then_pdf() -> Result<()> {
        let renditions: Vec<FrlRendition> = fixture_json("renditions.json")?;
        assert_eq!(select_rendition(&renditions)?.format, "Epub");
        let without_epub = renditions
            .iter()
            .filter(|rendition| rendition.format != "Epub")
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(select_rendition(&without_epub)?.format, "Word");
        let pdf_only = without_epub
            .iter()
            .filter(|rendition| rendition.format != "Word")
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(select_rendition(&pdf_only)?.format, "Pdf");
        Ok(())
    }

    #[test]
    fn invalid_epub_falls_back_to_the_official_docx_rendition() -> Result<()> {
        let source_title = title("A0001", "Example Act");
        let source_version = version("A0001", "2024-02-01T00:00:00");
        let renditions = vec![
            rendition("A0001", "Epub", ".epub")?,
            rendition("A0001", "Word", ".docx")?,
        ];
        let api = FakeApi::default()
            .with_payload("Epub", Ok(FrlPayload::Epub(b"invalid EPUB".to_vec())))
            .with_payload(
                "Word",
                Ok(FrlPayload::Docx(fs::read(
                    Path::new(FIXTURES).join("sample.docx"),
                )?)),
            );
        let (selected, document) =
            fetch_preferred_normalized_document(&api, &source_title, &source_version, &renditions)?;
        assert_eq!(selected.format, "Word");
        assert!(document.cleaned_html.contains("rule"));
        Ok(())
    }

    #[test]
    fn pdf_requires_official_extracted_text() -> Result<()> {
        let rendition = rendition("A0001", "Pdf", ".pdf")?;
        let response: OfficialTextResponse = serde_json::from_str(r#"{"Contents":"official"}"#)?;
        assert_eq!(response.contents.as_deref(), Some("official"));
        let error = normalize_official_pdf_text("")
            .err()
            .ok_or_else(|| anyhow!("empty official PDF text unexpectedly normalized"))?;
        assert!(error.to_string().contains("empty official extracted text"));
        assert_eq!(rendition_kind(&rendition), Some(RenditionKind::Pdf));
        Ok(())
    }

    #[test]
    fn pdf_without_official_extracted_text_indexes_only_official_metadata() -> Result<()> {
        let source_title = title("F2006B03624", "AD/BEECH 18/5 & Inspection");
        let source_version = version("F2006B03624", "2004-12-20T00:00:00");
        let source_rendition = rendition("F2006B03624", "Pdf", ".pdf")?;
        let document = normalize_document(
            &source_title,
            &source_version,
            &source_rendition,
            FrlPayload::OfficialMetadata,
        )?;
        assert_eq!(
            document.cleaned_html,
            "<article><h1>AD/BEECH 18/5 &amp; Inspection</h1></article>"
        );
        assert!(document.assets.is_empty());
        Ok(())
    }

    #[test]
    fn epub_and_docx_normalization_are_deterministic() -> Result<()> {
        let epub = fs::read(Path::new(FIXTURES).join("sample.epub"))?;
        let docx = fs::read(Path::new(FIXTURES).join("sample.docx"))?;
        let (epub_html, epub_assets) = normalize_epub(&epub, "A0001")?;
        let (docx_html, docx_assets) = normalize_docx(&docx, "A0001")?;
        assert_eq!(
            epub_html,
            fs::read_to_string(Path::new(FIXTURES).join("sample.epub.html"))?.trim()
        );
        assert_eq!(
            docx_html,
            fs::read_to_string(Path::new(FIXTURES).join("sample.docx.html"))?.trim()
        );
        assert_eq!(epub_assets.len(), 1);
        assert_eq!(docx_assets.len(), 1);
        assert!(epub_html.contains("data-asset-ref=\"frl:A0001/sha256-"));
        assert!(docx_html.contains("data-asset-ref=\"frl:A0001/sha256-"));
        assert!(epub_html.contains("<strong>beta</strong> and <em>delta</em> epsilon"));
        assert!(docx_html.contains("<strong>rule</strong> applies"));
        assert!(!docx_html.contains("PAGE"));

        let render_chunks = |html: &str| -> Result<String> {
            Ok(crate::chunker::chunk_html_with_token_count(
                html,
                None,
                crate::chunker::EMBED_MAX_TOKENS,
                |text| Ok(text.split_whitespace().count().max(1)),
            )?
            .into_iter()
            .map(|chunk| chunk.text)
            .collect::<Vec<_>>()
            .join("\n"))
        };
        let asset_marker = "[asset:frl:A0001/sha256-111d58ef8ef321b2f4e97801899f03f9ef0c4b00ead9fb641d868e59b6c77f5b]";
        let epub_chunks = render_chunks(&epub_html)?;
        let docx_chunks = render_chunks(&docx_html)?;
        assert!(epub_chunks.contains(&format!("[image: Seal] {asset_marker}")));
        assert!(docx_chunks.contains(asset_marker));
        assert_eq!(epub_chunks.matches(asset_marker).count(), 1);
        assert_eq!(docx_chunks.matches(asset_marker).count(), 1);
        Ok(())
    }

    #[test]
    fn genuine_docx_numbering_resolves_levels_formats_starts_and_overrides() -> Result<()> {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/federal-court/numbered-paragraph-footnote.docx");
        let bytes = fs::read(fixture)?;
        let archive = read_zip_archive(&bytes, "DOCX numbering fixture")?;
        let document = archive_text(
            &archive,
            Path::new("word/document.xml"),
            "DOCX numbering fixture",
        )?;
        assert!(document.contains("<w:numPr>"));
        assert!(archive.contains_key(Path::new("word/numbering.xml")));
        assert!(!document.contains("<w:t>1</w:t>"));
        assert!(!document.contains("<w:t>2</w:t>"));

        let numbering = parse_docx_numbering(&archive)?;
        let levels = numbering
            .instances
            .get(&42)
            .ok_or_else(|| anyhow!("fixture numbering instance was not parsed"))?;
        assert_eq!(levels.get(&0).map(|level| level.start), Some(1));
        assert_eq!(
            levels.get(&1).map(|level| level.format.as_str()),
            Some("lowerLetter")
        );
        assert_eq!(levels.get(&1).map(|level| level.start), Some(3));
        assert_eq!(format_docx_number(27, "upperLetter"), "AA");
        assert_eq!(format_docx_number(14, "lowerRoman"), "xiv");
        assert_eq!(format_docx_number(22, "ordinal"), "22nd");

        let (html, assets) = normalize_docx_with_source(
            &bytes,
            crate::official_sources::FEDERAL_COURT_SOURCE_ID,
            "fca/single/2026/2026fca0001",
        )?;
        assert!(assets.is_empty());
        assert!(html.contains("<p>1 The numbered paragraph"));
        assert!(html.contains("<p>(c) The nested point"));
        assert!(html.contains("<p>2 The next numbered paragraph"));
        assert!(html.contains("id=\"footnote-reference-9\" href=\"#footnote-9\">1</a>"));
        assert!(html.contains("id=\"footnote-reference-2\" href=\"#footnote-2\">2</a>"));
        assert!(html.find("id=\"footnote-9\"").unwrap() < html.find("id=\"footnote-2\"").unwrap());
        Ok(())
    }

    #[test]
    fn docx_numbering_resolves_a_numbering_style_link() -> Result<()> {
        let numbering = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:abstractNum w:abstractNumId="19">
    <w:styleLink w:val="FCListNo"/>
    <w:lvl w:ilvl="0">
      <w:start w:val="1"/>
      <w:numFmt w:val="decimal"/>
      <w:lvlText w:val="(%1)"/>
    </w:lvl>
  </w:abstractNum>
  <w:abstractNum w:abstractNumId="45">
    <w:numStyleLink w:val="FCListNo"/>
  </w:abstractNum>
  <w:num w:numId="20"><w:abstractNumId w:val="19"/></w:num>
  <w:num w:numId="21"><w:abstractNumId w:val="45"/></w:num>
</w:numbering>"#;
        let styles = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:style w:type="numbering" w:styleId="FCListNo">
    <w:pPr><w:numPr><w:numId w:val="20"/></w:numPr></w:pPr>
  </w:style>
</w:styles>"#;
        let archive = BTreeMap::from([
            (PathBuf::from("word/numbering.xml"), numbering.to_vec()),
            (PathBuf::from("word/styles.xml"), styles.to_vec()),
        ]);
        let parsed = parse_docx_numbering(&archive)?;
        let level = parsed
            .instances
            .get(&21)
            .and_then(|levels| levels.get(&0))
            .ok_or_else(|| anyhow!("linked numbering level was not resolved"))?;
        assert_eq!(level.start, 1);
        assert_eq!(level.format, "decimal");
        assert_eq!(level.text, "(%1)");
        Ok(())
    }

    #[test]
    fn docx_numbering_rejects_an_unresolved_numbering_style_link() {
        let numbering = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:abstractNum w:abstractNumId="45">
    <w:numStyleLink w:val="MissingStyle"/>
  </w:abstractNum>
  <w:num w:numId="21"><w:abstractNumId w:val="45"/></w:num>
</w:numbering>"#;
        let archive = BTreeMap::from([(PathBuf::from("word/numbering.xml"), numbering.to_vec())]);
        assert!(parse_docx_numbering(&archive)
            .unwrap_err()
            .to_string()
            .contains("links to missing numbering style MissingStyle"));
    }

    #[test]
    fn explicit_nonnumbered_paragraph_style_suppresses_numbered_default() -> Result<()> {
        let properties = parse_xml(
            r#"<w:pPr xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:pStyle w:val="Plain"/></w:pPr>"#,
        )?;
        let styles = DocxParagraphStyles {
            default_style: Some("Default".to_string()),
            numbering: BTreeMap::from([
                ("Default".to_string(), Some((7, 0))),
                ("Plain".to_string(), None),
            ]),
        };

        assert_eq!(
            paragraph_or_default_style_numbering(Some(&properties), &styles),
            None
        );
        Ok(())
    }

    #[test]
    fn paragraph_style_inheritance_has_a_hard_depth_bound() {
        let mut raw = BTreeMap::new();
        for index in 0..=MAX_DOCX_STYLE_REFERENCE_DEPTH {
            raw.insert(
                format!("Style{index}"),
                DocxRawParagraphStyle {
                    based_on: (index > 0).then(|| format!("Style{}", index - 1)),
                    numbering: None,
                },
            );
        }
        let mut resolving = BTreeSet::new();
        let mut resolved = BTreeMap::new();
        assert!(resolve_docx_paragraph_style(
            &format!("Style{MAX_DOCX_STYLE_REFERENCE_DEPTH}"),
            &raw,
            &mut resolving,
            &mut resolved,
        )
        .is_err());
    }

    #[test]
    fn deleted_footnote_references_do_not_consume_visible_ordinals() -> Result<()> {
        let document = parse_xml(
            r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:del><w:r><w:footnoteReference w:id="7"/></w:r></w:del><w:r><w:footnoteReference w:id="9"/></w:r></w:p></w:body></w:document>"#,
        )?;
        let mut seen = BTreeSet::new();
        let mut ordered = Vec::new();
        collect_visible_docx_footnote_references(&document, &mut seen, &mut ordered)?;
        assert_eq!(ordered, vec![9]);
        Ok(())
    }

    #[test]
    fn numbering_restart_zero_preserves_a_child_counter() -> Result<()> {
        let parent = DocxNumberingLevel {
            start: 1,
            format: "decimal".to_string(),
            text: "%1".to_string(),
            suffix: DocxNumberingSuffix::Space,
            legal: false,
            restart_after: None,
        };
        let child = DocxNumberingLevel {
            start: 1,
            format: "decimal".to_string(),
            text: "%1.%2".to_string(),
            suffix: DocxNumberingSuffix::Space,
            legal: false,
            restart_after: None,
        };
        let numbering = DocxNumbering {
            instances: BTreeMap::from([(20, BTreeMap::from([(0, parent), (1, child)]))]),
        };
        let styles = DocxParagraphStyles::default();
        let paragraph = |level| {
            parse_xml(&format!(
                r#"<w:p xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:pPr><w:numPr><w:ilvl w:val="{level}"/><w:numId w:val="20"/></w:numPr></w:pPr></w:p>"#
            ))
        };
        let mut state = DocxNumberingState::default();
        assert_eq!(
            numbering
                .paragraph_marker(&paragraph(1)?, &styles, &mut state)?
                .map(|marker| marker.0),
            Some("1.1".to_string())
        );
        numbering.paragraph_marker(&paragraph(0)?, &styles, &mut state)?;
        assert_eq!(
            numbering
                .paragraph_marker(&paragraph(1)?, &styles, &mut state)?
                .map(|marker| marker.0),
            Some("2.2".to_string())
        );
        Ok(())
    }

    #[test]
    fn numbering_style_inherits_its_numbering_instance() -> Result<()> {
        let styles = br#"<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
          <w:style w:type="numbering" w:styleId="Base"><w:pPr><w:numPr><w:numId w:val="20"/></w:numPr></w:pPr></w:style>
          <w:style w:type="numbering" w:styleId="Child"><w:basedOn w:val="Base"/></w:style>
        </w:styles>"#;
        let archive = BTreeMap::from([(PathBuf::from("word/styles.xml"), styles.to_vec())]);
        let resolved = parse_docx_numbering_style_ids(&archive)?;
        assert_eq!(resolved.get("Child"), Some(&20));
        Ok(())
    }

    #[test]
    fn asset_markers_encode_citation_characters_canonically() -> Result<()> {
        let mut assets = AssetCollector::new("high-court", "[2025] HCA 50")?;
        let marker = assets.insert(vec![1, 2, 3], "image/png".to_string())?;
        assert!(marker.starts_with("high-court:%5B2025%5D%20HCA%2050/sha256-"));
        let parsed: AssetRef = marker.parse()?;
        assert!(parsed.asset_id.starts_with("[2025] HCA 50/sha256-"));
        Ok(())
    }

    #[test]
    fn frl_workspace_normalization_indexes_and_builds_a_source_ann() -> Result<()> {
        let workspace = tempdir()?;
        let source_title = title("A0001", "Example Act");
        let source_version = version("A0001", "2024-02-01T00:00:00");
        let source_rendition = rendition("A0001", "Epub", ".epub")?;
        let normalized = normalize_document(
            &source_title,
            &source_version,
            &source_rendition,
            FrlPayload::Epub(fs::read(Path::new(FIXTURES).join("sample.epub"))?),
        )?;
        let stored = persist_document(workspace.path(), &normalized)?;
        let stored_hash = stored.content_hash.clone();
        let cursor = FrlCursor::from_version(&source_version)?
            .ok_or_else(|| anyhow!("fixture version has no registration cursor"))?;
        commit_state(
            workspace.path(),
            &FrlState {
                schema_version: STATE_SCHEMA_VERSION,
                cursor: Some(cursor.clone()),
                inventory: BTreeMap::from([(
                    source_title.id.clone(),
                    FrlInventoryEntry {
                        native_id: source_title.id.clone(),
                        upstream_version: FrlVersionKey::from_version(&source_version)?,
                        register_id: source_version.register_id.clone(),
                        canonical_url: normalized.canonical_url.clone(),
                        payload_hash: stored_hash.clone(),
                        last_successful_cursor: cursor,
                    },
                )]),
            },
        )?;

        let documents = load_normalized_documents(workspace.path())?;
        assert_eq!(documents.len(), 1);
        assert_eq!(
            documents[0].inventory.document.source.as_str(),
            FRL_SOURCE_ID
        );

        let mut conn = rusqlite::Connection::open_in_memory()?;
        crate::db::init_db(&conn)?;
        let source: SourceId = FRL_SOURCE_ID.parse()?;
        let descriptor = frl_descriptor()?;
        let report = crate::pipeline::ingest_source(
            &mut conn,
            &source,
            &descriptor,
            documents,
            &FixtureEmbeddings,
        )?;
        assert_eq!(report.inserted_documents, 1);
        assert!(report.inserted_chunks > 0);
        let ann_root = tempdir()?;
        let ann = crate::pipeline::finalise_source_ann(&conn, &source, ann_root.path())?;
        assert_eq!(ann.source_id, source);
        assert!(ann_root.path().join(ann.path).is_file());

        let stored_path = workspace
            .path()
            .join("documents")
            .join(&stored_hash[..2])
            .join(format!("{stored_hash}.json"));
        let mut tampered: serde_json::Value = serde_json::from_slice(&fs::read(&stored_path)?)?;
        tampered["title"] = serde_json::Value::String("Tampered Act".to_string());
        fs::write(&stored_path, serde_json::to_vec(&tampered)?)?;
        assert!(load_normalized_documents(workspace.path())
            .unwrap_err()
            .to_string()
            .contains("content-hash validation"));
        Ok(())
    }

    #[test]
    #[ignore = "requires the pinned ONNX embedding model bundle"]
    fn registered_source_fixtures_build_one_verified_generation() -> Result<()> {
        let model_dir = std::env::var_os("LEGAL_MCP_TEST_MODEL_DIR")
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("LEGAL_MCP_TEST_MODEL_DIR is required"))?;
        let root = tempdir()?;
        let ato_workspace = root.path().join("ato");
        let ato_payload = ato_workspace.join("payloads/cr-2025-13.html");
        let ato_payload_parent = ato_payload
            .parent()
            .ok_or_else(|| anyhow!("ATO fixture payload path has no parent"))?;
        fs::create_dir_all(ato_payload_parent)?;
        let ato_fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ato/cr-2025-13.html");
        fs::copy(&ato_fixture, &ato_payload)?;
        let ato_bytes = fs::read(&ato_payload)?;
        fs::write(
            ato_workspace.join("index.jsonl"),
            format!(
                "{}\n",
                serde_json::to_string(&serde_json::json!({
                    "canonical_id": "/law/view/document?docid=CLR/CR202513/NAT/ATO/00001",
                    "href": "/law/view/document?docid=CLR/CR202513/NAT/ATO/00001",
                    "status": "success",
                    "payload_path": "payloads/cr-2025-13.html",
                    "sha256": format!("{:x}", Sha256::digest(&ato_bytes)),
                    "size": ato_bytes.len(),
                }))?
            ),
        )?;

        let frl_workspace = root.path().join("frl");
        fs::create_dir_all(&frl_workspace)?;
        let source_title = title("A0001", "Example Act");
        let source_version = version("A0001", "2024-02-01T00:00:00");
        let source_rendition = rendition("A0001", "Epub", ".epub")?;
        let normalized = normalize_document(
            &source_title,
            &source_version,
            &source_rendition,
            FrlPayload::Epub(fs::read(Path::new(FIXTURES).join("sample.epub"))?),
        )?;
        let stored = persist_document(&frl_workspace, &normalized)?;
        let cursor = FrlCursor::from_version(&source_version)?
            .ok_or_else(|| anyhow!("fixture version has no registration cursor"))?;
        commit_state(
            &frl_workspace,
            &FrlState {
                schema_version: STATE_SCHEMA_VERSION,
                cursor: Some(cursor.clone()),
                inventory: BTreeMap::from([(
                    source_title.id.clone(),
                    FrlInventoryEntry {
                        native_id: source_title.id.clone(),
                        upstream_version: FrlVersionKey::from_version(&source_version)?,
                        register_id: source_version.register_id.clone(),
                        canonical_url: normalized.canonical_url,
                        payload_hash: stored.content_hash,
                        last_successful_cursor: cursor,
                    },
                )]),
            },
        )?;

        let output = root.path().join("generation");
        let database = output.join(crate::config::LEGAL_DB_FILENAME);
        let mut source_workspaces = BTreeMap::from([
            (
                crate::source_catalog::ATO_SOURCE_ID.parse()?,
                ato_workspace.clone(),
            ),
            (FRL_SOURCE_ID.parse()?, frl_workspace.clone()),
        ]);
        for source in crate::legal_source::source_registry().source_ids() {
            if matches!(source, crate::source_catalog::ATO_SOURCE_ID | FRL_SOURCE_ID) {
                continue;
            }
            let source_id: SourceId = source.parse()?;
            let workspace = root.path().join(source);
            fs::create_dir_all(&workspace)?;
            crate::official_sources::seed_test_workspace(&source_id, &workspace)?;
            source_workspaces.insert(source_id, workspace);
        }
        crate::build::build_corpus(crate::build::BuildCorpusArgs {
            source_workspaces: &source_workspaces,
            db_path: &database,
            model_dir: &model_dir,
            embedding_cache_db: None,
            out_dir: &output,
            zstd_level: 1,
            profile_enabled: false,
        })?;
        let manifest_path = output.join(crate::config::GENERATION_MANIFEST_FILENAME);
        let manifest: crate::source::Manifest = serde_json::from_slice(&fs::read(&manifest_path)?)?;
        let connection = rusqlite::Connection::open(&database)?;
        crate::source::validate_manifest(&manifest)?;
        crate::source::verify_corpus_manifest_binding(&connection, &manifest)?;
        crate::source::verify_semantic_install(&connection, &manifest)?;
        for (source, ann) in &manifest.ann {
            crate::ann::verify_sidecar(&output.join(&ann.path), source, ann)?;
        }
        crate::source::validate_generation_dir(&output)?;
        Ok(())
    }

    #[test]
    fn normalized_loader_requires_nonempty_committed_state() -> Result<()> {
        let workspace = tempdir()?;
        assert!(load_normalized_documents(workspace.path()).is_err());
        commit_state(
            workspace.path(),
            &FrlState {
                schema_version: STATE_SCHEMA_VERSION,
                ..FrlState::default()
            },
        )?;
        assert!(load_normalized_documents(workspace.path())
            .unwrap_err()
            .to_string()
            .contains("inventory is empty"));
        Ok(())
    }

    #[test]
    fn asset_identity_binds_media_type_and_bytes() -> Result<()> {
        let bytes = vec![1, 2, 3, 4];
        let mut assets = AssetCollector::new(FRL_SOURCE_ID, "A0001")?;
        let png = assets.insert(bytes.clone(), "image/png".to_owned())?;
        let jpeg = assets.insert(bytes.clone(), "image/jpeg".to_owned())?;
        assert_ne!(png, jpeg);
        assert_eq!(assets.into_vec().len(), 2);
        let same_asset_in_another_document =
            AssetCollector::new(FRL_SOURCE_ID, "A0002")?.insert(bytes, "image/png".to_owned())?;
        assert_ne!(png, same_asset_in_another_document);
        Ok(())
    }

    #[test]
    fn archive_and_workspace_paths_are_confined() -> Result<()> {
        assert!(safe_archive_path("../escape.xml").is_err());
        assert!(safe_archive_path("%2e%2e/escape.xml").is_err());
        assert!(safe_archive_path("word%5cescape.xml").is_err());
        assert!(safe_archive_path("word\\escape.xml").is_err());
        assert!(resolve_archive_reference(Path::new("word/document.xml"), "../../escape").is_err());
        let workspace = tempdir()?;
        assert!(confined_path(workspace.path(), Path::new("../escape")).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            let outside = tempdir()?;
            symlink(outside.path(), workspace.path().join("link"))?;
            assert!(
                atomic_write_confined(workspace.path(), Path::new("link/escape"), b"blocked")
                    .is_err()
            );
            assert!(!outside.path().join("escape").exists());
        }
        Ok(())
    }
}
