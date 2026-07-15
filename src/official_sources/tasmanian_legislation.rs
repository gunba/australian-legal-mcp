use super::*;
use serde::Deserialize;

pub(super) static ADAPTER: TasmanianLegislation = TasmanianLegislation;
pub(super) struct TasmanianLegislation;

const HOST: &str = "www.legislation.tas.gov.au";

#[derive(Deserialize)]
struct ProjectData {
    data: Option<OneOrMany<TasRecord>>,
    #[serde(rename = "totalCount")]
    total_count: Option<WrappedValue>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum OneOrMany<T> {
    One(T),
    Many(Vec<T>),
}

#[derive(Deserialize)]
struct TasRecord {
    id: WrappedValue,
    title: WrappedValue,
    repealed: WrappedValue,
    #[serde(rename = "first.valid.date")]
    first_valid_date: String,
}

#[derive(Deserialize)]
struct WrappedValue {
    #[serde(rename = "__value__")]
    #[serde(deserialize_with = "deserialize_wrapped_value")]
    value: String,
}

fn deserialize_wrapped_value<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Value {
        Text(String),
        Signed(i64),
        Unsigned(u64),
    }
    Ok(match Value::deserialize(deserializer)? {
        Value::Text(value) => value,
        Value::Signed(value) => value.to_string(),
        Value::Unsigned(value) => value.to_string(),
    })
}

impl OfficialAdapter for TasmanianLegislation {
    fn source_id(&self) -> &'static str {
        TASMANIAN_LEGISLATION_SOURCE_ID
    }
    fn display_name(&self) -> &'static str {
        "Tasmanian Legislation"
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
        const PAGE_SIZE: usize = 1_000;
        let pit = Utc::now().format("%Y%m%d%H%M%S").to_string();
        let first_pages = parallel_map(
            SOURCE_WORKER_CEILING,
            vec!["act.reprint", "reprint"],
            |print_type| {
                let url = project_data_url(print_type, &pit, 1, PAGE_SIZE)?;
                let payload = client.get_required(&url, "application/json", MAX_INDEX_BYTES)?;
                let page: ProjectData = serde_json::from_slice(&payload.bytes)
                    .with_context(|| format!("decoding Tasmanian {print_type} index"))?;
                let total = page
                    .total_count
                    .ok_or_else(|| anyhow!("Tasmanian index has no total count"))?
                    .value
                    .parse::<usize>()?;
                Ok((print_type, total, project_records(page.data)))
            },
        )?;
        let mut additional = Vec::new();
        for (print_type, total, _) in &first_pages {
            if *total > 10_000 {
                bail!("Tasmanian {print_type} inventory exceeds its bound");
            }
            for start in (PAGE_SIZE + 1..=*total).step_by(PAGE_SIZE) {
                additional.push((*print_type, start));
            }
        }
        let additional_pages =
            parallel_map(SOURCE_WORKER_CEILING, additional, |(print_type, start)| {
                let url = project_data_url(print_type, &pit, start, PAGE_SIZE)?;
                let payload = client.get_required(&url, "application/json", MAX_INDEX_BYTES)?;
                let page: ProjectData = serde_json::from_slice(&payload.bytes)
                    .with_context(|| format!("decoding Tasmanian {print_type} page {start}"))?;
                Ok((print_type, project_records(page.data)))
            })?;
        let mut by_type = BTreeMap::new();
        for (print_type, total, records) in first_pages {
            by_type.insert(print_type, (total, records));
        }
        for (print_type, records) in additional_pages {
            by_type
                .get_mut(print_type)
                .ok_or_else(|| anyhow!("Tasmanian index returned an unknown print type"))?
                .1
                .extend(records);
        }
        let mut documents = Vec::new();
        for (print_type, (total, records)) in by_type {
            if records.len() != total {
                bail!(
                    "Tasmanian {print_type} index expected {total} records but returned {}",
                    records.len()
                );
            }
            let document_type = if print_type == "act.reprint" {
                "primary_legislation"
            } else {
                "secondary_legislation"
            };
            for record in records {
                if record.repealed.value != "N" {
                    continue;
                }
                let date = record
                    .first_valid_date
                    .get(..10)
                    .filter(|value| chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d").is_ok())
                    .ok_or_else(|| anyhow!("Tasmanian record has invalid first-valid date"))?
                    .to_owned();
                let url = format!(
                    "https://{HOST}/view/whole/html/inforce/current/{}",
                    record.id.value
                );
                let title = record.title.value.trim().to_owned();
                documents.push(DiscoveredDocument {
                    native_id: record.id.value.clone(),
                    upstream_version: format!("{date}/{}", record.id.value),
                    title: title.clone(),
                    document_type: document_type.to_owned(),
                    date: Some(date),
                    citation: Some(title),
                    canonical_url: url.clone(),
                    renditions: vec![Rendition {
                        url,
                        kind: RenditionKind::Html,
                    }],
                });
            }
        }
        Ok(documents)
    }

    fn acquire(
        &self,
        client: &OfficialHttpClient,
        entry: &DiscoveredDocument,
    ) -> Result<Option<AcquiredDocument>> {
        let payload = client.get(&entry.renditions[0].url, "text/html", MAX_DOCUMENT_BYTES)?;
        if payload.status == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !payload.status.is_success() {
            bail!(
                "Tasmanian legislation document returned HTTP {}",
                payload.status
            );
        }
        let html = decode_utf8(&payload.bytes)?.replace("&#150;", "&#8211;");
        if html.contains("Content Not Found") {
            bail!("Tasmanian legislation page reports that its content was not found");
        }
        let normalized = normalize_html(
            &html,
            &entry.canonical_url,
            HtmlRules {
                content_selector: "#fragview",
                drop_ids: &[],
                drop_classes: &["view-history-note"],
                heading_classes: &["HeadingParagraph"],
            },
        )?;
        Ok(Some(make_acquired_html(
            normalized,
            entry.canonical_url.clone(),
        )))
    }
}

fn project_records(data: Option<OneOrMany<TasRecord>>) -> Vec<TasRecord> {
    match data {
        None => Vec::new(),
        Some(OneOrMany::One(record)) => vec![record],
        Some(OneOrMany::Many(records)) => records,
    }
}

fn project_data_url(print_type: &str, pit: &str, start: usize, count: usize) -> Result<String> {
    let expression = format!(
        "PrintType={print_type} AND Repealed<>Y AND Amending<>pure AND PitValid=@pointInTime({pit})"
    );
    let mut url = Url::parse(&format!("https://{HOST}/projectdata"))?;
    url.query_pairs_mut()
        .append_pair("ds", "EnAct-BrowseDataSource")
        .append_pair("start", &start.to_string())
        .append_pair("count", &count.to_string())
        .append_pair("sortField", "new.sort.title")
        .append_pair("sortDirection", "asc")
        .append_pair("expression", &expression)
        .append_pair("collection", "");
    Ok(url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn project_data_accepts_one_or_many_records() -> Result<()> {
        let page: ProjectData = serde_json::from_str(
            r#"{"data":{"id":{"__value__":"act-1"},"title":{"__value__":"Act"},"repealed":{"__value__":"N"},"first.valid.date":"2025-01-01T00:00:00"},"totalCount":{"__value__":"1"}}"#,
        )?;
        assert!(matches!(page.data, Some(OneOrMany::One(_))));
        Ok(())
    }

    #[test]
    fn project_data_query_uses_the_current_official_sort_contract() -> Result<()> {
        let url = Url::parse(&project_data_url(
            "act.reprint",
            "20260712000000",
            1,
            1_000,
        )?)?;
        let pairs = url.query_pairs().collect::<BTreeMap<_, _>>();
        assert_eq!(
            pairs.get("sortField").map(|value| value.as_ref()),
            Some("new.sort.title")
        );
        let expression = pairs
            .get("expression")
            .ok_or_else(|| anyhow!("missing expression"))?;
        assert!(expression.contains("Repealed<>Y"));
        assert!(expression.contains("Amending<>pure"));
        Ok(())
    }
}
