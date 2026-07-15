use super::*;

pub(super) static ADAPTER: SouthAustralianLegislation = SouthAustralianLegislation;
pub(super) struct SouthAustralianLegislation;

const HOST: &str = "www.legislation.sa.gov.au";

impl OfficialAdapter for SouthAustralianLegislation {
    fn source_id(&self) -> &'static str {
        SOUTH_AUSTRALIAN_LEGISLATION_SOURCE_ID
    }
    fn display_name(&self) -> &'static str {
        "South Australian Legislation"
    }
    fn approved_hosts(&self) -> &'static [&'static str] {
        &[HOST]
    }
    fn rate_policy(&self) -> SourceRatePolicy {
        SourceRatePolicy {
            minimum_request_interval_ms: 1_000,
            request_timeout_seconds: 120,
        }
    }

    fn discover(
        &self,
        client: &OfficialHttpClient,
        _mode: SourceUpdateMode,
    ) -> Result<Vec<DiscoveredDocument>> {
        let categories = [
            ("acts/consolidated", "primary_legislation"),
            ("bills/current", "bill"),
            ("bills/archived", "bill"),
            (
                "regulations-and-rules/consolidated",
                "secondary_legislation",
            ),
            ("policies/consolidated", "secondary_legislation"),
            (
                "proclamations-and-notices/consolidated",
                "secondary_legislation",
            ),
        ];
        let category_pages = parallel_map(
            SOURCE_WORKER_CEILING,
            categories.to_vec(),
            |(category, document_type)| {
                let url = format!("https://{HOST}/legislation/{category}");
                let payload = client.get_required(&url, "text/html", MAX_INDEX_BYTES)?;
                let html = decode_utf8(&payload.bytes)?;
                let mut urls = south_australian_index_pages(&url, &html)?;
                if urls.is_empty() {
                    urls.push(url);
                }
                Ok(urls
                    .into_iter()
                    .map(|url| (url, document_type))
                    .collect::<Vec<_>>())
            },
        )?;
        let pages = category_pages.into_iter().flatten().collect::<Vec<_>>();
        let status_rows = parallel_map(SOURCE_WORKER_CEILING, pages, |(url, document_type)| {
            let payload = client.get_required(&url, "text/html", MAX_INDEX_BYTES)?;
            let html = decode_utf8(&payload.bytes)?;
            let parsed = Html::parse_document(&html);
            let selector = Selector::parse("tr a[href][title]")
                .map_err(|_| anyhow!("invalid South Australian index selector"))?;
            let mut rows = Vec::new();
            for element in parsed.select(&selector) {
                let Some(status_url) = element.value().attr("title") else {
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
                let status_url = Url::parse(&url)?.join(status_url)?.to_string();
                if Url::parse(&status_url)?.host_str() != Some(HOST) {
                    continue;
                }
                rows.push((status_url, title, document_type.to_owned()));
            }
            Ok(rows)
        })?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        let mut status_rows = status_rows;
        status_rows.sort();
        status_rows.dedup();
        let entries = parallel_map(
            SOURCE_WORKER_CEILING,
            status_rows,
            |(status_url, title, document_type)| {
                discover_entry(client, status_url, title, document_type)
            },
        )?;
        Ok(entries.into_iter().flatten().collect())
    }

    fn acquire(
        &self,
        client: &OfficialHttpClient,
        entry: &DiscoveredDocument,
    ) -> Result<Option<AcquiredDocument>> {
        let mut failures = Vec::new();
        let mut unavailable = 0usize;
        for rendition in &entry.renditions {
            let payload = client.get(
                &rendition.url,
                "application/rtf, text/rtf, application/pdf",
                MAX_DOCUMENT_BYTES,
            )?;
            if payload.status == StatusCode::NOT_FOUND {
                unavailable += 1;
                continue;
            }
            if !payload.status.is_success() {
                failures.push(format!("{}: HTTP {}", rendition.url, payload.status));
                continue;
            }
            let normalized = match rendition.kind {
                RenditionKind::Rtf => normalize_rtf(&payload.bytes, &entry.canonical_url),
                RenditionKind::Pdf => normalize_pdf(&payload.bytes),
                _ => bail!("unsupported South Australian rendition kind"),
            };
            match normalized {
                Ok(html) => return Ok(Some(make_acquired_text(html, entry.canonical_url.clone()))),
                Err(error) => failures.push(format!("{}: {error:#}", rendition.url)),
            }
        }
        if unavailable == entry.renditions.len() {
            return Ok(None);
        }
        bail!(
            "all South Australian renditions failed for {}: {}",
            entry.native_id,
            failures.join("; ")
        )
    }
}

fn discover_entry(
    client: &OfficialHttpClient,
    status_url: String,
    title: String,
    document_type: String,
) -> Result<Option<DiscoveredDocument>> {
    let payload = client.get(&status_url, "text/html", MAX_INDEX_BYTES)?;
    if payload.status == StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !payload.status.is_success() {
        bail!(
            "South Australian status page returned HTTP {}",
            payload.status
        );
    }
    let html = decode_utf8(&payload.bytes)?;
    let parsed = Html::parse_document(&html);
    let link_selector = Selector::parse("a[href$='.rtf'], a[href$='.pdf']")
        .map_err(|_| anyhow!("invalid South Australian rendition selector"))?;
    let mut renditions = parsed
        .select(&link_selector)
        .filter_map(|element| {
            let href = element.value().attr("href")?;
            if !href.contains("/_legislation-documents/") {
                return None;
            }
            let kind = if href.ends_with(".rtf") {
                RenditionKind::Rtf
            } else {
                RenditionKind::Pdf
            };
            let url = Url::parse(&status_url)
                .ok()?
                .join(href)
                .ok()
                .map(|url| url.to_string())?;
            let current = url.contains("/current/");
            Some((!current, kind != RenditionKind::Rtf, url, kind))
        })
        .collect::<Vec<_>>();
    renditions.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then(left.1.cmp(&right.1))
            .then(left.2.cmp(&right.2))
    });
    let Some((_, _, rendition_url, _)) = renditions.first() else {
        return Ok(None);
    };
    let rendition_group = rendition_url
        .strip_suffix(".rtf")
        .or_else(|| rendition_url.strip_suffix(".pdf"))
        .ok_or_else(|| anyhow!("South Australian rendition has no supported extension"))?
        .to_owned();
    let selected_renditions = renditions
        .into_iter()
        .filter_map(|(_, _, url, kind)| {
            let group = url
                .strip_suffix(".rtf")
                .or_else(|| url.strip_suffix(".pdf"))?;
            (group == rendition_group.as_str()).then_some(Rendition { url, kind })
        })
        .collect::<Vec<_>>();
    let status = Url::parse(&status_url)?;
    let native_id = status
        .query_pairs()
        .find_map(|(key, value)| (key == "path").then(|| value.into_owned()))
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow!("South Australian status URL has no stable path"))?;
    let previous_end =
        regex::Regex::new(r"\(\d{2} [A-Z][a-z]+ \d{4} - (\d{2} [A-Z][a-z]+ \d{4}), Authorised\)")?
            .captures(&html)
            .and_then(|captures| captures.get(1))
            .and_then(|value| chrono::NaiveDate::parse_from_str(value.as_str(), "%d %B %Y").ok())
            .and_then(|date| date.succ_opt())
            .map(|date| date.format("%Y-%m-%d").to_string());
    let main_selector = Selector::parse("main").map_err(|_| anyhow!("invalid main selector"))?;
    let main_html = parsed
        .select(&main_selector)
        .next()
        .map(|main| main.inner_html())
        .unwrap_or_else(|| html.clone());
    let upstream_version = previous_end
        .as_ref()
        .map(|date| format!("{date}/{native_id}"))
        .unwrap_or_else(|| format!("{}/{native_id}", sha256_bytes(main_html.as_bytes())));
    Ok(Some(DiscoveredDocument {
        native_id,
        upstream_version,
        title: title.clone(),
        document_type,
        date: previous_end,
        citation: Some(title),
        canonical_url: status_url,
        renditions: selected_renditions,
    }))
}

fn south_australian_index_pages(base_url: &str, html: &str) -> Result<Vec<String>> {
    let parsed = Html::parse_document(html);
    let selector = Selector::parse("a[href]")
        .map_err(|_| anyhow!("invalid South Australian page selector"))?;
    let mut pages = parsed
        .select(&selector)
        .filter_map(|element| {
            let text = element.text().collect::<String>();
            let key = text.trim();
            if key.len() != 1 || !key.as_bytes()[0].is_ascii_alphabetic() {
                return None;
            }
            let href = element.value().attr("href")?;
            if !href.contains("meta_resourceTitleAZ=") {
                return None;
            }
            Url::parse(base_url)
                .ok()?
                .join(href)
                .ok()
                .map(|url| url.to_string())
        })
        .collect::<Vec<_>>();
    pages.sort();
    pages.dedup();
    if pages.len() > 32 {
        bail!("South Australian index has too many alphabet pages");
    }
    Ok(pages)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn previous_version_end_advances_one_day() {
        let end = chrono::NaiveDate::parse_from_str("31 December 2024", "%d %B %Y")
            .expect("valid fixture date");
        assert_eq!(
            end.succ_opt()
                .expect("fixture date has a successor")
                .to_string(),
            "2025-01-01"
        );
    }

    #[test]
    fn current_official_index_links_supply_alphabet_pages() -> Result<()> {
        let pages = south_australian_index_pages(
            "https://www.legislation.sa.gov.au/legislation/acts/consolidated",
            r#"<a href="/legislation/acts/consolidated?collection=official&amp;meta_resourceTitleAZ=A">A</a>"#,
        )?;
        assert_eq!(pages.len(), 1);
        assert!(pages[0].contains("meta_resourceTitleAZ=A"));
        Ok(())
    }
}
