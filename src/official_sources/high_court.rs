use super::*;

pub(super) static ADAPTER: HighCourt = HighCourt;
pub(super) struct HighCourt;

const HOST: &str = "www.hcourt.gov.au";
const JUDGMENTS_INDEX_PATH: &str = "/cases-and-judgments/judgments";
const JUDGMENT_CATEGORY_PATH_PREFIX: &str = "/cases-and-judgments/judgments/";
const MAX_JUDGMENT_CATEGORY_CANDIDATES: usize = 64;
const MAX_JUDGMENTS: usize = 100_000;
// Official-only coverage follows the categories published by the Court. The
// landing page currently leaves a reported-judgment gap from 1960 through 1997,
// and describes its separate 1906-1994 unreported collection as incomplete.
// The Court's per-judgment CLR scans can exceed 170 MiB.
const MAX_HISTORICAL_JUDGMENT_BYTES: u64 = 512 * 1024 * 1024;

impl OfficialAdapter for HighCourt {
    fn source_id(&self) -> &'static str {
        HIGH_COURT_SOURCE_ID
    }
    fn display_name(&self) -> &'static str {
        "High Court of Australia judgments"
    }
    fn approved_hosts(&self) -> &'static [&'static str] {
        &[HOST]
    }
    fn rate_policy(&self) -> SourceRatePolicy {
        SourceRatePolicy {
            minimum_request_interval_ms: 0,
            request_timeout_seconds: 30,
        }
    }

    fn normalization_revision(&self) -> Option<&'static str> {
        Some("1")
    }

    fn minimum_snapshot_retention_percent(&self) -> usize {
        99
    }

    fn discover(
        &self,
        client: &OfficialHttpClient,
        _mode: SourceUpdateMode,
    ) -> Result<Vec<DiscoveredDocument>> {
        discover_current_site(client, SOURCE_WORKER_CEILING)
    }

    fn acquire(
        &self,
        client: &OfficialHttpClient,
        entry: &DiscoveredDocument,
    ) -> Result<Option<AcquiredDocument>> {
        let landing = client.get(&entry.renditions[0].url, "text/html", MAX_DOCUMENT_BYTES)?;
        validate_exact_hca_response_url(&entry.renditions[0].url, &landing.final_url)?;
        if landing.status == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !landing.status.is_success() {
            bail!("High Court judgment returned HTTP {}", landing.status);
        }
        let html =
            decode_utf8(&landing.bytes).unwrap_or_else(|_| decode_windows_1252(&landing.bytes));
        let parsed = Html::parse_document(&html);
        let date_selector = Selector::parse("h2, .field--hca-date-issued")
            .map_err(|_| anyhow!("invalid High Court date selector"))?;
        let date = parsed
            .select(&date_selector)
            .find_map(|element| {
                let value = element.text().collect::<String>();
                parse_date(
                    value
                        .trim_start_matches("Judgment date")
                        .trim_start_matches("Date:")
                        .trim(),
                    &["%d %b %Y", "%d %B %Y"],
                )
            })
            .or_else(|| entry.date.clone());
        let case_selector = Selector::parse("div.wellCase")
            .map_err(|_| anyhow!("invalid High Court case selector"))?;
        if parsed.select(&case_selector).next().is_some() {
            let normalized = normalize_html(
                &html,
                &entry.canonical_url,
                HtmlRules {
                    content_selector: "div.wellCase",
                    drop_ids: &[],
                    drop_classes: &[],
                    heading_classes: &[],
                    preserve_same_document_fragments: false,
                    repair_broken_links: false,
                },
            )?;
            let mut acquired = make_acquired_html(normalized, entry.canonical_url.clone());
            acquired.date = date;
            return Ok(Some(acquired));
        }
        let link_selector = Selector::parse("a[href]")
            .map_err(|_| anyhow!("invalid High Court download selector"))?;
        let mut downloads = Vec::new();
        for element in parsed.select(&link_selector) {
            let Some(href) = element.value().attr("href") else {
                continue;
            };
            let lower_href = href.to_ascii_lowercase();
            let label = element
                .text()
                .collect::<String>()
                .trim()
                .to_ascii_uppercase();
            let kind = if lower_href.ends_with(".rtf") || label == "RTF" {
                Some(RenditionKind::Rtf)
            } else if lower_href.ends_with(".docx") || label == "DOCX" {
                Some(RenditionKind::Docx)
            } else if lower_href.ends_with(".pdf")
                || matches!(label.as_str(), "PDF" | "VIEW" | "DOWNLOAD")
            {
                Some(RenditionKind::Pdf)
            } else {
                None
            };
            if let Some(kind) = kind {
                downloads.push((
                    Url::parse(&entry.canonical_url)?.join(href)?.to_string(),
                    kind,
                ));
            }
        }
        downloads.sort_by_key(|(_, kind)| match kind {
            RenditionKind::Docx => 0,
            RenditionKind::Rtf => 1,
            RenditionKind::Pdf => 2,
            RenditionKind::Html => 3,
        });
        let had_downloads = !downloads.is_empty();
        let mut failures = Vec::new();
        for (url, kind) in downloads {
            let payload = client.get(
                &url,
                "application/octet-stream",
                MAX_HISTORICAL_JUDGMENT_BYTES,
            )?;
            validate_exact_hca_response_url(&url, &payload.final_url)?;
            if !payload.status.is_success()
                || payload
                    .bytes
                    .windows(27)
                    .any(|window| window == b"Document could not be found")
            {
                failures.push(format!(
                    "{url}: HTTP {} or missing document",
                    payload.status
                ));
                continue;
            }
            let result = normalize_hca_download(&payload.bytes, kind, entry);
            match result {
                Ok((normalized, assets)) => {
                    return Ok(Some(AcquiredDocument {
                        html: normalized,
                        assets,
                        date,
                        canonical_url: entry.canonical_url.clone(),
                    }));
                }
                Err(error) => failures.push(format!("{url}: {error:#}")),
            }
        }
        if had_downloads {
            bail!(
                "all High Court renditions failed for {}: {}",
                entry.native_id,
                failures.join("; ")
            );
        }
        Ok(None)
    }
}

fn validate_exact_hca_response_url(requested: &str, final_url: &Url) -> Result<()> {
    let requested = Url::parse(requested).context("parsing requested High Court URL")?;
    if final_url != &requested {
        bail!("High Court response URL differs from the requested official resource");
    }
    Ok(())
}

fn normalize_hca_download(
    bytes: &[u8],
    advertised_kind: RenditionKind,
    entry: &DiscoveredDocument,
) -> Result<(String, Vec<NormalizedAsset>)> {
    if bytes.starts_with(b"%PDF") {
        return normalize_pdf(bytes).map(|html| (html, Vec::new()));
    }
    if bytes.starts_with(b"PK\x03\x04") {
        let source: SourceId = HIGH_COURT_SOURCE_ID.parse()?;
        return crate::frl::normalize_docx_for_source(bytes, &source, &entry.native_id);
    }
    if bytes.starts_with(&[0xd0, 0xcf, 0x11, 0xe0])
        || bytes
            .iter()
            .position(|byte| !byte.is_ascii_whitespace())
            .and_then(|start| bytes.get(start..))
            .is_some_and(|bytes| bytes.starts_with(b"{\\rtf"))
    {
        return normalize_rtf(bytes, &entry.canonical_url).map(|html| (html, Vec::new()));
    }
    match advertised_kind {
        RenditionKind::Rtf => {
            normalize_rtf(bytes, &entry.canonical_url).map(|html| (html, Vec::new()))
        }
        RenditionKind::Docx => {
            let source: SourceId = HIGH_COURT_SOURCE_ID.parse()?;
            crate::frl::normalize_docx_for_source(bytes, &source, &entry.native_id)
        }
        RenditionKind::Pdf => normalize_pdf(bytes).map(|html| (html, Vec::new())),
        RenditionKind::Html => bail!("unsupported High Court download format"),
    }
}

fn discover_current_site(
    client: &OfficialHttpClient,
    concurrency: usize,
) -> Result<Vec<DiscoveredDocument>> {
    let index_url = judgments_index_url()?;
    let payload = client.get_required(index_url.as_str(), "text/html", MAX_INDEX_BYTES)?;
    validate_discovery_response_url(&index_url, &payload.final_url)?;
    let index_html = decode_utf8(&payload.bytes)?;
    discover_from_index(&index_html, concurrency, |url| {
        let payload = client.get_required(url.as_str(), "text/html", MAX_INDEX_BYTES)?;
        validate_discovery_response_url(url, &payload.final_url)?;
        decode_utf8(&payload.bytes)
    })
}

struct JudgmentCategory {
    url: Url,
    total: usize,
    last_page: usize,
    ranges: Vec<ResultRange>,
    documents: Vec<DiscoveredDocument>,
}

enum CategoryPage {
    JudgmentListing {
        total: usize,
        last_page: usize,
        range: ResultRange,
        documents: Vec<DiscoveredDocument>,
    },
    ResourceCollection,
}

#[derive(Clone, Copy, Debug)]
struct ResultRange {
    first: usize,
    last: usize,
    total: usize,
}

impl ResultRange {
    fn displayed(self) -> usize {
        if self.total == 0 {
            0
        } else {
            self.last - self.first + 1
        }
    }
}

fn discover_from_index<F>(
    index_html: &str,
    concurrency: usize,
    fetch_html: F,
) -> Result<Vec<DiscoveredDocument>>
where
    F: Fn(&Url) -> Result<String> + Send + Sync,
{
    let category_urls = parse_judgment_category_urls(index_html)?;
    let first_pages = parallel_map(concurrency, category_urls, |url| {
        let html =
            fetch_html(&url).with_context(|| format!("fetching High Court category {url}"))?;
        match classify_category_page(&html, &url)? {
            CategoryPage::JudgmentListing {
                total,
                last_page,
                range,
                documents,
            } => Ok(Some(JudgmentCategory {
                url,
                total,
                last_page,
                ranges: vec![range],
                documents,
            })),
            CategoryPage::ResourceCollection => Ok(None),
        }
    })?;
    let mut categories = first_pages.into_iter().flatten().collect::<Vec<_>>();
    if categories.is_empty() {
        bail!("High Court judgments index has no judgment categories");
    }
    let expected_total = categories.iter().try_fold(0usize, |total, category| {
        total
            .checked_add(category.total)
            .filter(|total| *total <= MAX_JUDGMENTS)
            .ok_or_else(|| anyhow!("High Court judgment inventory exceeds its discovery bound"))
    })?;
    let mut page_requests = Vec::new();
    for (category_index, category) in categories.iter().enumerate() {
        for page in 1..=category.last_page {
            page_requests.push((
                category_index,
                category.total,
                category_page_url(&category.url, page),
            ));
        }
    }
    let pages = parallel_map(
        concurrency,
        page_requests,
        |(category_index, expected_total, url)| {
            let html =
                fetch_html(&url).with_context(|| format!("fetching High Court page {url}"))?;
            match classify_category_page(&html, &url)? {
                CategoryPage::JudgmentListing {
                    total,
                    range,
                    documents,
                    ..
                } if total == expected_total => Ok((category_index, range, documents)),
                CategoryPage::JudgmentListing { total, .. } => bail!(
                    "High Court category page {url} changed its result total from {expected_total} to {total}"
                ),
                CategoryPage::ResourceCollection => {
                    bail!("High Court judgment category page {url} became a different collection")
                }
            }
        },
    )?;
    for (category_index, range, documents) in pages {
        categories[category_index].ranges.push(range);
        categories[category_index].documents.extend(documents);
    }
    let mut documents = Vec::with_capacity(expected_total);
    for mut category in categories {
        category.ranges.sort_by_key(|range| range.first);
        let mut expected_first = if category.total == 0 { 0 } else { 1 };
        for range in &category.ranges {
            if range.first != expected_first || range.total != category.total {
                bail!(
                    "High Court category {} has an overlapping or incomplete result-range partition",
                    category.url
                );
            }
            expected_first = if category.total == 0 {
                0
            } else {
                range
                    .last
                    .checked_add(1)
                    .ok_or_else(|| anyhow!("High Court result range overflow"))?
            };
        }
        if (category.total == 0 && category.ranges.len() != 1)
            || (category.total > 0 && expected_first != category.total + 1)
        {
            bail!(
                "High Court category {} does not cover its complete result range",
                category.url
            );
        }
        if category.documents.len() != category.total {
            bail!(
                "High Court category {} expected {} judgments but parsed {}",
                category.url,
                category.total,
                category.documents.len()
            );
        }
        documents.extend(category.documents);
    }
    if documents.len() != expected_total {
        bail!(
            "High Court discovery expected {expected_total} judgments but parsed {}",
            documents.len()
        );
    }
    validate_judgment_inventory(&documents)?;
    Ok(documents)
}

fn judgments_index_url() -> Result<Url> {
    Url::parse(&format!("https://{HOST}{JUDGMENTS_INDEX_PATH}")).map_err(Into::into)
}

fn parse_judgment_category_urls(html: &str) -> Result<Vec<Url>> {
    parse_judgment_category_urls_with_limit(html, MAX_JUDGMENT_CATEGORY_CANDIDATES)
}

fn parse_judgment_category_urls_with_limit(html: &str, maximum: usize) -> Result<Vec<Url>> {
    let parsed = Html::parse_document(html);
    let selector = Selector::parse("main a[href]")
        .map_err(|_| anyhow!("invalid High Court category selector"))?;
    let title_selector = Selector::parse("main .field--name-field-title a[href]")
        .map_err(|_| anyhow!("invalid High Court category-title selector"))?;
    let index_url = judgments_index_url()?;
    let mut summary_collections = BTreeSet::new();
    for element in parsed.select(&title_selector) {
        let title = element
            .text()
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();
        if !title.contains("summar") {
            continue;
        }
        let Some(href) = element.value().attr("href") else {
            continue;
        };
        let Ok(url) = index_url.join(href) else {
            continue;
        };
        if is_allowed_category_url(&url) {
            summary_collections.insert(url.to_string());
        }
    }
    let mut categories = BTreeSet::new();
    for element in parsed.select(&selector) {
        let Some(href) = element.value().attr("href") else {
            continue;
        };
        let Ok(url) = index_url.join(href) else {
            continue;
        };
        if is_allowed_category_url(&url) && !summary_collections.contains(url.as_str()) {
            categories.insert(url.to_string());
            if categories.len() > maximum {
                bail!("High Court judgments index exceeds its category bound");
            }
        }
    }
    if categories.is_empty() {
        bail!("High Court judgments index has no allowlisted category links");
    }
    categories
        .into_iter()
        .map(|url| Url::parse(&url).map_err(Into::into))
        .collect()
}

fn is_allowed_category_url(url: &Url) -> bool {
    if url.scheme() != "https"
        || url.host_str() != Some(HOST)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return false;
    }
    let Some(slug) = url.path().strip_prefix(JUDGMENT_CATEGORY_PATH_PREFIX) else {
        return false;
    };
    !slug.is_empty()
        && !slug.contains('/')
        && slug
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn validate_discovery_response_url(requested: &Url, final_url: &Url) -> Result<()> {
    if final_url.scheme() != "https"
        || final_url.host_str() != Some(HOST)
        || !final_url.username().is_empty()
        || final_url.password().is_some()
        || final_url.port().is_some()
        || final_url.path() != requested.path()
        || final_url.query() != requested.query()
        || final_url.fragment().is_some()
    {
        bail!("High Court discovery escaped its allowlisted host or path");
    }
    Ok(())
}

fn category_page_url(category: &Url, page: usize) -> Url {
    let mut url = category.clone();
    url.query_pairs_mut()
        .clear()
        .append_pair("page", &page.to_string());
    url
}

fn classify_category_page(html: &str, category_url: &Url) -> Result<CategoryPage> {
    let parsed = Html::parse_document(html);
    let judgment_view_selector = Selector::parse("main .view.view-judgments")
        .map_err(|_| anyhow!("invalid High Court judgment-view selector"))?;
    let resource_view_selector = Selector::parse("main .view.view-resources-publications")
        .map_err(|_| anyhow!("invalid High Court resource-view selector"))?;
    let judgment_views = parsed.select(&judgment_view_selector).collect::<Vec<_>>();
    let resource_views = parsed.select(&resource_view_selector).collect::<Vec<_>>();
    if judgment_views.len() > 1 || resource_views.len() > 1 {
        bail!("High Court category {category_url} has multiple official collection views");
    }
    if !judgment_views.is_empty() && !resource_views.is_empty() {
        bail!("High Court category {category_url} mixes judgment and resource collections");
    }

    if let Some(view) = judgment_views.into_iter().next() {
        let range = result_range(view, category_url)?;
        if range.total == 0 {
            bail!("High Court judgment category {category_url} reports no judgments");
        }
        if range.total > 20_000 {
            bail!("current High Court index exceeds its discovery bound");
        }
        let documents = parse_current_page(html, category_url)?;
        if documents.len() != range.displayed() {
            bail!(
                "High Court judgment listing {category_url} displays {} rows but parsed {} judgments",
                range.displayed(),
                documents.len()
            );
        }
        let last_page = last_page(view, category_url)?;
        if (range.total == 0 && last_page != 0) || (range.total > 0 && last_page >= range.total) {
            bail!("High Court category {category_url} has an implausible pager range");
        }
        return Ok(CategoryPage::JudgmentListing {
            total: range.total,
            last_page,
            range,
            documents,
        });
    }

    if let Some(view) = resource_views.into_iter().next() {
        validate_resource_collection(view, category_url)?;
        return Ok(CategoryPage::ResourceCollection);
    }

    bail!("High Court category {category_url} has an unrecognised official collection structure")
}

fn result_range(root: scraper::ElementRef<'_>, category_url: &Url) -> Result<ResultRange> {
    let selector = Selector::parse(".view-summary, .view-header")
        .map_err(|_| anyhow!("invalid High Court result-summary selector"))?;
    let expression =
        regex::Regex::new(r"^Displaying\s+([\d,]+)\s*-\s*([\d,]+)\s+of\s+([\d,]+)\s+results$")?;
    let ranges = root
        .select(&selector)
        .filter_map(|element| {
            let text = element
                .text()
                .collect::<Vec<_>>()
                .join(" ")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            let captures = expression.captures(&text)?;
            let parse = |index| {
                captures
                    .get(index)
                    .map(|value| value.as_str().replace(',', ""))?
                    .parse::<usize>()
                    .ok()
            };
            Some((parse(1)?, parse(2)?, parse(3)?))
        })
        .collect::<BTreeSet<_>>();
    if ranges.len() != 1 {
        bail!("High Court category {category_url} has no unique result range");
    }
    let (first, last, total) = ranges
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("High Court result range disappeared"))?;
    if (total == 0 && (first != 0 || last != 0))
        || (total > 0 && (first == 0 || last < first || last > total))
    {
        bail!("High Court category {category_url} has an invalid result range");
    }
    Ok(ResultRange { first, last, total })
}

fn last_page(root: scraper::ElementRef<'_>, category_url: &Url) -> Result<usize> {
    let selector = Selector::parse("a[href*='page=']")
        .map_err(|_| anyhow!("invalid current High Court pager selector"))?;
    let last_page = root
        .select(&selector)
        .filter_map(|element| element.value().attr("href"))
        .filter_map(|href| pagination_page(category_url, href))
        .max()
        .unwrap_or(0);
    if last_page > 2_000 {
        bail!("current High Court index exceeds its discovery bound");
    }
    Ok(last_page)
}

fn validate_resource_collection(view: scraper::ElementRef<'_>, category_url: &Url) -> Result<()> {
    let range = result_range(view, category_url)?;
    if range.total > 20_000 {
        bail!("High Court resource collection exceeds its discovery bound");
    }
    let item_selector = Selector::parse(".view-content .views-field-name")
        .map_err(|_| anyhow!("invalid High Court resource-item selector"))?;
    let link_selector = Selector::parse(".view-content .media-links a[href]")
        .map_err(|_| anyhow!("invalid High Court resource-link selector"))?;
    let empty_selector = Selector::parse(".view-empty")
        .map_err(|_| anyhow!("invalid High Court empty-resource selector"))?;
    let items = view.select(&item_selector).count();
    let links = view.select(&link_selector).count();
    if (range.total == 0 && view.select(&empty_selector).next().is_none())
        || (range.total > 0 && (items != range.displayed() || links < items))
    {
        bail!("High Court category {category_url} has a malformed resource collection");
    }
    Ok(())
}

fn pagination_page(category_url: &Url, href: &str) -> Option<usize> {
    let url = category_url.join(href).ok()?;
    if url.scheme() != "https"
        || url.host_str() != Some(HOST)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.path() != category_url.path()
        || url.fragment().is_some()
    {
        return None;
    }
    let pairs = url.query_pairs().collect::<Vec<_>>();
    if pairs.len() != 1 || pairs[0].0 != "page" {
        return None;
    }
    pairs[0].1.parse().ok()
}

fn parse_current_page(html: &str, category_url: &Url) -> Result<Vec<DiscoveredDocument>> {
    let parsed = Html::parse_document(html);
    let row_selector = Selector::parse(".views-row-item-judgement")
        .map_err(|_| anyhow!("invalid current High Court row selector"))?;
    let title_selector = Selector::parse(".field--title")
        .map_err(|_| anyhow!("invalid current High Court title selector"))?;
    let citation_selector = Selector::parse(".field--citation")
        .map_err(|_| anyhow!("invalid current High Court citation selector"))?;
    let date_selector = Selector::parse(".field--hca-date-issued")
        .map_err(|_| anyhow!("invalid current High Court date selector"))?;
    let mut documents = Vec::new();
    for row in parsed.select(&row_selector) {
        if row.value().name() != "a" {
            bail!("current High Court judgment row is not a link");
        }
        let href = row
            .value()
            .attr("href")
            .ok_or_else(|| anyhow!("current High Court row has no href"))?;
        let canonical = category_url.join(href)?;
        validate_judgment_url(category_url, &canonical)?;
        let canonical_url = canonical.to_string();
        let encoded_title = row
            .select(&title_selector)
            .next()
            .map(|element| element.text().collect::<String>())
            .map(|value| value.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("current High Court row has no title"))?;
        let title = Html::parse_fragment(&encoded_title)
            .root_element()
            .text()
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let citation = row
            .select(&citation_selector)
            .filter_map(|element| {
                let value = element.text().collect::<String>();
                let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
                value
                    .strip_prefix("Citation:")
                    .map(str::trim)
                    .map(str::to_owned)
            })
            .find(|value| !value.is_empty());
        let path = Url::parse(&canonical_url)?.path().to_owned();
        let unreported_slug = path
            .strip_prefix("/cases-and-judgments/judgments/unreported-judgments/")
            .filter(|value| !value.is_empty() && !value.contains('/'));
        let native_id = if let Some(slug) = unreported_slug {
            format!("unreported/{slug}")
        } else {
            citation.clone().ok_or_else(|| {
                anyhow!("reported High Court row has no official citation identity")
            })?
        };
        let date = row
            .select(&date_selector)
            .next()
            .map(|element| element.text().collect::<String>())
            .and_then(|value| parse_date(value.trim_start_matches("Date:").trim(), &["%d %b %Y"]));
        let upstream_version = sha256_bytes(
            format!(
                "{canonical_url}\n{title}\n{}\n{}",
                citation.as_deref().unwrap_or_default(),
                date.as_deref().unwrap_or_default()
            )
            .as_bytes(),
        );
        documents.push(DiscoveredDocument {
            native_id,
            upstream_version,
            title: citation
                .as_ref()
                .map_or_else(|| title.clone(), |citation| format!("{title} {citation}")),
            document_type: "decision".to_owned(),
            date,
            citation,
            canonical_url: canonical_url.clone(),
            renditions: vec![Rendition {
                url: canonical_url,
                kind: RenditionKind::Html,
            }],
        });
    }
    Ok(documents)
}

fn validate_judgment_url(category_url: &Url, judgment_url: &Url) -> Result<()> {
    let expected_prefix = format!("{}/", category_url.path());
    let Some(slug) = judgment_url.path().strip_prefix(&expected_prefix) else {
        bail!("High Court judgment URL escaped its category path");
    };
    if judgment_url.scheme() != "https"
        || judgment_url.host_str() != Some(HOST)
        || !judgment_url.username().is_empty()
        || judgment_url.password().is_some()
        || judgment_url.port().is_some()
        || judgment_url.query().is_some()
        || judgment_url.fragment().is_some()
        || slug.is_empty()
        || slug.contains('/')
        || !slug
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        bail!("High Court judgment URL is outside the allowlisted path shape");
    }
    Ok(())
}

fn validate_judgment_inventory(documents: &[DiscoveredDocument]) -> Result<()> {
    if documents.is_empty() || documents.len() > MAX_JUDGMENTS {
        bail!("High Court judgment inventory is empty or exceeds its bound");
    }
    let mut identities = BTreeSet::new();
    for document in documents {
        if !identities.insert(document.native_id.as_str()) {
            bail!(
                "High Court judgment inventory contains duplicate {}",
                document.native_id
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const INDEX_FIXTURE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/high-court/judgments-index.html"
    ));
    const CURRENT_JUDGMENTS_FIXTURE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/high-court/judgments-1998-current.html"
    ));
    const SINGLE_JUSTICE_FIXTURE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/high-court/single-justice-judgments.html"
    ));
    const CLR_FIXTURE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/high-court/1-clr-100-clr.html"
    ));
    const UNREPORTED_FIXTURE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/high-court/unreported-judgments.html"
    ));
    const SUMMARIES_FIXTURE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/high-court/judgment-summaries.html"
    ));
    const EMPTY_JUDGMENTS_FIXTURE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/high-court/empty-judgment-listing.html"
    ));
    const MALFORMED_JUDGMENTS_FIXTURE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/high-court/malformed-judgment-listing.html"
    ));

    fn category_url(slug: &str) -> Result<Url> {
        Url::parse(&format!(
            "https://{HOST}{JUDGMENT_CATEGORY_PATH_PREFIX}{slug}"
        ))
        .map_err(Into::into)
    }

    #[test]
    fn published_categories_are_dynamic_deduplicated_and_preserve_the_official_coverage_gap(
    ) -> Result<()> {
        let categories = parse_judgment_category_urls(INDEX_FIXTURE)?;
        // The official landing shape has no reported collection for 1960-1997,
        // and its published unreported category is explicitly incomplete.
        assert_eq!(
            categories.iter().map(|url| url.path()).collect::<Vec<_>>(),
            vec![
                "/cases-and-judgments/judgments/1-clr-100-clr",
                "/cases-and-judgments/judgments/judgments-1998-current",
                "/cases-and-judgments/judgments/single-justice-judgments",
                "/cases-and-judgments/judgments/unreported-judgments",
            ]
        );
        assert!(categories
            .iter()
            .all(|url| !url.path().contains("judgment-summaries")));

        let neutral_title = INDEX_FIXTURE.replace(
            "</main>",
            r#"<div class="field--name-field-title"><a href="/cases-and-judgments/judgments/decisions-archive">Decisions archive</a></div></main>"#,
        );
        assert!(parse_judgment_category_urls(&neutral_title)?
            .iter()
            .any(|url| url.path().ends_with("/decisions-archive")));

        assert!(parse_judgment_category_urls_with_limit(INDEX_FIXTURE, 3).is_err());
        Ok(())
    }

    #[test]
    fn dynamic_discovery_traverses_every_published_judgment_category_once() -> Result<()> {
        let visits = std::sync::Mutex::new(BTreeMap::<String, usize>::new());
        let documents = discover_from_index(INDEX_FIXTURE, 2, |url| {
            *visits
                .lock()
                .map_err(|_| anyhow!("fixture visit lock is poisoned"))?
                .entry(url.path().to_owned())
                .or_default() += 1;
            match url.path() {
                "/cases-and-judgments/judgments/judgments-1998-current" => {
                    Ok(CURRENT_JUDGMENTS_FIXTURE.to_owned())
                }
                "/cases-and-judgments/judgments/single-justice-judgments" => {
                    Ok(SINGLE_JUSTICE_FIXTURE.to_owned())
                }
                "/cases-and-judgments/judgments/1-clr-100-clr" => Ok(CLR_FIXTURE.to_owned()),
                "/cases-and-judgments/judgments/unreported-judgments" => {
                    Ok(UNREPORTED_FIXTURE.to_owned())
                }
                path => bail!("unexpected fixture category {path}"),
            }
        })?;
        let identities = documents
            .iter()
            .map(|document| document.native_id.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            identities,
            BTreeSet::from([
                "[1903] HCA 1",
                "[2026] HCA 22",
                "[2026] HCASJ 21",
                "unreported/robert-clive-fordham-and-state-victoria-v-gareth-evans",
            ])
        );
        assert_eq!(
            visits.into_inner().expect("fixture visit lock is healthy"),
            BTreeMap::from([
                ("/cases-and-judgments/judgments/1-clr-100-clr".to_owned(), 1,),
                (
                    "/cases-and-judgments/judgments/judgments-1998-current".to_owned(),
                    1,
                ),
                (
                    "/cases-and-judgments/judgments/single-justice-judgments".to_owned(),
                    1,
                ),
                (
                    "/cases-and-judgments/judgments/unreported-judgments".to_owned(),
                    1,
                ),
            ])
        );
        Ok(())
    }

    #[test]
    fn category_structure_distinguishes_other_collections_and_fails_closed() -> Result<()> {
        let category = category_url("judgments-1998-current")?;
        assert!(matches!(
            classify_category_page(SUMMARIES_FIXTURE, &category)?,
            CategoryPage::ResourceCollection
        ));
        assert!(classify_category_page(EMPTY_JUDGMENTS_FIXTURE, &category).is_err());

        let index = r#"<main><div class="field--name-field-title"><a href="/cases-and-judgments/judgments/judgments-1998-current">Judgments</a></div></main>"#;
        let error = discover_from_index(index, 1, |_| Ok(MALFORMED_JUDGMENTS_FIXTURE.to_owned()))
            .expect_err("a non-empty malformed judgment listing must fail discovery");
        assert!(error.to_string().contains("displays 1 rows but parsed 0"));

        let changed_row_class =
            CURRENT_JUDGMENTS_FIXTURE.replace("views-row-item-judgement", "judgment-row");
        assert!(classify_category_page(&changed_row_class, &category).is_err());
        Ok(())
    }

    #[test]
    fn duplicate_accessible_result_summaries_must_agree() -> Result<()> {
        let category = category_url("judgments-1998-current")?;
        let html = r#"
            <main><div class="view view-judgments">
              <div class="view-summary">Displaying 1 - 12 of 1,658 results</div>
              <div class="view-header"><span>Displaying 1 - 12 of 1,658 results</span></div>
            </div></main>
        "#;
        let parsed = Html::parse_document(html);
        let selector = Selector::parse("main .view.view-judgments")
            .map_err(|_| anyhow!("invalid test selector"))?;
        let range = result_range(
            parsed
                .select(&selector)
                .next()
                .ok_or_else(|| anyhow!("missing test judgment view"))?,
            &category,
        )?;
        assert_eq!((range.first, range.last, range.total), (1, 12, 1_658));

        let conflicting = html.replace(
            "<span>Displaying 1 - 12 of 1,658 results</span>",
            "<span>Displaying 1 - 12 of 1,659 results</span>",
        );
        let parsed = Html::parse_document(&conflicting);
        let error = result_range(
            parsed
                .select(&selector)
                .next()
                .ok_or_else(|| anyhow!("missing conflicting test judgment view"))?,
            &category,
        )
        .expect_err("conflicting result summaries must fail closed");
        assert!(error.to_string().contains("no unique result range"));
        Ok(())
    }

    #[test]
    fn judgment_inventory_rejects_duplicates_and_inexact_category_totals() -> Result<()> {
        let mut duplicated = parse_current_page(
            CURRENT_JUDGMENTS_FIXTURE,
            &category_url("judgments-1998-current")?,
        )?;
        duplicated.extend(parse_current_page(
            SINGLE_JUSTICE_FIXTURE,
            &category_url("single-justice-judgments")?,
        )?);
        duplicated.push(duplicated[0].clone());
        let error =
            validate_judgment_inventory(&duplicated).expect_err("duplicate identity must fail");
        assert!(error.to_string().contains("duplicate [2026] HCA 22"));

        let index = r#"<main><div class="field--name-field-title"><a href="/cases-and-judgments/judgments/judgments-1998-current">Judgments</a></div></main>"#;
        let malformed_total = CURRENT_JUDGMENTS_FIXTURE.replace("of 1 results", "of 2 results");
        let error = discover_from_index(index, 1, |_| Ok(malformed_total.clone()))
            .expect_err("category inventory total must be exact");
        assert!(error
            .to_string()
            .contains("does not cover its complete result range"));

        let mut first_page = CURRENT_JUDGMENTS_FIXTURE.replace(
            "Displaying 1 - 1 of 1 results",
            "Displaying 1 - 1 of 2 results",
        );
        let closing_view = first_page
            .rfind("</div>")
            .ok_or_else(|| anyhow!("test judgment view has no closing element"))?;
        first_page.insert_str(closing_view, "<a href=\"?page=1\">Last page</a>");
        let second_page = first_page
            .replace(
                "chaplin-v-secretary-department-social-services",
                "second-v-commonwealth",
            )
            .replace(
                "Chaplin v Secretary, Department of Social Services",
                "Second v Commonwealth",
            )
            .replace("[2026] HCA 22", "[2026] HCA 23");
        let error = discover_from_index(index, 1, |url| {
            if url
                .query_pairs()
                .any(|(name, value)| name == "page" && value == "1")
            {
                Ok(second_page.clone())
            } else {
                Ok(first_page.clone())
            }
        })
        .expect_err("overlapping category ranges must fail closed");
        assert!(
            error
                .to_string()
                .contains("overlapping or incomplete result-range partition"),
            "unexpected discovery error: {error:#}"
        );
        Ok(())
    }

    #[test]
    fn judgment_and_pagination_links_cannot_escape_the_category_path() -> Result<()> {
        let category = category_url("judgments-1998-current")?;
        assert_eq!(pagination_page(&category, "?page=7"), Some(7));
        assert_eq!(
            pagination_page(
                &category,
                "https://www.hcourt.gov.au/cases-and-judgments/judgments/single-justice-judgments?page=7"
            ),
            None
        );
        assert_eq!(
            pagination_page(&category, "https://evil.example/?page=7"),
            None
        );
        assert_eq!(pagination_page(&category, "?page=7&sort=title"), None);

        let escaped = CURRENT_JUDGMENTS_FIXTURE.replace(
            "/cases-and-judgments/judgments/judgments-1998-current/chaplin-v-secretary-department-social-services",
            "/cases-and-judgments/judgments/single-justice-judgments/chaplin-v-secretary-department-social-services",
        );
        assert!(parse_current_page(&escaped, &category).is_err());

        let redirected = Url::parse(
            "https://www.hcourt.gov.au/cases-and-judgments/judgments/single-justice-judgments",
        )?;
        assert!(validate_discovery_response_url(&category, &redirected).is_err());
        let narrowed = Url::parse(&format!("{category}?year=2026"))?;
        assert!(validate_discovery_response_url(&category, &narrowed).is_err());
        assert!(validate_exact_hca_response_url(category.as_str(), &redirected).is_err());
        Ok(())
    }

    #[test]
    fn current_hca_listing_uses_neutral_citation_identity() -> Result<()> {
        let html = r#"
            <main><div class="view view-judgments">
              <div class="view-summary">Displaying 1 - 1 of 13 results</div>
              <div class="view-content">
                <a class="views-row-item views-row-item-judgement"
                   href="/cases-and-judgments/judgments/judgments-1998-current/example">
                  <div class="field--title">Example v Commonwealth</div>
                  <div class="field--citation"><strong>Citation:</strong> [2026] HCA 1</div>
                  <div class="field--hca-date-issued"><strong>Date:</strong> 1 Jan 2026</div>
                </a>
              </div>
              <a href="?page=1">Last page</a>
            </div></main>
        "#;
        let category = category_url("judgments-1998-current")?;
        let CategoryPage::JudgmentListing {
            total,
            last_page,
            documents,
            ..
        } = classify_category_page(html, &category)?
        else {
            bail!("judgment fixture was misclassified as a resource collection")
        };
        assert_eq!((total, last_page), (13, 1));
        assert_eq!(documents.len(), 1);
        assert_eq!(documents[0].native_id, "[2026] HCA 1");
        assert_eq!(documents[0].date.as_deref(), Some("2026-01-01"));
        Ok(())
    }

    #[test]
    fn historical_clr_listing_prefers_neutral_over_clr_citation() -> Result<()> {
        let html = r#"
            <div class="view-summary">Displaying 1 - 1 of 1 results</div>
            <a class="views-row-item views-row-item-judgement"
               href="/cases-and-judgments/judgments/1-clr-100-clr/dalgarno-v-hannah">
              <div class="field field--title">Dalgarno &amp;amp; Hannah</div>
              <div class="field field--citation"><strong>CLR citation:</strong> 1 CLR 1</div>
              <div class="field field--citation"><strong>Citation:</strong> [1903] HCA 1</div>
              <div class="field field--hca-date-issued"><strong>Date:</strong> 11 Nov 1903</div>
            </a>
        "#;
        let documents = parse_current_page(html, &category_url("1-clr-100-clr")?)?;
        assert_eq!(documents[0].native_id, "[1903] HCA 1");
        assert_eq!(documents[0].citation.as_deref(), Some("[1903] HCA 1"));
        assert_eq!(documents[0].title, "Dalgarno & Hannah [1903] HCA 1");
        Ok(())
    }

    #[test]
    fn unreported_listing_uses_the_official_path_slug_identity() -> Result<()> {
        let html = r#"
            <div class="view-summary">Displaying 1 - 1 of 1 results</div>
            <a class="views-row-item views-row-item-judgement"
               href="/cases-and-judgments/judgments/unreported-judgments/jones-v-cusack">
              <div class="field field--title">Jones v Cusack</div>
              <div class="field field--citation"><strong>Citation:</strong> 1/1923</div>
              <div class="field field--hca-date-issued"><strong>Date:</strong> 19 Apr 1993</div>
            </a>
        "#;
        let documents = parse_current_page(html, &category_url("unreported-judgments")?)?;
        assert_eq!(documents[0].native_id, "unreported/jones-v-cusack");
        assert_eq!(documents[0].citation.as_deref(), Some("1/1923"));
        Ok(())
    }
}
