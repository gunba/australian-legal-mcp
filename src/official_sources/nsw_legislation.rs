use super::*;

pub(super) static ADAPTER: NswLegislation = NswLegislation;

pub(super) struct NswLegislation;

const HOST: &str = "legislation.nsw.gov.au";

impl OfficialAdapter for NswLegislation {
    fn source_id(&self) -> &'static str {
        NSW_LEGISLATION_SOURCE_ID
    }
    fn display_name(&self) -> &'static str {
        "NSW Legislation"
    }
    fn approved_hosts(&self) -> &'static [&'static str] {
        &[HOST]
    }
    fn rate_policy(&self) -> SourceRatePolicy {
        SourceRatePolicy {
            minimum_request_interval_ms: 2_000,
            request_timeout_seconds: 45,
        }
    }

    fn discover(
        &self,
        client: &OfficialHttpClient,
        _mode: SourceUpdateMode,
    ) -> Result<Vec<DiscoveredDocument>> {
        let pit = Utc::now().format("%d/%m/%Y");
        let tables = [
            ("pubacts", "primary_legislation"),
            ("pvtacts", "primary_legislation"),
            ("si", "secondary_legislation"),
            ("epi", "secondary_legislation"),
        ];
        let pages = parallel_map(
            SOURCE_WORKER_CEILING,
            tables.to_vec(),
            |(table, document_type)| {
                let url = format!(
                    "https://{HOST}/tables/{table}if?pit={pit}&sort=chron&renderas=html&generate="
                );
                let payload = client.get_required(&url, "text/html", MAX_INDEX_BYTES)?;
                Ok((document_type, decode_utf8(&payload.bytes)?))
            },
        )?;
        let mut index_rows = Vec::new();
        let selector = Selector::parse("a[href^='/view/html/'], a[href^='/view/pdf/']")
            .map_err(|_| anyhow!("invalid NSW legislation index selector"))?;
        for (document_type, html) in pages {
            let parsed = Html::parse_document(&html);
            for element in parsed.select(&selector) {
                let Some(href) = element.value().attr("href") else {
                    continue;
                };
                let title = element
                    .text()
                    .collect::<String>()
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                if title.is_empty() {
                    continue;
                }
                let path = href
                    .strip_prefix("/view/html/")
                    .or_else(|| href.strip_prefix("/view/pdf/"))
                    .ok_or_else(|| anyhow!("NSW legislation index link changed shape"))?;
                index_rows.push((path.to_owned(), title, document_type.to_owned()));
            }
        }
        index_rows.sort();
        index_rows.dedup();
        let documents = parallel_map(
            SOURCE_WORKER_CEILING,
            index_rows,
            |(path, title, document_type)| discover_entry(client, &path, title, document_type),
        )?;
        Ok(documents)
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
            bail!("NSW legislation document returned HTTP {}", payload.status);
        }
        if payload.content_type.as_deref() == Some("application/pdf") {
            let html = normalize_pdf(&payload.bytes)?;
            return Ok(Some(make_acquired_text(html, entry.canonical_url.clone())));
        }
        let html = decode_utf8(&payload.bytes)?;
        if html.contains("No fragments found.") {
            bail!("NSW legislation page reports that no full-text fragments were found");
        }
        let normalized = normalize_html(
            &html,
            &entry.canonical_url,
            HtmlRules {
                content_selector: "#frag-col",
                drop_ids: &["fragToolbar"],
                drop_classes: &["nav-result", "view-history-note"],
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
    let doc_id = path
        .rsplit('/')
        .next()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("NSW legislation path has no document id"))?;
    let (upstream_version, rendition_url, date, canonical_url) = if path.starts_with("asmade/") {
        (
            path.to_owned(),
            format!("https://{HOST}/view/whole/html/inforce/{path}"),
            None,
            format!("https://{HOST}/view/html/{path}"),
        )
    } else {
        let status_url = format!("https://{HOST}/view/html/inforce/current/{doc_id}");
        let status = client.get(
            &status_url,
            "text/html, application/pdf",
            MAX_DOCUMENT_BYTES,
        )?;
        let mut pit = None;
        if status.status.is_success() && status.content_type.as_deref() != Some("application/pdf") {
            let html = decode_utf8(&status.bytes)?;
            let parsed = Html::parse_document(&html);
            let selector = Selector::parse("a[href*='pointInTime=']")
                .map_err(|_| anyhow!("invalid NSW point-in-time selector"))?;
            pit = parsed.select(&selector).find_map(|element| {
                let href = element.value().attr("href")?;
                let url = Url::parse(&format!("https://{HOST}"))
                    .ok()?
                    .join(href)
                    .ok()?;
                url.query_pairs()
                    .find_map(|(name, value)| (name == "pointInTime").then(|| value.into_owned()))
            });
        }
        let version = pit
            .clone()
            .map(|date| format!("{date}/{doc_id}"))
            .unwrap_or_else(|| format!("current/{doc_id}/{}", sha256_bytes(&status.bytes)));
        let rendition = pit
            .as_deref()
            .map(|date| format!("https://{HOST}/view/whole/html/inforce/{date}/{doc_id}"))
            .unwrap_or_else(|| format!("https://{HOST}/view/whole/html/inforce/current/{doc_id}"));
        (version, rendition, pit, status_url.clone())
    };
    Ok(DiscoveredDocument {
        native_id: doc_id.to_owned(),
        upstream_version,
        title: title.clone(),
        document_type,
        date,
        citation: Some(title),
        canonical_url,
        renditions: vec![Rendition {
            url: rendition_url,
            kind: RenditionKind::Html,
        }],
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn document_id_is_stable_across_point_in_time_versions() {
        let path = "inforce/2025-01-01/act-2000-001";
        assert_eq!(path.rsplit('/').next(), Some("act-2000-001"));
    }
}
