use super::*;

pub(super) static ADAPTER: HighCourt = HighCourt;
pub(super) struct HighCourt;

const HOST: &str = "www.hcourt.gov.au";
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
    let bases = [
        format!("https://{HOST}/cases-and-judgments/judgments/judgments-1998-current"),
        format!("https://{HOST}/cases-and-judgments/judgments/single-justice-judgments"),
        format!("https://{HOST}/cases-and-judgments/judgments/1-clr-100-clr"),
        format!("https://{HOST}/cases-and-judgments/judgments/unreported-judgments"),
    ];
    let first_pages = parallel_map(concurrency, bases.to_vec(), |base| {
        let payload = client.get_required(&base, "text/html", MAX_INDEX_BYTES)?;
        let html = decode_utf8(&payload.bytes)?;
        let (total, last_page) = current_page_bounds(&html)?;
        Ok((base, total, last_page, html))
    })?;
    let mut page_requests = Vec::new();
    let mut documents = Vec::new();
    let mut expected_total = 0usize;
    for (base, total, last_page, first_html) in first_pages {
        expected_total += total;
        documents.extend(parse_current_page(&first_html)?);
        for page in 1..=last_page {
            page_requests.push(format!("{base}?page={page}"));
        }
    }
    let pages = parallel_map(concurrency, page_requests, |url| {
        let payload = client.get_required(&url, "text/html", MAX_INDEX_BYTES)?;
        parse_current_page(&decode_utf8(&payload.bytes)?)
    })?;
    documents.extend(pages.into_iter().flatten());
    if documents.len() != expected_total {
        bail!(
            "current High Court discovery expected {expected_total} judgments but parsed {}",
            documents.len()
        );
    }
    Ok(documents)
}

fn current_page_bounds(html: &str) -> Result<(usize, usize)> {
    let total = regex::Regex::new(r"Displaying\s+\d+\s+-\s+\d+\s+of\s+([\d,]+)\s+results")?
        .captures(html)
        .and_then(|captures| captures.get(1))
        .map(|value| value.as_str().replace(',', ""))
        .ok_or_else(|| anyhow!("current High Court index has no result total"))?
        .parse::<usize>()?;
    let parsed = Html::parse_document(html);
    let selector = Selector::parse("a[href*='page=']")
        .map_err(|_| anyhow!("invalid current High Court pager selector"))?;
    let last_page = parsed
        .select(&selector)
        .filter_map(|element| element.value().attr("href"))
        .filter_map(|href| Url::parse("https://example.invalid/").ok()?.join(href).ok())
        .flat_map(|url| {
            url.query_pairs()
                .filter_map(|(key, value)| {
                    (key == "page")
                        .then(|| value.parse::<usize>().ok())
                        .flatten()
                })
                .collect::<Vec<_>>()
        })
        .max()
        .unwrap_or(0);
    if total > 20_000 || last_page > 2_000 {
        bail!("current High Court index exceeds its discovery bound");
    }
    Ok((total, last_page))
}

fn parse_current_page(html: &str) -> Result<Vec<DiscoveredDocument>> {
    let parsed = Html::parse_document(html);
    let row_selector = Selector::parse("a.views-row-item-judgement[href]")
        .map_err(|_| anyhow!("invalid current High Court row selector"))?;
    let title_selector = Selector::parse(".field--title")
        .map_err(|_| anyhow!("invalid current High Court title selector"))?;
    let citation_selector = Selector::parse(".field--citation")
        .map_err(|_| anyhow!("invalid current High Court citation selector"))?;
    let date_selector = Selector::parse(".field--hca-date-issued")
        .map_err(|_| anyhow!("invalid current High Court date selector"))?;
    let mut documents = Vec::new();
    for row in parsed.select(&row_selector) {
        let href = row
            .value()
            .attr("href")
            .ok_or_else(|| anyhow!("current High Court row has no href"))?;
        let canonical_url = Url::parse(&format!("https://{HOST}"))?
            .join(href)?
            .to_string();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_hca_listing_uses_neutral_citation_identity() -> Result<()> {
        let html = r#"
            <div class="view-header">Displaying 1 - 1 of 13 results</div>
            <a class="views-row-item views-row-item-judgement"
               href="/cases-and-judgments/judgments/judgments-1998-current/example">
              <div class="field--title">Example v Commonwealth</div>
              <div class="field--citation"><strong>Citation:</strong> [2026] HCA 1</div>
              <div class="field--hca-date-issued"><strong>Date:</strong> 1 Jan 2026</div>
            </a>
            <a href="?page=1">Last page</a>
        "#;
        assert_eq!(current_page_bounds(html)?, (13, 1));
        let documents = parse_current_page(html)?;
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
        let documents = parse_current_page(html)?;
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
        let documents = parse_current_page(html)?;
        assert_eq!(documents[0].native_id, "unreported/jones-v-cusack");
        assert_eq!(documents[0].citation.as_deref(), Some("1/1923"));
        Ok(())
    }
}
