use super::*;
use serde::Deserialize;

pub(super) static ADAPTER: NswCaselaw = NswCaselaw;

pub(super) struct NswCaselaw;

const HOST: &str = "www.caselaw.nsw.gov.au";
const BROWSE_URL: &str = "https://www.caselaw.nsw.gov.au/browse?display=all";
const PAGE_SIZE: usize = 200;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DecisionPage {
    searchable_decisions: Vec<DecisionRecord>,
    total_elements: usize,
    total_pages: usize,
    size: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DecisionRecord {
    id: String,
    title: Option<String>,
    mnc: String,
    decision_date_text: Option<String>,
    last_published_date: Option<i64>,
    decision_date: Option<i64>,
    #[serde(default)]
    amendment: bool,
    restricted: bool,
}

impl OfficialAdapter for NswCaselaw {
    fn source_id(&self) -> &'static str {
        NSW_CASELAW_SOURCE_ID
    }

    fn display_name(&self) -> &'static str {
        "NSW Caselaw"
    }

    fn approved_hosts(&self) -> &'static [&'static str] {
        &[HOST]
    }

    fn rate_policy(&self) -> SourceRatePolicy {
        SourceRatePolicy {
            minimum_request_interval_ms: 0,
            request_timeout_seconds: 45,
        }
    }

    fn discover(
        &self,
        client: &OfficialHttpClient,
        _mode: SourceUpdateMode,
    ) -> Result<Vec<DiscoveredDocument>> {
        client.get_required(BROWSE_URL, "text/html", MAX_INDEX_BYTES)?;
        let mut snapshot = None;
        for attempt in 1..=3 {
            match discover_decision_snapshot(client, SOURCE_WORKER_CEILING) {
                Ok(records) => {
                    snapshot = Some(records);
                    break;
                }
                Err(error) if attempt < 3 => {
                    eprintln!(
                        "legal-mcp source nsw-caselaw: unstable snapshot attempt {attempt}: {error:#}"
                    );
                    thread::sleep(Duration::from_secs(2));
                }
                Err(error) => return Err(error),
            }
        }
        let records = snapshot.ok_or_else(|| anyhow!("NSW Caselaw snapshot was not produced"))?;
        let mut documents = Vec::new();
        for record in records {
            if record.restricted {
                continue;
            }
            let title = record.title.unwrap_or_default();
            let normalized_title = title.split_whitespace().collect::<Vec<_>>().join(" ");
            let lowered = normalized_title.to_ascii_lowercase();
            if lowered.contains("decision number not in use")
                || lowered.contains("decision restricted")
            {
                continue;
            }
            let citation = if normalized_title.is_empty() {
                record.mnc.trim().to_owned()
            } else {
                format!("{} {}", normalized_title, record.mnc.trim())
            };
            let date = record
                .decision_date_text
                .as_deref()
                .and_then(|value| parse_date(value, &["%d %B %Y"]));
            let canonical_url = format!("https://{HOST}/decision/{}", record.id);
            let upstream_version = sha256_bytes(
                format!(
                    "{}\n{}\n{}\n{}\n{}\n{}",
                    record.id,
                    record.last_published_date.unwrap_or_default(),
                    record.decision_date.unwrap_or_default(),
                    record.amendment,
                    normalized_title,
                    record.mnc
                )
                .as_bytes(),
            );
            documents.push(DiscoveredDocument {
                native_id: record.id.clone(),
                upstream_version,
                title: citation.clone(),
                document_type: "decision".to_owned(),
                date,
                citation: Some(citation),
                canonical_url: canonical_url.clone(),
                renditions: vec![Rendition {
                    url: canonical_url,
                    kind: RenditionKind::Html,
                }],
            });
        }
        Ok(documents)
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
            bail!("NSW Caselaw decision returned HTTP {}", landing.status);
        }
        let html = decode_utf8(&landing.bytes)?;
        let parsed = Html::parse_document(&html);
        let attachment_selector = Selector::parse("a[href^='/asset/']")
            .map_err(|_| anyhow!("invalid NSW attachment selector"))?;
        let pdf_url = parsed.select(&attachment_selector).find_map(|element| {
            let label = element.text().collect::<String>();
            label
                .contains("Attachment (PDF)")
                .then(|| element.value().attr("href"))
                .flatten()
                .and_then(|href| Url::parse(&entry.canonical_url).ok()?.join(href).ok())
                .map(|url| url.to_string())
        });
        if let Some(pdf_url) = pdf_url {
            let pdf = client.get_required(&pdf_url, "application/pdf", MAX_DOCUMENT_BYTES)?;
            let normalized = normalize_pdf(&pdf.bytes)?;
            return Ok(Some(make_acquired_text(
                normalized,
                entry.canonical_url.clone(),
            )));
        }
        let normalized = normalize_html(
            &html,
            &entry.canonical_url,
            HtmlRules {
                content_selector: "div.judgment",
                drop_ids: &[],
                drop_classes: &["decision-actions"],
                heading_classes: &[],
            },
        )?;
        Ok(Some(make_acquired_html(
            normalized,
            entry.canonical_url.clone(),
        )))
    }
}

fn discover_decision_snapshot(
    client: &OfficialHttpClient,
    concurrency: usize,
) -> Result<Vec<DecisionRecord>> {
    let first_url = format!("https://{HOST}/browse/list?page=0");
    let first_payload = client.get_required(&first_url, "application/json", MAX_INDEX_BYTES)?;
    let first: DecisionPage =
        serde_json::from_slice(&first_payload.bytes).context("decoding NSW Caselaw page 0")?;
    if first.size != PAGE_SIZE || first.total_pages != first.total_elements.div_ceil(PAGE_SIZE) {
        bail!("NSW Caselaw first page has inconsistent bounds");
    }
    let total = first.total_elements;
    let pages = first.total_pages;
    let remaining = (1..pages).collect::<Vec<_>>();
    let subsequent = parallel_map(concurrency, remaining, |page_number| {
        let url = format!("https://{HOST}/browse/list?page={page_number}");
        let payload = client.get_required(&url, "application/json", MAX_INDEX_BYTES)?;
        let page: DecisionPage = serde_json::from_slice(&payload.bytes)
            .with_context(|| format!("decoding NSW Caselaw page {page_number}"))?;
        if page.size != PAGE_SIZE || page.total_elements != total || page.total_pages != pages {
            bail!(
                "NSW Caselaw page {page_number} changed bounds from {total}/{pages} to {}/{}",
                page.total_elements,
                page.total_pages
            );
        }
        Ok(page.searchable_decisions)
    })?;
    let mut records = first.searchable_decisions;
    records.extend(subsequent.into_iter().flatten());
    if records.len() != total {
        bail!(
            "NSW Caselaw snapshot expected {total} decisions but returned {}",
            records.len()
        );
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_page_filters_restricted_and_versions_amendments() -> Result<()> {
        let page: DecisionPage = serde_json::from_str(
            r#"{
          "searchableDecisions":[{"id":"abc123","title":"Example v State","mnc":"[2026] NSWSC 1","decisionDateText":"1 July 2026","lastPublishedDate":1234,"restricted":false}],
          "totalElements":1,"totalPages":1,"size":200
        }"#,
        )?;
        assert_eq!(page.searchable_decisions[0].last_published_date, Some(1234));
        assert!(!page.searchable_decisions[0].restricted);
        Ok(())
    }
}
