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

    fn normalization_revision(&self) -> Option<&'static str> {
        Some("3")
    }

    fn validate_normalized_html(&self, html: &str) -> Result<()> {
        validate_south_australian_html(html)
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
                RenditionKind::Rtf => {
                    normalize_south_australian_rtf(&payload.bytes, &entry.canonical_url)
                }
                RenditionKind::Pdf => normalize_pdf(&payload.bytes),
                _ => bail!("unsupported South Australian rendition kind"),
            }
            .and_then(|html| {
                validate_south_australian_html(&html)?;
                Ok(html)
            });
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

fn normalize_south_australian_rtf(bytes: &[u8], base_url: &str) -> Result<String> {
    if bytes.starts_with(&[0xd0, 0xcf, 0x11, 0xe0]) {
        return normalize_rtf(bytes, base_url);
    }
    let start = bytes
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    if !bytes
        .get(start..)
        .is_some_and(|bytes| bytes.starts_with(b"{\\rtf"))
    {
        bail!("official South Australian RTF has an unrecognised file signature");
    }

    let structured = (|| -> Result<String> {
        let temp = tempfile::tempdir().context("creating South Australian RTF workspace")?;
        let input = temp.path().join("input.rtf");
        fs::write(&input, bytes).context("writing South Australian RTF input")?;
        let profile = temp.path().join("libreoffice-profile");
        let mut command = sandboxed_soffice_command(temp.path())?;
        command
            .arg("--headless")
            .arg(format!(
                "-env:UserInstallation=file://{}",
                profile.display()
            ))
            .arg("--convert-to")
            .arg("html:HTML (StarWriter)")
            .arg("--outdir")
            .arg(temp.path())
            .arg(&input)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        run_command_with_timeout(
            command,
            Duration::from_secs(15 * 60),
            "LibreOffice South Australian RTF conversion",
        )?;
        let output = read_bounded_file(&temp.path().join("input.html"), MAX_DOCUMENT_BYTES)
            .context("reading LibreOffice-converted South Australian RTF")?;
        let html = decode_utf8(&output).unwrap_or_else(|_| decode_windows_1252(&output));
        normalize_south_australian_converted_html(&html, base_url)
    })();
    match structured {
        Ok(html) => Ok(html),
        Err(structured_error) => normalize_rtf(bytes, base_url).with_context(|| {
            format!(
                "structured South Australian RTF conversion failed before the official fallback: {structured_error:#}"
            )
        }),
    }
}

fn normalize_south_australian_converted_html(html: &str, base_url: &str) -> Result<String> {
    normalize_html(
        html,
        base_url,
        HtmlRules {
            content_selector: "body",
            drop_ids: &[],
            drop_classes: &[],
            heading_classes: &[],
            preserve_same_document_fragments: true,
            repair_broken_links: true,
        },
    )
}

fn validate_south_australian_html(html: &str) -> Result<()> {
    let parsed = Html::parse_fragment(html);
    let article_selector = Selector::parse("article")
        .map_err(|_| anyhow!("invalid South Australian article selector"))?;
    let mut articles = parsed.select(&article_selector);
    let article = articles
        .next()
        .ok_or_else(|| anyhow!("South Australian document lacks an article root"))?;
    if articles.next().is_some() {
        bail!("South Australian document has multiple article roots");
    }

    let block_selector = Selector::parse("h1, h2, h3, h4, h5, h6, p, li")
        .map_err(|_| anyhow!("invalid South Australian block selector"))?;
    if article.select(&block_selector).next().is_none() {
        bail!("South Australian document has no structural text blocks");
    }

    let heading_selector = Selector::parse("h1, h2, h3, h4, h5, h6")
        .map_err(|_| anyhow!("invalid South Australian heading selector"))?;
    let invalid_heading_child_selector =
        Selector::parse("article, section, div, h1, h2, h3, h4, h5, h6, p, ol, ul, li, table")
            .map_err(|_| anyhow!("invalid South Australian heading-child selector"))?;
    for heading in article.select(&heading_selector) {
        if heading
            .text()
            .flat_map(str::chars)
            .all(|character| !character.is_alphanumeric())
            || heading
                .select(&invalid_heading_child_selector)
                .next()
                .is_some()
        {
            bail!("South Australian document contains a malformed heading");
        }
    }

    let link_selector =
        Selector::parse("a").map_err(|_| anyhow!("invalid South Australian hyperlink selector"))?;
    let target_selector = Selector::parse("[id], a[name]")
        .map_err(|_| anyhow!("invalid South Australian link-target selector"))?;
    let targets = article
        .select(&target_selector)
        .flat_map(|element| {
            [element.value().attr("id"), element.value().attr("name")]
                .into_iter()
                .flatten()
        })
        .collect::<BTreeSet<_>>();
    for link in article.select(&link_selector) {
        let Some(href) = link.value().attr("href") else {
            if [link.value().attr("id"), link.value().attr("name")]
                .into_iter()
                .flatten()
                .any(is_safe_fragment)
            {
                continue;
            }
            bail!("South Australian hyperlink has no target");
        };
        let label = link.text().collect::<Vec<_>>().join(" ");
        let label = label.split_whitespace().collect::<Vec<_>>().join(" ");
        if label.is_empty() || label.eq_ignore_ascii_case("hyperlink") {
            bail!("South Australian document contains a malformed hyperlink");
        }
        if let Some(fragment) = href.strip_prefix('#') {
            if !is_safe_fragment(fragment) || !targets.contains(fragment) {
                bail!("South Australian document contains an unresolved fragment hyperlink");
            }
            continue;
        }
        let target = Url::parse(href).context("parsing South Australian hyperlink target")?;
        if target.scheme() != "https"
            || target.host_str().is_none()
            || !target.username().is_empty()
            || target.password().is_some()
            || target.port().is_some()
        {
            bail!("South Australian document contains a malformed hyperlink");
        }
    }

    let visible_text = article.text().collect::<Vec<_>>().join(" ");
    let lowercase_text = visible_text.to_ascii_lowercase();
    if lowercase_text.contains("error! hyperlink reference not valid")
        || lowercase_text.contains("error! reference source not found")
        || regex::Regex::new(r#"(?i)\bHYPERLINK\s+(?:\\l\s+)?[\"{]"#)?.is_match(&visible_text)
    {
        bail!("South Australian document contains a literal hyperlink placeholder");
    }
    Ok(())
}

fn is_safe_fragment(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | ':')
        })
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

    const FIXTURES: &str = "tests/fixtures/sa-legislation";

    fn assert_structural_document(normalized: &str) -> Result<()> {
        validate_south_australian_html(normalized)?;
        let parsed = Html::parse_fragment(normalized);
        let text = |selector: &str| -> Result<Vec<String>> {
            let selector = Selector::parse(selector)
                .map_err(|_| anyhow!("invalid fixture assertion selector"))?;
            Ok(parsed
                .select(&selector)
                .map(|element| {
                    element
                        .text()
                        .collect::<String>()
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .collect())
        };
        assert_eq!(text("h1")?, vec!["PART 1"]);
        assert_eq!(text("h2")?, vec!["Preliminary"]);
        assert_eq!(
            text("article > p")?,
            vec![
                "1 Short title",
                "(1) This Act may be cited as the Test Act 2026.",
                "See the official Act page and short title provision."
            ]
        );
        assert_eq!(text("ol > li")?, vec!["First item", "Second item"]);
        let link_selector =
            Selector::parse("a[href]").map_err(|_| anyhow!("invalid fixture link selector"))?;
        let links = parsed
            .select(&link_selector)
            .filter_map(|link| link.value().attr("href"))
            .collect::<Vec<_>>();
        assert_eq!(
            links,
            vec![
                "https://www.legislation.sa.gov.au/lz?path=/c/a/test%20act%202026",
                "#provision-1"
            ]
        );
        let bookmark_selector = Selector::parse("a[name='provision-1']")
            .map_err(|_| anyhow!("invalid fixture bookmark selector"))?;
        assert!(parsed.select(&bookmark_selector).next().is_some());
        assert!(!normalized.contains("HYPERLINK"));
        Ok(())
    }

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

    #[test]
    fn converted_structure_preserves_headings_lists_and_resolved_links_without_libreoffice(
    ) -> Result<()> {
        let converted = r##"
            <html><body>
              <h1>PART 1</h1><h2>Preliminary</h2>
              <p><a name="provision-1"></a>1 Short title</p>
              <p>(1) This Act may be cited as the Test Act 2026.</p>
              <ol><li>First item</li><li>Second item</li></ol>
              <p>See the <a href="https://www.legislation.sa.gov.au/lz?path=/c/a/test%20act%202026">official Act page</a>
                 and <a href="#provision-1">short title provision</a>.</p>
            </body></html>
        "##;
        let normalized = normalize_south_australian_converted_html(
            converted,
            "https://www.legislation.sa.gov.au/lz?path=/c/a/test-act-2026",
        )?;
        assert_structural_document(&normalized)
    }

    #[test]
    fn converted_same_host_http_links_upgrade_to_https() -> Result<()> {
        let normalized = normalize_south_australian_converted_html(
            r#"<html><body><p>See the <a href="http://www.legislation.sa.gov.au/index.aspx?action=legref">legacy official link</a>.</p></body></html>"#,
            "https://www.legislation.sa.gov.au/lz?path=/c/a/test-act-2026",
        )?;
        assert_eq!(
            normalized,
            r#"<article><p>See the <a href="https://www.legislation.sa.gov.au/index.aspx?action=legref">legacy official link</a>.</p></article>"#
        );
        validate_south_australian_html(&normalized)
    }

    #[test]
    fn converted_unresolved_fragment_link_preserves_its_visible_text() -> Result<()> {
        let normalized = normalize_south_australian_converted_html(
            r##"<html><body><p>Use <a href="#missing-form">Form 1</a>. See <a href="#section-1">section 1</a>.</p><a name="section-1"></a></body></html>"##,
            "https://www.legislation.sa.gov.au/lz?path=/c/r/test-regulations-2026",
        )?;
        assert_eq!(
            normalized,
            r##"<article><p>Use Form 1. See <a href="#section-1">section 1</a>.</p><a name="section-1"></a></article>"##
        );
        validate_south_australian_html(&normalized)
    }

    #[test]
    #[ignore = "maintainer conversion gate: requires LibreOffice (soffice)"]
    fn maintainer_rtf_conversion_preserves_structure_and_bookmarks() -> Result<()> {
        let fixture = fs::read(Path::new(FIXTURES).join("structural.rtf"))?;
        let normalized = normalize_south_australian_rtf(
            &fixture,
            "https://www.legislation.sa.gov.au/lz?path=/c/a/test-act-2026",
        )?;
        assert_structural_document(&normalized)
    }

    #[test]
    fn validation_rejects_hyperlink_placeholders_and_malformed_headings() {
        assert!(validate_south_australian_html(
            r#"<article><h1><p>Part 1</p></h1><p>Text.</p></article>"#
        )
        .is_err());
        assert!(validate_south_australian_html(
            r#"<article><h1>Part 1</h1><p>See <a>hyperlink</a>.</p></article>"#
        )
        .is_err());
        assert!(validate_south_australian_html(
            r#"<article><h1>Part 1</h1><p>HYPERLINK "https://example.invalid"</p></article>"#
        )
        .is_err());
        assert!(validate_south_australian_html(
            r##"<article><h1>Part 1</h1><p><a id="section-1"></a>Text. See <a href="#section-1">section 1</a>.</p></article>"##
        )
        .is_ok());
        assert!(validate_south_australian_html(
            r##"<article><h1>Part 1</h1><p>See <a href="#missing">missing section</a>.</p></article>"##
        )
        .is_err());
        assert!(validate_south_australian_html(
            r##"<article><h1>Part 1</h1><p><a id="unsafe target"></a>See <a href="#unsafe target">section</a>.</p></article>"##
        )
        .is_err());
    }
}
