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

    fn normalization_revision(&self) -> Option<&'static str> {
        Some("2")
    }

    fn minimum_request_interval_ms(&self, url: &Url) -> u64 {
        if url.host_str() == Some(SEARCH_HOST) {
            1_200
        } else {
            0
        }
    }
    fn has_browser_transport(&self) -> bool {
        true
    }
    fn use_browser_transport(&self, url: &Url) -> bool {
        matches!(url.host_str(), Some(JUDGMENTS_HOST | FILE_STORE_HOST))
    }

    fn validate_normalized_html(&self, html: &str) -> Result<()> {
        validate_normalized_federal_document(html)
    }

    fn discover(
        &self,
        client: &OfficialHttpClient,
        _mode: SourceUpdateMode,
    ) -> Result<Vec<DiscoveredDocument>> {
        let first_url = format!("{BASE_URL}num_ranks=1");
        let first = client.get_required(&first_url, "text/html", MAX_INDEX_BYTES)?;
        validate_federal_response_url(&first_url, &first.final_url)?;
        let first_html = decode_federal_court_html(&first.bytes);
        let alleged_total = result_total(&first_html)?;
        let final_url = format!("{BASE_URL}num_ranks=1&start_rank={alleged_total}");
        let final_page = client.get_required(&final_url, "text/html", MAX_INDEX_BYTES)?;
        validate_federal_response_url(&final_url, &final_page.final_url)?;
        let final_html = decode_federal_court_html(&final_page.bytes);
        let total = result_total(&final_html)?.max(alleged_total);
        let starts = (0..total.div_ceil(PAGE_SIZE))
            .map(|page| page * PAGE_SIZE + 1)
            .collect::<Vec<_>>();
        let pages = parallel_map(SOURCE_WORKER_CEILING, starts, |start| {
            let url = format!("{BASE_URL}num_ranks={PAGE_SIZE}&start_rank={start}");
            let payload = client.get_required(&url, "text/html", MAX_INDEX_BYTES)?;
            validate_federal_response_url(&url, &payload.final_url)?;
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
        validate_federal_judgment_response_url(
            &entry.renditions[0].url,
            &payload.final_url,
            &entry.native_id,
        )?;
        if payload.status == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !payload.status.is_success() {
            bail!("Federal Court judgment returned HTTP {}", payload.status);
        }
        if payload.content_type.as_deref() == Some("application/pdf") {
            let html = normalize_federal_pdf(&payload.bytes)?;
            return Ok(Some(make_acquired_text(html, entry.canonical_url.clone())));
        }
        let html = decode_federal_court_html(&payload.bytes);
        let parsed = Html::parse_document(&html);
        if federal_missing_document_page(&parsed)? {
            return Ok(None);
        }
        let content_selector = Selector::parse("div.judgment_content")
            .map_err(|_| anyhow!("invalid Federal Court judgment selector"))?;
        let judgment_content = parsed.select(&content_selector).next();
        let word_url = official_rendition_url(&parsed, &entry.canonical_url, "word")?;
        let pdf_url = official_rendition_url(&parsed, &entry.canonical_url, "pdf")?;
        let mut rejected = Vec::new();
        if let Some(content) = judgment_content {
            match validate_federal_html_content(content).and_then(|()| {
                let normalized = normalize_html(
                    &html,
                    &entry.canonical_url,
                    HtmlRules {
                        content_selector: "div.judgment_content",
                        drop_ids: &[],
                        drop_classes: &[],
                        heading_classes: &[],
                        preserve_same_document_fragments: true,
                        repair_broken_links: false,
                    },
                )?;
                validate_normalized_federal_document(&normalized)?;
                Ok(normalized)
            }) {
                Ok(normalized) => {
                    return Ok(Some(make_acquired_html(
                        normalized,
                        entry.canonical_url.clone(),
                    )))
                }
                Err(error) => rejected.push(format!("HTML: {error:#}")),
            }
        } else {
            rejected.push("HTML: judgment content is absent".to_owned());
        }

        if let Some(word_url) = word_url {
            let acquired: Result<AcquiredDocument> = (|| {
                let word = client.get_required(
                    &word_url,
                    "application/vnd.openxmlformats-officedocument.wordprocessingml.document, application/msword, application/rtf, text/rtf, application/pdf",
                    MAX_DOCUMENT_BYTES,
                )?;
                validate_federal_response_url(&word_url, &word.final_url)?;
                let (normalized, assets) = normalize_federal_word_rendition(
                    self.source_id(),
                    &entry.native_id,
                    &entry.canonical_url,
                    &word.bytes,
                )?;
                validate_normalized_federal_document(&normalized)?;
                Ok(AcquiredDocument {
                    html: normalized,
                    assets,
                    date: None,
                    canonical_url: entry.canonical_url.clone(),
                })
            })();
            match acquired {
                Ok(acquired) => return Ok(Some(acquired)),
                Err(error) => rejected.push(format!("Word: {error:#}")),
            }
        }

        if let Some(pdf_url) = pdf_url {
            let acquired: Result<AcquiredDocument> = (|| {
                let pdf = client.get_required(&pdf_url, "application/pdf", MAX_DOCUMENT_BYTES)?;
                validate_federal_response_url(&pdf_url, &pdf.final_url)?;
                if !pdf.bytes.starts_with(b"%PDF") {
                    bail!("Federal Court PDF rendition has an unrecognised file signature");
                }
                let normalized = normalize_federal_pdf(&pdf.bytes)?;
                Ok(make_acquired_text(normalized, entry.canonical_url.clone()))
            })();
            match acquired {
                Ok(acquired) => return Ok(Some(acquired)),
                Err(error) => rejected.push(format!("PDF: {error:#}")),
            }
        }

        bail!(
            "Federal Court judgment {} has no usable official rendition ({})",
            entry.native_id,
            rejected.join("; ")
        )
    }
}

fn validate_federal_judgment_response_url(
    requested: &str,
    final_url: &Url,
    native_id: &str,
) -> Result<()> {
    let requested_url = Url::parse(requested).context("parsing requested Federal Court URL")?;
    if final_url == &requested_url {
        return Ok(());
    }
    let expected_stem = native_id
        .rsplit('/')
        .next()
        .ok_or_else(|| anyhow!("Federal Court native identity has no citation segment"))?;
    let expected_suffix = format!("/{}.pdf", expected_stem.to_ascii_uppercase());
    if requested_url.host_str() == Some(JUDGMENTS_HOST)
        && requested_url.path().starts_with("/judgments/")
        && final_url.scheme() == "https"
        && final_url.host_str() == Some(FILE_STORE_HOST)
        && final_url.path().starts_with("/file-store/Judgments/")
        && final_url.path().ends_with(&expected_suffix)
        && final_url.query().is_none()
        && final_url.fragment().is_none()
    {
        return Ok(());
    }
    bail!("Federal Court judgment response differs from its requested official identity")
}

fn validate_federal_response_url(requested: &str, final_url: &Url) -> Result<()> {
    let requested = Url::parse(requested).context("parsing requested Federal Court URL")?;
    if final_url != &requested {
        bail!("Federal Court response URL differs from the requested official resource");
    }
    Ok(())
}

fn federal_missing_document_page(parsed: &Html) -> Result<bool> {
    let content_selector = Selector::parse("div.judgment_content")
        .map_err(|_| anyhow!("invalid Federal Court judgment selector"))?;
    if parsed.select(&content_selector).next().is_some() {
        return Ok(false);
    }
    let body_selector =
        Selector::parse("body").map_err(|_| anyhow!("invalid Federal Court body selector"))?;
    Ok(parsed.select(&body_selector).any(|body| {
        body.text()
            .collect::<Vec<_>>()
            .join(" ")
            .contains("Document could not be found")
    }))
}

fn official_rendition_url(parsed: &Html, base_url: &str, kind: &str) -> Result<Option<String>> {
    let selector =
        Selector::parse("a[href]").map_err(|_| anyhow!("invalid Federal Court link selector"))?;
    let base = Url::parse(base_url).context("parsing Federal Court judgment URL")?;
    let expected_label = format!("original {kind} document");
    parsed
        .select(&selector)
        .find(|element| {
            element
                .text()
                .collect::<Vec<_>>()
                .join(" ")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .to_ascii_lowercase()
                .contains(&expected_label)
        })
        .map(|element| {
            let href = element
                .value()
                .attr("href")
                .ok_or_else(|| anyhow!("Federal Court rendition link has no href"))?;
            Ok(base.join(href)?.to_string())
        })
        .transpose()
}

fn normalize_federal_word_rendition(
    source_id: &str,
    native_id: &str,
    canonical_url: &str,
    bytes: &[u8],
) -> Result<(String, Vec<NormalizedAsset>)> {
    if bytes.starts_with(b"%PDF") {
        return normalize_federal_pdf(bytes).map(|html| (html, Vec::new()));
    }

    let source: SourceId = source_id.parse()?;
    let (extension, structured) = if bytes.starts_with(b"PK\x03\x04") {
        (
            "docx",
            crate::frl::normalize_docx_for_source(bytes, &source, native_id),
        )
    } else if bytes.starts_with(&[0xd0, 0xcf, 0x11, 0xe0])
        || bytes
            .iter()
            .position(|byte| !byte.is_ascii_whitespace())
            .and_then(|start| bytes.get(start..))
            .is_some_and(|bytes| bytes.starts_with(b"{\\rtf"))
    {
        let extension = if bytes.starts_with(&[0xd0, 0xcf, 0x11, 0xe0]) {
            "doc"
        } else {
            "rtf"
        };
        (
            extension,
            convert_office_document_to_docx(bytes, extension)
                .and_then(|docx| crate::frl::normalize_docx_for_source(&docx, &source, native_id)),
        )
    } else {
        bail!("Federal Court Word rendition has an unrecognised file signature")
    };

    let structured_error = match structured.and_then(|normalized| {
        validate_normalized_federal_document(&normalized.0)?;
        Ok(normalized)
    }) {
        Ok(normalized) => return Ok(normalized),
        Err(error) => error,
    };

    let mut fallback_errors = Vec::new();
    if matches!(extension, "doc" | "rtf") {
        match normalize_rtf(bytes, canonical_url).and_then(|html| {
            validate_normalized_federal_document(&html)?;
            Ok(html)
        }) {
            Ok(html) => return Ok((html, Vec::new())),
            Err(error) => fallback_errors.push(format!("text fallback: {error:#}")),
        }
    }

    let rendered = render_office_document_to_pdf_bytes(bytes, extension).with_context(|| {
        format!(
            "structured Federal Court Word normalization failed: {structured_error:#}; {}",
            fallback_errors.join("; ")
        )
    })?;
    let html = normalize_federal_pdf(&rendered).with_context(|| {
        format!(
            "structured Federal Court Word normalization failed before rendered-PDF fallback: {structured_error:#}; {}",
            fallback_errors.join("; ")
        )
    })?;
    Ok((html, Vec::new()))
}

fn normalize_federal_pdf(bytes: &[u8]) -> Result<String> {
    let direct_error = match super::extract_pdf_text(bytes) {
        Ok(extracted) => match super::normalize_extracted_pdf_text(&extracted).and_then(|html| {
            validate_normalized_federal_document(&html)?;
            Ok(html)
        }) {
            Ok(html) => return Ok(html),
            Err(error) => error,
        },
        Err(error) => error,
    };
    let ocr = super::ocr_pdf(bytes).with_context(|| {
        format!("Federal Court direct PDF rendition failed quality validation: {direct_error:#}")
    })?;
    let html = super::normalize_extracted_pdf_text(&ocr)
        .context("Federal Court PDF OCR produced no indexable text")?;
    validate_normalized_federal_document(&html)
        .context("Federal Court PDF OCR failed quality validation")?;
    Ok(html)
}

fn validate_federal_html_content(content: scraper::ElementRef<'_>) -> Result<()> {
    validate_federal_content_structure(content)?;
    validate_federal_footnote_targets(content)
}

fn validate_normalized_federal_document(html: &str) -> Result<()> {
    let parsed = Html::parse_document(html);
    let selector =
        Selector::parse("article").map_err(|_| anyhow!("invalid normalized article selector"))?;
    let article = parsed
        .select(&selector)
        .next()
        .ok_or_else(|| anyhow!("normalized Federal Court document lacks an article"))?;
    validate_federal_content_structure(article)?;
    validate_federal_footnote_targets(article)
}

fn validate_federal_content_structure(content: scraper::ElementRef<'_>) -> Result<()> {
    let text = content.text().collect::<Vec<_>>().join(" ");
    validate_federal_text(&text)?;
    let lowercase = text.to_ascii_lowercase();
    if lowercase.contains("to view this judgment in full")
        || lowercase.contains("the summary is reproduced below")
    {
        bail!("Federal Court rendition is a summary or download placeholder");
    }

    let alphanumeric = text
        .chars()
        .filter(|character| character.is_alphanumeric())
        .count();
    let block_selector = Selector::parse(
        "p, li, blockquote, tr, h1, h2, h3, h4, h5, h6, section, figure, figcaption",
    )
    .map_err(|_| anyhow!("invalid Federal Court structural selector"))?;
    let substantive_blocks = content
        .select(&block_selector)
        .filter(|element| {
            element
                .text()
                .flat_map(str::chars)
                .filter(|character| character.is_alphanumeric())
                .take(20)
                .count()
                == 20
        })
        .take(3)
        .count();
    if substantive_blocks == 0 {
        bail!("Federal Court rendition has no substantive structural blocks");
    }
    if alphanumeric >= 1_000 && substantive_blocks < 3 {
        bail!("Federal Court rendition collapses substantive text into too few structural blocks");
    }
    Ok(())
}

fn validate_federal_text(text: &str) -> Result<()> {
    if !text.chars().any(|character| character.is_alphanumeric()) {
        bail!("Federal Court rendition has no substantive text");
    }
    if source_text_has_encoding_damage(text) {
        bail!("Federal Court rendition contains encoding damage");
    }
    if has_malformed_spaced_glyphs(text) {
        bail!("Federal Court rendition contains malformed spaced-glyph text");
    }
    Ok(())
}

fn validate_federal_footnote_targets(root: scraper::ElementRef<'_>) -> Result<()> {
    let target_selector = Selector::parse("[id], a[name]")
        .map_err(|_| anyhow!("invalid Federal Court footnote target selector"))?;
    let targets = root
        .select(&target_selector)
        .flat_map(|element| {
            [element.value().attr("id"), element.value().attr("name")]
                .into_iter()
                .flatten()
        })
        .collect::<BTreeSet<_>>();
    let link_selector = Selector::parse("a[href^='#']")
        .map_err(|_| anyhow!("invalid Federal Court footnote link selector"))?;
    for link in root.select(&link_selector) {
        let fragment = link
            .value()
            .attr("href")
            .unwrap_or_default()
            .trim_start_matches('#');
        let lowercase = fragment.to_ascii_lowercase();
        if (lowercase.contains("footnote") || lowercase.contains("ftn"))
            && !targets.contains(fragment)
        {
            bail!("Federal Court footnote link #{fragment} has no attached target");
        }
    }
    Ok(())
}

fn has_malformed_spaced_glyphs(text: &str) -> bool {
    let alphabetic = text
        .chars()
        .filter(|character| character.is_alphabetic())
        .count();
    let mut run = 0usize;
    let mut spaced = 0usize;
    for token in text.split_whitespace().chain(std::iter::once("")) {
        if token.chars().count() == 1 && token.chars().all(char::is_alphabetic) {
            run += 1;
            continue;
        }
        if run >= 8 {
            spaced += run;
        }
        run = 0;
    }
    (spaced >= 100 && spaced.saturating_mul(20) >= alphabetic)
        || (spaced >= 24 && spaced.saturating_mul(2) >= alphabetic)
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
    fn federal_response_url_is_bound_to_the_requested_resource() -> Result<()> {
        let requested = "https://www.judgments.fedcourt.gov.au/files/example.docx";
        assert!(validate_federal_response_url(requested, &Url::parse(requested)?).is_ok());
        assert!(validate_federal_response_url(
            requested,
            &Url::parse("https://www.judgments.fedcourt.gov.au/files/other.docx")?
        )
        .is_err());
        let judgment =
            "https://www.judgments.fedcourt.gov.au/judgments/Judgments/fca/single/1981/1981fca0206";
        let official_pdf = Url::parse("https://www.fedcourt.gov.au/file-store/Judgments/Federal%20Court/Single%20Court/1981/1981FCA0206/1981FCA0206.pdf")?;
        assert!(validate_federal_judgment_response_url(
            judgment,
            &official_pdf,
            "fca/single/1981/1981fca0206"
        )
        .is_ok());
        assert!(validate_federal_judgment_response_url(
            judgment,
            &Url::parse("https://www.fedcourt.gov.au/file-store/Judgments/Federal%20Court/Single%20Court/1981/1981FCA9999/1981FCA9999.pdf")?,
            "fca/single/1981/1981fca0206"
        )
        .is_err());
        Ok(())
    }

    const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/federal-court");

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
    fn federal_html_preserves_numbered_paragraph_and_footnote_relationships() -> Result<()> {
        let html = fs::read_to_string(Path::new(FIXTURES).join("structured-judgment.html"))?;
        let parsed = Html::parse_document(&html);
        let selector = Selector::parse("div.judgment_content").unwrap();
        validate_federal_html_content(parsed.select(&selector).next().unwrap())?;
        let normalized = normalize_html(
            &html,
            "https://www.judgments.fedcourt.gov.au/judgments/Judgments/fca/single/2026/2026fca0001",
            HtmlRules {
                content_selector: "div.judgment_content",
                drop_ids: &[],
                drop_classes: &[],
                heading_classes: &[],
                preserve_same_document_fragments: true,
                repair_broken_links: false,
            },
        )?;
        validate_normalized_federal_document(&normalized)?;
        assert!(normalized.contains("<p id=\"paragraph-1\">1"));
        assert!(normalized.contains("<p id=\"paragraph-2\">2"));
        assert!(normalized.contains("href=\"#_ftn1\""));
        assert!(normalized.contains("id=\"_ftn1\""));
        assert!(normalized.contains("href=\"#_ftnref1\""));
        Ok(())
    }

    #[test]
    fn federal_degraded_html_exposes_word_then_pdf_fallbacks() -> Result<()> {
        let html = fs::read_to_string(Path::new(FIXTURES).join("degraded-judgment.html"))?;
        let parsed = Html::parse_document(&html);
        let selector = Selector::parse("div.judgment_content").unwrap();
        let error = validate_federal_html_content(parsed.select(&selector).next().unwrap())
            .unwrap_err()
            .to_string();
        assert!(error.contains("summary or download placeholder"));
        assert_eq!(
            official_rendition_url(
                &parsed,
                "https://www.judgments.fedcourt.gov.au/judgments/Judgments/fca/single/2026/2026fca0002",
                "word",
            )?
            .as_deref(),
            Some("https://www.judgments.fedcourt.gov.au/files/2026fca0002.docx")
        );
        assert_eq!(
            official_rendition_url(
                &parsed,
                "https://www.judgments.fedcourt.gov.au/judgments/Judgments/fca/single/2026/2026fca0002",
                "pdf",
            )?
            .as_deref(),
            Some("https://www.judgments.fedcourt.gov.au/files/2026fca0002.pdf")
        );
        Ok(())
    }

    #[test]
    fn judgment_text_does_not_trigger_the_missing_document_page() -> Result<()> {
        let valid = Html::parse_document(
            "<html><body><div class=\"judgment_content\"><p>The requested Document could not be found or did not exist.</p></div></body></html>",
        );
        assert!(!federal_missing_document_page(&valid)?);

        let missing = Html::parse_document(
            "<html><body><main><p>Document could not be found</p></main></body></html>",
        );
        assert!(federal_missing_document_page(&missing)?);
        Ok(())
    }

    #[test]
    fn federal_short_non_structural_html_is_rejected() -> Result<()> {
        let html = fs::read_to_string(Path::new(FIXTURES).join("short-degraded-judgment.html"))?;
        let parsed = Html::parse_document(&html);
        let selector = Selector::parse("div.judgment_content").unwrap();
        let error = validate_federal_html_content(parsed.select(&selector).next().unwrap())
            .expect_err("short non-structural HTML must use an official fallback");
        assert!(error
            .to_string()
            .contains("no substantive structural blocks"));
        Ok(())
    }

    #[test]
    fn federal_fallbacks_reject_placeholders_and_accept_short_substantive_reasons() -> Result<()> {
        for placeholder in [
            "<article><p>Error</p></article>",
            "<article><p>To view this judgment in full, download the document.</p></article>",
        ] {
            validate_normalized_federal_document(placeholder)
                .expect_err("fallback placeholder must not become a committed judgment");
        }
        validate_normalized_federal_document(
            "<article><p>The application is dismissed by consent.</p></article>",
        )?;
        Ok(())
    }

    #[test]
    fn federal_word_preserves_automatic_numbering_and_reference_ordered_footnotes() -> Result<()> {
        let bytes = fs::read(Path::new(FIXTURES).join("numbered-paragraph-footnote.docx"))?;
        let (html, assets) = normalize_federal_word_rendition(
            FEDERAL_COURT_SOURCE_ID,
            "fca/single/2026/2026fca0001",
            "https://www.judgments.fedcourt.gov.au/judgments/Judgments/fca/single/2026/2026fca0001",
            &bytes,
        )?;
        validate_normalized_federal_document(&html)?;
        assert!(assets.is_empty());
        assert!(html.contains("<p>1 The numbered paragraph"));
        assert!(html.contains("<p>2 The next numbered paragraph"));
        assert_eq!(
            html.matches("<p>(c) The nested").count(),
            2,
            "the overridden lower-letter level must restart after the next level-zero paragraph"
        );
        assert!(
            html.contains("<sup><a id=\"footnote-reference-9\" href=\"#footnote-9\">1</a></sup>")
        );
        assert!(
            html.contains("<sup><a id=\"footnote-reference-2\" href=\"#footnote-2\">2</a></sup>")
        );
        assert!(html.contains(
            "<li id=\"footnote-9\"><a href=\"#footnote-reference-9\"></a><p>The first-referenced footnote has the higher internal ID."
        ));
        assert!(html.contains(
            "<li id=\"footnote-2\"><a href=\"#footnote-reference-2\"></a><p>The second-referenced footnote has the lower internal ID."
        ));
        assert!(html.find("id=\"footnote-9\"").unwrap() < html.find("id=\"footnote-2\"").unwrap());
        Ok(())
    }

    #[test]
    fn federal_spaced_glyph_output_is_rejected_without_rejecting_letterspaced_heading() {
        let malformed = (0..12)
            .map(|_| "T h i s j u d g m e n t t e x t i s m a l f o r m e d")
            .collect::<Vec<_>>()
            .join(" ");
        assert!(has_malformed_spaced_glyphs(&malformed));
        assert!(!has_malformed_spaced_glyphs(
            "C A T C H W O R D S Bankruptcy law and ordinary substantive judgment text follows."
        ));
        assert!(validate_normalized_federal_document(&format!(
            "<article><p>{malformed}</p></article>"
        ))
        .is_err());
    }

    #[test]
    fn federal_detached_footnote_is_rejected() {
        assert!(validate_normalized_federal_document(
            "<article><p>1 Reasons<sup><a href=\"#_ftn1\">1</a></sup></p></article>"
        )
        .is_err());
    }

    #[test]
    fn federal_linked_docx_numbering_preserves_visible_markers() -> Result<()> {
        let bytes =
            fs::read(Path::new(FIXTURES).join("2015fca1134-abstract-numbering-no-levels.docx"))?;
        let source: SourceId = FEDERAL_COURT_SOURCE_ID.parse()?;
        let (html, assets) =
            crate::frl::normalize_docx_for_source(&bytes, &source, "fca/single/2015/2015fca1134")?;
        validate_normalized_federal_document(&html)?;
        assert!(assets.is_empty());
        assert!(html.contains("Federal Court of Australia"));
        assert!(html.contains("<p>1. Pursuant to s 411"));
        assert!(html.contains("<p>(a) a meeting of holders"));
        assert!(html.contains("<p>1 Three separate but interdependent Schemes"));
        assert!(html.contains("<p>(i) each of the Schemes is an arrangement"));
        Ok(())
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
