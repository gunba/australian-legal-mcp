use super::*;

pub(super) static ADAPTER: QueenslandLegislation = QueenslandLegislation;
pub(super) struct QueenslandLegislation;

const HOSTS: &[&str] = &["www.legislation.qld.gov.au", "legislation.qld.gov.au"];

impl OfficialAdapter for QueenslandLegislation {
    fn source_id(&self) -> &'static str {
        QUEENSLAND_LEGISLATION_SOURCE_ID
    }
    fn display_name(&self) -> &'static str {
        "Queensland Legislation"
    }
    fn approved_hosts(&self) -> &'static [&'static str] {
        HOSTS
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
        let pit = Utc::now().format("%d/%m/%Y");
        let tables = vec![
            (format!("https://www.legislation.qld.gov.au/tables/pubactsif?pit={pit}&sort=chron&renderas=html&generate="), "primary_legislation"),
            (format!("https://www.legislation.qld.gov.au/tables/siif?pit={pit}&sort=chron&renderas=html&generate="), "secondary_legislation"),
            (format!("https://www.legislation.qld.gov.au/tables/bills?dstart=03/11/1992&dend={pit}&sort=chron&renderas=html&generate="), "bill"),
        ];
        let pages = parallel_map(SOURCE_WORKER_CEILING, tables, |(url, document_type)| {
            let payload = client.get_required(&url, "text/html", MAX_INDEX_BYTES)?;
            Ok((document_type, decode_utf8(&payload.bytes)?))
        })?;
        let selector = Selector::parse("a[href^='/view/']")
            .map_err(|_| anyhow!("invalid Queensland legislation index selector"))?;
        let mut rows = Vec::new();
        for (document_type, html) in pages {
            let parsed = Html::parse_document(&html);
            for element in parsed.select(&selector) {
                let Some(path) = element
                    .value()
                    .attr("href")
                    .and_then(|href| href.strip_prefix("/view/"))
                else {
                    continue;
                };
                let title = element
                    .text()
                    .collect::<String>()
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                if !title.is_empty() {
                    rows.push((path.to_owned(), title, document_type.to_owned()));
                }
            }
        }
        rows.sort();
        rows.dedup();
        parallel_map(
            SOURCE_WORKER_CEILING,
            rows,
            |(path, title, document_type)| discover_entry(client, &path, title, document_type),
        )
    }

    fn acquire(
        &self,
        client: &OfficialHttpClient,
        entry: &DiscoveredDocument,
    ) -> Result<Option<AcquiredDocument>> {
        let rendition = &entry.renditions[0];
        let payload = client.get(
            &rendition.url,
            "text/html, application/pdf",
            MAX_DOCUMENT_BYTES,
        )?;
        if payload.status == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !payload.status.is_success() {
            bail!(
                "Queensland legislation document returned HTTP {}",
                payload.status
            );
        }
        if payload.content_type.as_deref() == Some("application/pdf") {
            let html = normalize_pdf(&payload.bytes)?;
            return Ok(Some(make_acquired_text(html, entry.canonical_url.clone())));
        }
        let html = decode_utf8(&payload.bytes)?;
        if !html.contains("id=\"view-whole\"") && !html.contains("id='view-whole'") {
            let pdf_url = rendition.url.replacen("/html/", "/pdf/", 1);
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
                content_selector: "#fragview",
                drop_ids: &[],
                drop_classes: &["view-history-note", "view-repealed", "source"],
                heading_classes: &[],
            },
        )?;
        Ok(Some(make_acquired_html(
            normalized,
            entry.canonical_url.clone(),
        )))
    }
}

fn discover_entry(
    client: &OfficialHttpClient,
    path: &str,
    title: String,
    document_type: String,
) -> Result<DiscoveredDocument> {
    let normalized_path = path
        .strip_prefix("html/")
        .or_else(|| path.strip_prefix("pdf/"))
        .unwrap_or(path);
    if document_type == "bill" {
        let native_id = normalized_path.to_owned();
        let canonical_url = format!("https://www.legislation.qld.gov.au/view/{path}");
        let rendition_url =
            format!("https://www.legislation.qld.gov.au/view/whole/html/{normalized_path}");
        return Ok(DiscoveredDocument {
            native_id: native_id.clone(),
            upstream_version: native_id,
            title: title.clone(),
            document_type,
            date: None,
            citation: Some(title),
            canonical_url,
            renditions: vec![Rendition {
                url: rendition_url,
                kind: RenditionKind::Html,
            }],
        });
    }
    let doc_id = normalized_path
        .rsplit('/')
        .next()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("Queensland legislation path has no document id"))?;
    let status_url =
        format!("https://www.legislation.qld.gov.au/view/html/inforce/current/{doc_id}");
    let status = client.get_required(&status_url, "text/html", MAX_DOCUMENT_BYTES)?;
    let html = decode_utf8(&status.bytes)?;
    let publication = regex::Regex::new(r"(?i)PublicationDate%3D(\d{8})")?
        .captures(&html)
        .and_then(|captures| captures.get(1))
        .map(|value| value.as_str())
        .ok_or_else(|| anyhow!("Queensland status page has no publication date"))?;
    let date = format!(
        "{}-{}-{}",
        &publication[..4],
        &publication[4..6],
        &publication[6..8]
    );
    let rendition_url =
        format!("https://www.legislation.qld.gov.au/view/whole/html/inforce/{date}/{doc_id}");
    Ok(DiscoveredDocument {
        native_id: doc_id.to_owned(),
        upstream_version: format!("{date}/{doc_id}"),
        title: title.clone(),
        document_type,
        date: Some(date),
        citation: Some(title),
        canonical_url: status_url,
        renditions: vec![Rendition {
            url: rendition_url,
            kind: RenditionKind::Html,
        }],
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn publication_date_becomes_iso_date() {
        let raw = "20250703";
        assert_eq!(
            format!("{}-{}-{}", &raw[..4], &raw[4..6], &raw[6..8]),
            "2025-07-03"
        );
    }
}
