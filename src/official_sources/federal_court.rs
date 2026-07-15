use super::*;

pub(super) static ADAPTER: FederalCourt = FederalCourt;
pub(super) struct FederalCourt;

const SEARCH_HOST: &str = "search.judgments.fedcourt.gov.au";
const JUDGMENTS_HOST: &str = "www.judgments.fedcourt.gov.au";
const FILE_STORE_HOST: &str = "www.fedcourt.gov.au";
const PAGE_SIZE: usize = 1_000;
const BASE_URL: &str = "https://search.judgments.fedcourt.gov.au/s/search.html?collection=fca%7Esp-judgments-internet&profile=judgments-internet&sort=adate&meta_CourtID_orsand=FCA+FCAFC+IRCA+ACOMPT+ACOPYT+ADFDAT+FPDT+NFSC&meta_MNC=&meta_Judge=&meta_Reported=&meta_FileNumber=&meta_NPA_phrase_orsand=&query_sand=&query_or=&query_not=&query_phrase=&query_prox=&meta_d=&meta_d1=&meta_d2=&meta_Legislation=&meta_CasesCited=&meta_Catchwords=&";

impl OfficialAdapter for FederalCourt {
    fn source_id(&self) -> &'static str {
        FEDERAL_COURT_SOURCE_ID
    }
    fn display_name(&self) -> &'static str {
        "Federal Court of Australia judgments"
    }
    fn approved_hosts(&self) -> &'static [&'static str] {
        &[SEARCH_HOST, JUDGMENTS_HOST, FILE_STORE_HOST]
    }
    fn rate_policy(&self) -> SourceRatePolicy {
        SourceRatePolicy {
            minimum_request_interval_ms: 0,
            request_timeout_seconds: 60,
        }
    }

    fn minimum_request_interval_ms(&self, url: &Url) -> u64 {
        if url.host_str() == Some(SEARCH_HOST) {
            1_200
        } else {
            0
        }
    }
    fn use_browser_transport(&self, url: &Url) -> bool {
        matches!(url.host_str(), Some(JUDGMENTS_HOST | FILE_STORE_HOST))
    }

    fn discover(
        &self,
        client: &OfficialHttpClient,
        _mode: SourceUpdateMode,
    ) -> Result<Vec<DiscoveredDocument>> {
        let first_url = format!("{BASE_URL}num_ranks=1");
        let first = client.get_required(&first_url, "text/html", MAX_INDEX_BYTES)?;
        let first_html = decode_federal_court_html(&first.bytes);
        let alleged_total = result_total(&first_html)?;
        let final_url = format!("{BASE_URL}num_ranks=1&start_rank={alleged_total}");
        let final_page = client.get_required(&final_url, "text/html", MAX_INDEX_BYTES)?;
        let final_html = decode_federal_court_html(&final_page.bytes);
        let total = result_total(&final_html)?.max(alleged_total);
        let starts = (0..total.div_ceil(PAGE_SIZE))
            .map(|page| page * PAGE_SIZE + 1)
            .collect::<Vec<_>>();
        let pages = parallel_map(SOURCE_WORKER_CEILING, starts, |start| {
            let url = format!("{BASE_URL}num_ranks={PAGE_SIZE}&start_rank={start}");
            let payload = client.get_required(&url, "text/html", MAX_INDEX_BYTES)?;
            let html = decode_federal_court_html(&payload.bytes);
            parse_search_page(&html)
        })?;
        let documents = pages.into_iter().flatten().collect::<Vec<_>>();
        if documents.len().saturating_mul(100) < total.saturating_mul(99) {
            bail!(
                "Federal Court discovery expected {total} judgments but parsed only {}",
                documents.len()
            );
        }
        if documents.len() != total {
            eprintln!(
                "legal-mcp source federal-court: retained {} of {total} official search records with stable judgment links",
                documents.len()
            );
        }
        Ok(documents)
    }

    fn acquire(
        &self,
        client: &OfficialHttpClient,
        entry: &DiscoveredDocument,
    ) -> Result<Option<AcquiredDocument>> {
        let payload = client.get(
            &entry.renditions[0].url,
            "text/html, application/pdf",
            MAX_DOCUMENT_BYTES,
        )?;
        if payload.status == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !payload.status.is_success() {
            bail!("Federal Court judgment returned HTTP {}", payload.status);
        }
        if payload.content_type.as_deref() == Some("application/pdf") {
            let html = normalize_pdf(&payload.bytes)?;
            return Ok(Some(make_acquired_text(html, entry.canonical_url.clone())));
        }
        let html = decode_federal_court_html(&payload.bytes);
        let parsed = Html::parse_document(&html);
        let content_selector = Selector::parse("div.judgment_content")
            .map_err(|_| anyhow!("invalid Federal Court judgment selector"))?;
        let judgment_content = parsed.select(&content_selector).next();
        let word_selector = Selector::parse("a[href]")
            .map_err(|_| anyhow!("invalid Federal Court link selector"))?;
        let word_url = parsed.select(&word_selector).find_map(|element| {
            let label = element.text().collect::<String>();
            label
                .contains("Original Word Document")
                .then(|| element.value().attr("href"))
                .flatten()
                .and_then(|href| Url::parse(&entry.canonical_url).ok()?.join(href).ok())
                .map(|url| url.to_string())
        });
        let judgment_has_encoding_damage = judgment_content
            .as_ref()
            .is_some_and(|element| element.text().any(source_text_has_encoding_damage));
        if judgment_content.as_ref().is_some_and(|element| {
            element
                .text()
                .any(|text| !text.trim_matches(|ch: char| ch.is_whitespace()).is_empty())
        }) && !judgment_has_encoding_damage
        {
            let normalized = normalize_html(
                &html,
                &entry.canonical_url,
                HtmlRules {
                    content_selector: "div.judgment_content",
                    drop_ids: &[],
                    drop_classes: &[],
                    heading_classes: &[],
                },
            )?;
            return Ok(Some(make_acquired_html(
                normalized,
                entry.canonical_url.clone(),
            )));
        }
        if let Some(word_url) = word_url {
            let word = client.get_required(
                &word_url,
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
                MAX_DOCUMENT_BYTES,
            )?;
            let (normalized, assets) = if word.bytes.starts_with(b"PK\x03\x04") {
                let source: SourceId = self.source_id().parse()?;
                crate::frl::normalize_docx_for_source(&word.bytes, &source, &entry.native_id)?
            } else if word.bytes.starts_with(&[0xd0, 0xcf, 0x11, 0xe0])
                || word
                    .bytes
                    .iter()
                    .position(|byte| !byte.is_ascii_whitespace())
                    .and_then(|start| word.bytes.get(start..))
                    .is_some_and(|bytes| bytes.starts_with(b"{\\rtf"))
            {
                (
                    normalize_rtf(&word.bytes, &entry.canonical_url)?,
                    Vec::new(),
                )
            } else if word.bytes.starts_with(b"%PDF") {
                (normalize_pdf(&word.bytes)?, Vec::new())
            } else {
                bail!("Federal Court Word rendition has an unrecognised file signature")
            };
            return Ok(Some(AcquiredDocument {
                html: normalized,
                assets,
                date: None,
                canonical_url: entry.canonical_url.clone(),
            }));
        }
        if html.contains("Document could not be found") {
            return Ok(None);
        }
        if judgment_has_encoding_damage {
            bail!(
                "Federal Court judgment {} contains encoding damage and has no official Word rendition",
                entry.native_id
            );
        }
        if judgment_content.is_some() {
            return Ok(None);
        }
        bail!(
            "Federal Court judgment {} has neither judgment HTML nor an official document rendition",
            entry.native_id
        )
    }
}

fn source_text_has_encoding_damage(text: &str) -> bool {
    text.contains('\u{fffd}') || text.contains("ï¿½") || text.contains("â€") || text.contains("Â ")
}

fn decode_federal_court_html(bytes: &[u8]) -> String {
    decode_utf8(bytes).unwrap_or_else(|_| decode_windows_1252(bytes))
}

fn result_total(html: &str) -> Result<usize> {
    let text = Html::parse_document(html)
        .root_element()
        .text()
        .collect::<Vec<_>>()
        .join(" ");
    regex::Regex::new(r"\bof\s+([\d,]+)")?
        .captures_iter(&text)
        .filter_map(|captures| captures.get(1))
        .filter_map(|value| value.as_str().replace(',', "").parse::<usize>().ok())
        .max()
        .ok_or_else(|| anyhow!("Federal Court search page has no result count"))
}

fn parse_search_page(html: &str) -> Result<Vec<DiscoveredDocument>> {
    let parsed_html = Html::parse_document(html);
    let row_selector =
        Selector::parse("div.result").map_err(|_| anyhow!("invalid Federal Court row selector"))?;
    let link_selector = Selector::parse("h3 a[href]")
        .map_err(|_| anyhow!("invalid Federal Court judgment selector"))?;
    let meta_selector = Selector::parse("p.meta")
        .map_err(|_| anyhow!("invalid Federal Court metadata selector"))?;
    let mut documents = Vec::new();
    for row in parsed_html.select(&row_selector) {
        let Some(link) = row.select(&link_selector).next() else {
            continue;
        };
        let Some(url) = link.value().attr("href") else {
            continue;
        };
        let parsed = Url::parse(url)?;
        if parsed.host_str() != Some(JUDGMENTS_HOST) {
            continue;
        }
        let title = link
            .value()
            .attr("title")
            .map(str::to_owned)
            .unwrap_or_else(|| link.text().collect::<String>())
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if title.is_empty() {
            continue;
        }
        let marker = "/judgments/Judgments/";
        let path = parsed.path();
        let native_id = path
            .split_once(marker)
            .map(|(_, tail)| tail)
            .unwrap_or(path.trim_start_matches('/'))
            .split('.')
            .next()
            .unwrap_or_default()
            .to_owned();
        if native_id.is_empty() {
            continue;
        }
        let date_text = row
            .select(&meta_selector)
            .next()
            .and_then(|element| element.text().next())
            .unwrap_or_default();
        let date = parse_date(date_text.trim(), &["%d %b %Y"]).filter(|value| {
            value
                .get(..4)
                .and_then(|year| year.parse::<u16>().ok())
                .is_some_and(|year| year >= 1976)
        });
        let jurisdiction = if native_id.starts_with("nfsc/") {
            "norfolk_island_decision"
        } else {
            "decision"
        };
        documents.push(DiscoveredDocument {
            native_id: native_id.clone(),
            upstream_version: sha256_bytes(
                format!("{url}\n{title}\n{}", date.as_deref().unwrap_or_default()).as_bytes(),
            ),
            title: title.clone(),
            document_type: jurisdiction.to_owned(),
            date,
            citation: Some(title),
            canonical_url: url.to_owned(),
            renditions: vec![Rendition {
                url: url.to_owned(),
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
    fn federal_search_page_extracts_stable_judgment_identity() -> Result<()> {
        let html = r#"<div class="result"><h3><a href="https://www.judgments.fedcourt.gov.au/judgments/Judgments/fca/single/2024/2024fca0255" title="Case [2024] FCA 255">Case</a></h3><p class=meta>20 Mar 2024<span class="divide">x</span></p></div>"#;
        let docs = parse_search_page(html)?;
        assert_eq!(docs[0].native_id, "fca/single/2024/2024fca0255");
        assert_eq!(docs[0].date.as_deref(), Some("2024-03-20"));
        Ok(())
    }

    #[test]
    fn federal_html_prefers_utf8_and_falls_back_to_windows_1252() {
        assert_eq!(
            decode_federal_court_html("purpose – fulcrum ‘test’".as_bytes()),
            "purpose – fulcrum ‘test’"
        );
        assert_eq!(
            decode_federal_court_html(b"legacy \x96 dash"),
            "legacy – dash"
        );
    }

    #[test]
    fn federal_encoding_damage_uses_an_official_fallback() {
        assert!(source_text_has_encoding_damage("Jacobsonï¿½J"));
        assert!(source_text_has_encoding_damage("broken â€™ apostrophe"));
        assert!(!source_text_has_encoding_damage(
            "Jacobson J – correct punctuation"
        ));
    }

    #[test]
    #[ignore = "requires Chrome, antiword, and live Federal Court access"]
    fn live_encoding_damaged_html_uses_word_fallback() -> Result<()> {
        let url =
            "https://www.judgments.fedcourt.gov.au/judgments/Judgments/fca/single/2010/2010fca0638";
        let entry = DiscoveredDocument {
            native_id: "fca/single/2010/2010fca0638".to_owned(),
            upstream_version: "live-test".to_owned(),
            title: "Suyen Corporation v Americana International Limited [2010] FCA 638".to_owned(),
            document_type: "Judgment".to_owned(),
            date: Some("2010-06-18".to_owned()),
            citation: Some("[2010] FCA 638".to_owned()),
            canonical_url: url.to_owned(),
            renditions: vec![Rendition {
                url: url.to_owned(),
                kind: RenditionKind::Html,
            }],
        };
        let client = OfficialHttpClient::new(&ADAPTER)?;
        let acquired = ADAPTER
            .acquire(&client, &entry)?
            .ok_or_else(|| anyhow!("live Federal Court fallback returned no document"))?;
        assert!(acquired.html.contains("Suyen"));
        assert!(!source_text_has_encoding_damage(&acquired.html));
        Ok(())
    }
}
