use super::*;

pub(super) static ADAPTER: WesternAustralianLegislation = WesternAustralianLegislation;
pub(super) struct WesternAustralianLegislation;

const HOST: &str = "www.legislation.wa.gov.au";

impl OfficialAdapter for WesternAustralianLegislation {
    fn source_id(&self) -> &'static str {
        WESTERN_AUSTRALIAN_LEGISLATION_SOURCE_ID
    }
    fn display_name(&self) -> &'static str {
        "Western Australian Legislation"
    }
    fn approved_hosts(&self) -> &'static [&'static str] {
        &[HOST]
    }
    fn rate_policy(&self) -> SourceRatePolicy {
        SourceRatePolicy {
            minimum_request_interval_ms: 0,
            request_timeout_seconds: 60,
        }
    }

    fn normalization_revision(&self) -> Option<&'static str> {
        Some("1")
    }

    fn discover(
        &self,
        client: &OfficialHttpClient,
        _mode: SourceUpdateMode,
    ) -> Result<Vec<DiscoveredDocument>> {
        let mut pages = Vec::new();
        for (category, document_type) in [
            ("acts", "primary_legislation"),
            ("subs", "secondary_legislation"),
        ] {
            for letter in 'a'..='z' {
                pages.push((category, document_type, letter));
            }
        }
        let rows = parallel_map(
            SOURCE_WORKER_CEILING,
            pages,
            |(category, document_type, letter)| {
                let url =
                    format!("https://{HOST}/legislation/statutes.nsf/{category}if_{letter}.html");
                let payload = client.get_required(&url, "text/html", MAX_INDEX_BYTES)?;
                let html = decode_windows_1252(&payload.bytes);
                parse_index_rows(&html, document_type)
            },
        )?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        let mut rows_by_id: BTreeMap<String, IndexRow> = BTreeMap::new();
        for row in rows {
            if let Some(existing) = rows_by_id.get(&row.doc_id) {
                if existing.document_type != row.document_type
                    || existing.rendition_url != row.rendition_url
                    || existing.upstream_version != row.upstream_version
                {
                    bail!("WA index has conflicting records for {}", row.doc_id);
                }
                continue;
            }
            rows_by_id.insert(row.doc_id.clone(), row);
        }
        parallel_map(
            SOURCE_WORKER_CEILING,
            rows_by_id.into_values().collect(),
            |row| discover_entry(client, row),
        )
    }

    fn acquire(
        &self,
        client: &OfficialHttpClient,
        entry: &DiscoveredDocument,
    ) -> Result<Option<AcquiredDocument>> {
        let payload = client.get(
            &entry.renditions[0].url,
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            MAX_DOCUMENT_BYTES,
        )?;
        if payload.status == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !payload.status.is_success() {
            bail!("Western Australian DOCX returned HTTP {}", payload.status);
        }
        let source: SourceId = self.source_id().parse()?;
        let (html, assets) =
            crate::frl::normalize_docx_for_source(&payload.bytes, &source, &entry.native_id)
                .with_context(|| format!("normalizing WA DOCX {}", entry.native_id))?;
        Ok(Some(AcquiredDocument {
            html,
            assets,
            date: None,
            canonical_url: entry.canonical_url.clone(),
        }))
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct IndexRow {
    doc_id: String,
    title: String,
    document_type: String,
    rendition_url: String,
    upstream_version: String,
    date_text: Option<String>,
}

fn parse_index_rows(html: &str, document_type: &str) -> Result<Vec<IndexRow>> {
    let parsed = Html::parse_document(html);
    let row_selector = Selector::parse("tr").map_err(|_| anyhow!("invalid WA row selector"))?;
    let alive_selector = Selector::parse("a.alive[href*='.html']")
        .map_err(|_| anyhow!("invalid WA alive selector"))?;
    let docx_selector = Selector::parse("a[href*='RedirectURL'][href*='.docx']")
        .map_err(|_| anyhow!("invalid WA DOCX selector"))?;
    let date_pattern = regex::Regex::new(r"\b\d{1,2} [A-Z][a-z]{2} \d{4}\b")?;
    let mut rows = Vec::new();
    for row in parsed.select(&row_selector) {
        let Some(alive) = row.select(&alive_selector).next() else {
            continue;
        };
        let Some(docx) = row.select(&docx_selector).next() else {
            continue;
        };
        let Some(doc_href) = alive.value().attr("href") else {
            continue;
        };
        let Some(doc_id) = doc_href
            .split('&')
            .next()
            .and_then(|value| value.strip_suffix(".html"))
        else {
            continue;
        };
        let title = alive
            .text()
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if title.is_empty() {
            continue;
        }
        let docx_href = docx
            .value()
            .attr("href")
            .ok_or_else(|| anyhow!("WA DOCX link has no href"))?;
        let rendition_url = Url::parse(&format!("https://{HOST}/legislation/statutes.nsf/"))?
            .join(docx_href)?
            .to_string();
        let parsed_url = Url::parse(&rendition_url)?;
        let upstream_version = parsed_url
            .query_pairs()
            .find_map(|(name, value)| {
                (name.eq_ignore_ascii_case("query"))
                    .then(|| value.trim_end_matches(".docx").to_owned())
            })
            .ok_or_else(|| anyhow!("WA DOCX link has no version query"))?;
        let row_text = row.text().collect::<Vec<_>>().join(" ");
        let date_text = date_pattern
            .find(&row_text)
            .map(|value| value.as_str().to_owned());
        rows.push(IndexRow {
            doc_id: doc_id.to_owned(),
            title,
            document_type: document_type.to_owned(),
            rendition_url,
            upstream_version,
            date_text,
        });
    }
    Ok(rows)
}

fn discover_entry(client: &OfficialHttpClient, row: IndexRow) -> Result<DiscoveredDocument> {
    let status_url = format!(
        "https://{HOST}/legislation/statutes.nsf/{}.html",
        row.doc_id
    );
    let date = match row
        .date_text
        .as_deref()
        .and_then(|value| parse_date(value, &["%d %b %Y"]))
    {
        Some(date) => Some(date),
        None => {
            let payload = client.get_required(&status_url, "text/html", MAX_INDEX_BYTES)?;
            let html = decode_windows_1252(&payload.bytes);
            let text = Html::parse_document(&html)
                .root_element()
                .text()
                .collect::<Vec<_>>()
                .join(" ");
            regex::Regex::new(r"\b\d{1,2} [A-Z][a-z]{2} \d{4}\b")?
                .find(&text)
                .and_then(|value| parse_date(value.as_str(), &["%d %b %Y"]))
        }
    };
    Ok(DiscoveredDocument {
        native_id: row.doc_id,
        upstream_version: row.upstream_version,
        title: row.title.clone(),
        document_type: row.document_type,
        date,
        citation: Some(row.title),
        canonical_url: status_url,
        renditions: vec![Rendition {
            url: row.rendition_url,
            kind: RenditionKind::Docx,
        }],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wa_index_row_uses_status_identity_and_docx_version() -> Result<()> {
        let html = "<table><tr><td><a href='A1.html' class='x alive'>Act One</a></td><td>1 Jan 2025</td><td><a href='RedirectURL?OpenAgent&amp;query=v123.docx' class='tooltip'>Word</a></td></tr></table>";
        let rows = parse_index_rows(html, "primary_legislation")?;
        assert_eq!(rows[0].doc_id, "A1");
        assert_eq!(rows[0].upstream_version, "v123");
        Ok(())
    }
}
