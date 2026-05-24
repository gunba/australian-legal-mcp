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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DocUri {
    Ato {
        doc_id: String,
        pit: Option<String>,
        view: Option<String>,
    },
}

impl DocUri {
    /// Render back to the canonical string form so error messages and tool
    /// responses can echo the parsed URI without losing detail.
    pub(crate) fn to_uri_string(&self) -> String {
        match self {
            DocUri::Ato { doc_id, pit, view } => {
                let mut s = format!("ato:{doc_id}");
                let mut qs = Vec::new();
                if let Some(p) = pit.as_deref().filter(|p| !p.is_empty()) {
                    qs.push(format!("pit={p}"));
                }
                if let Some(v) = view.as_deref().filter(|v| !v.is_empty()) {
                    qs.push(format!("view={v}"));
                }
                if !qs.is_empty() {
                    s.push('?');
                    s.push_str(&qs.join("&"));
                }
                s
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

fn parse_ato_body(body: &str) -> Result<DocUri> {
    let (path, query) = match body.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (body, None),
    };
    if path.is_empty() {
        bail!("`ato:` URI missing doc_id; example: `ato:JUD/2025ATC20-969/00002`");
    }
    let mut pit: Option<String> = None;
    let mut view: Option<String> = None;
    if let Some(q) = query {
        for pair in q.split('&').filter(|s| !s.is_empty()) {
            let (k, v) = pair
                .split_once('=')
                .ok_or_else(|| anyhow!("malformed query parameter `{pair}` in ato URI"))?;
            match k {
                "pit" => pit = Some(v.to_string()),
                "view" => view = Some(v.to_string()),
                other => {
                    bail!("unknown ato URI query parameter `{other}`; supported: `pit`, `view`")
                }
            }
        }
    }
    Ok(DocUri::Ato {
        doc_id: path.to_string(),
        pit,
        view,
    })
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
}
