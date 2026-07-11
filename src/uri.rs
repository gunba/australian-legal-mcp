//! Document URI parsing.
//!
//! Live document identifiers flow through a single typed URI so the `fetch`
//! tool and its CLI counterpart can dispatch without per-call source
//! detection. Scheme:
//!
//! - `ato:<doc_id>[?pit=...&view=...]` — live-fetch from ato.gov.au's law
//!   database. `pit` and `view` correspond to the ATO query params of the
//!   same name and are preserved verbatim.
//!
//! Bare strings without a scheme are rejected with a message that tells the
//! caller what the supported form is.

use anyhow::{anyhow, bail, Result};
use url::Url;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DocUri {
    Ato {
        doc_id: String,
        pit: Option<String>,
        view: Option<String>,
    },
}

impl DocUri {
    /// Render to a canonical URI. URL's path/query serializers provide the
    /// percent-encoding; no URI field is interpolated into a query string.
    pub(crate) fn to_uri_string(&self) -> String {
        match self {
            DocUri::Ato { doc_id, pit, view } => {
                let mut url = Url::parse("https://ato.invalid/").expect("static URL is valid");
                url.set_path(doc_id);
                if pit.is_some() || view.is_some() {
                    let mut query = url.query_pairs_mut();
                    if let Some(pit) = pit {
                        query.append_pair("pit", pit);
                    }
                    if let Some(view) = view {
                        query.append_pair("view", view);
                    }
                }
                let path = url.path().strip_prefix('/').unwrap_or(url.path());
                match url.query() {
                    Some(query) => format!("ato:{path}?{query}"),
                    None => format!("ato:{path}"),
                }
            }
        }
    }
}

pub(crate) fn parse_doc_uri(input: &str) -> Result<DocUri> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        bail!("empty URI; expected `ato:<doc_id>`");
    }
    let Some((scheme, rest)) = trimmed.split_once(':') else {
        bail!(
            "missing URI scheme in `{input}`; use `ato:<doc_id>` for ATO live-fetch \
             (e.g. `ato:JUD/2025ATC20-969/00002`)"
        );
    };
    match scheme {
        "ato" => parse_ato_body(rest),
        other => bail!("unknown URI scheme `{other}` in `{input}`; supported scheme: `ato`"),
    }
}

fn decode_uri_component(value: &str, field: &str) -> Result<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let pair = bytes
                .get(i + 1..i + 3)
                .ok_or_else(|| anyhow!("invalid percent-encoding in ato URI {field}"))?;
            let hex = std::str::from_utf8(pair).expect("ASCII slice");
            let byte = u8::from_str_radix(hex, 16)
                .map_err(|_| anyhow!("invalid percent-encoding in ato URI {field}"))?;
            decoded.push(byte);
            i += 3;
        } else {
            decoded.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(decoded).map_err(|_| anyhow!("ato URI {field} is not valid UTF-8"))
}

fn validate_doc_id(doc_id: &str) -> Result<()> {
    if doc_id.is_empty() {
        bail!("`ato:` URI missing doc_id; example: `ato:JUD/2025ATC20-969/00002`");
    }
    if doc_id.starts_with('/')
        || doc_id.contains('\\')
        || doc_id.chars().any(|c| c.is_control() || c.is_whitespace())
        || doc_id
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | ".."))
        || !doc_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.'))
    {
        bail!("invalid ato URI doc_id `{doc_id}`");
    }
    Ok(())
}

fn parse_ato_body(body: &str) -> Result<DocUri> {
    if body.matches('?').count() > 1 || body.contains('#') {
        bail!("malformed ato URI");
    }
    let (encoded_path, query) = match body.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (body, None),
    };
    let doc_id = decode_uri_component(encoded_path, "doc_id")?;
    validate_doc_id(&doc_id)?;

    let mut pit = None;
    let mut view = None;
    if let Some(query) = query {
        if query.is_empty() {
            bail!("empty query in ato URI");
        }
        for pair in query.split('&') {
            if pair.is_empty() {
                bail!("empty query parameter in ato URI");
            }
            let (encoded_key, encoded_value) = pair
                .split_once('=')
                .ok_or_else(|| anyhow!("malformed query parameter `{pair}` in ato URI"))?;
            let key = decode_uri_component(encoded_key, "query key")?;
            let value = decode_uri_component(encoded_value, &key)?;
            if value.is_empty() {
                bail!("empty ato URI query parameter `{key}`");
            }
            match key.as_str() {
                "pit" => {
                    if pit.is_some() {
                        bail!("duplicate ato URI query parameter `pit`");
                    }
                    if !(8..=14).contains(&value.len())
                        || !value.bytes().all(|b| b.is_ascii_digit())
                        || chrono::NaiveDate::parse_from_str(&value[..8], "%Y%m%d").is_err()
                    {
                        bail!(
                            "invalid ato URI `pit`; expected 8 to 14 digits beginning with a valid date"
                        );
                    }
                    pit = Some(value);
                }
                "view" => {
                    if view.is_some() {
                        bail!("duplicate ato URI query parameter `view`");
                    }
                    if value.len() > 32
                        || !value
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
                    {
                        bail!("invalid ato URI `view`");
                    }
                    view = Some(value);
                }
                other => {
                    bail!("unknown ato URI query parameter `{other}`; supported: `pit`, `view`")
                }
            }
        }
    }
    Ok(DocUri::Ato { doc_id, pit, view })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bare_ato_uri() {
        let parsed = parse_doc_uri("ato:JUD/2025ATC20-969/00002").unwrap();
        assert_eq!(
            parsed,
            DocUri::Ato {
                doc_id: "JUD/2025ATC20-969/00002".to_string(),
                pit: None,
                view: None,
            }
        );
    }

    #[test]
    fn parses_ato_uri_with_pit() {
        let parsed = parse_doc_uri("ato:PAC/19360027/26?pit=19960320000001").unwrap();
        assert_eq!(
            parsed,
            DocUri::Ato {
                doc_id: "PAC/19360027/26".to_string(),
                pit: Some("19960320000001".to_string()),
                view: None,
            }
        );
    }

    #[test]
    fn parses_ato_uri_with_view() {
        let parsed = parse_doc_uri("ato:PAC/19360027/26?view=HISTFT").unwrap();
        assert_eq!(
            parsed,
            DocUri::Ato {
                doc_id: "PAC/19360027/26".to_string(),
                pit: None,
                view: Some("HISTFT".to_string()),
            }
        );
    }

    #[test]
    fn parses_ato_uri_with_pit_and_view() {
        let parsed = parse_doc_uri("ato:PAC/19360027/26?pit=19960320000001&view=HISTFT").unwrap();
        assert_eq!(
            parsed,
            DocUri::Ato {
                doc_id: "PAC/19360027/26".to_string(),
                pit: Some("19960320000001".to_string()),
                view: Some("HISTFT".to_string()),
            }
        );
    }

    #[test]
    fn rejects_unknown_ato_query_param() {
        let err = parse_doc_uri("ato:JUD/X/Y?wat=1").unwrap_err();
        assert!(err
            .to_string()
            .contains("unknown ato URI query parameter `wat`"));
    }

    #[test]
    fn rejects_malformed_query_pair() {
        let err = parse_doc_uri("ato:JUD/X/Y?nokeyhere").unwrap_err();
        assert!(err.to_string().contains("malformed query parameter"));
    }

    #[test]
    fn rejects_missing_scheme() {
        let err = parse_doc_uri("JUD/2025ATC20-969/00002").unwrap_err();
        assert!(err.to_string().contains("missing URI scheme"));
    }

    #[test]
    fn rejects_unknown_scheme() {
        let err = parse_doc_uri("nzlii:nz/cases/NZSC/2020/1").unwrap_err();
        assert!(err.to_string().contains("unknown URI scheme `nzlii`"));
    }

    #[test]
    fn rejects_empty_input() {
        let err = parse_doc_uri("").unwrap_err();
        assert!(err.to_string().contains("empty URI"));
    }

    #[test]
    fn rejects_empty_ato_doc_id() {
        let err = parse_doc_uri("ato:").unwrap_err();
        assert!(err.to_string().contains("missing doc_id"));
    }

    #[test]
    fn roundtrips_to_uri_string() {
        let cases = [
            "ato:JUD/2025ATC20-969/00002",
            "ato:PAC/19360027/26?pit=19960320000001",
            "ato:PAC/19360027/26?view=HISTFT",
            "ato:PAC/19360027/26?pit=19960320000001&view=HISTFT",
        ];
        for input in cases {
            let parsed = parse_doc_uri(input).unwrap();
            assert_eq!(parsed.to_uri_string(), input, "input: {input}");
        }
    }

    #[test]
    fn encodes_fields_and_decodes_once() {
        let uri = DocUri::Ato {
            doc_id: "JUD/A B/1".to_string(),
            pit: Some("20250101".to_string()),
            view: Some("HIST&FT".to_string()),
        };
        assert_eq!(
            uri.to_uri_string(),
            "ato:JUD/A%20B/1?pit=20250101&view=HIST%26FT"
        );
        assert_eq!(
            parse_doc_uri("ato:JUD/A%252FB/1").unwrap_err().to_string(),
            "invalid ato URI doc_id `JUD/A%2FB/1`"
        );
    }

    #[test]
    fn rejects_duplicate_invalid_and_ambiguous_fields() {
        for input in [
            "ato:JUD/X/Y?pit=20250101&pit=20250102",
            "ato:JUD/X/Y?view=A&view=B",
            "ato:JUD/X/Y?pit=2025-01-01",
            "ato:JUD/X/Y?pit=20250230",
            "ato:JUD/../Y",
            "ato:JUD%2F..%2FY",
            "ato:JUD/X/Y?",
            "ato:JUD/X/Y?pit=%GG",
            "ato:JUD/X/Y?pit=20250101&&view=HISTFT",
        ] {
            assert!(parse_doc_uri(input).is_err(), "accepted {input}");
        }
    }
}
