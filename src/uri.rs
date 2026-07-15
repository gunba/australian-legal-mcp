//! Canonical source-qualified document URIs used by the live `fetch` tool.

use anyhow::{anyhow, bail, Context, Result};
use legal_model::{DocumentId, SourceId};
use url::Url;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DocUri {
    document: DocumentId,
    pit: Option<String>,
    view: Option<String>,
}

impl DocUri {
    pub(crate) fn new(
        document: DocumentId,
        pit: Option<String>,
        view: Option<String>,
    ) -> Result<Self> {
        validate_qualifiers(&document.source, pit.as_deref(), view.as_deref())?;
        Ok(Self {
            document,
            pit,
            view,
        })
    }

    pub(crate) fn into_parts(self) -> (DocumentId, Option<String>, Option<String>) {
        (self.document, self.pit, self.view)
    }

    pub(crate) fn to_uri_string(&self) -> String {
        let mut rendered = format!(
            "legal://{}/{}",
            self.document.source,
            encode_path_segment(&self.document.native_id)
        );
        if self.pit.is_some() || self.view.is_some() {
            let mut query = url::form_urlencoded::Serializer::new(String::new());
            if let Some(pit) = &self.pit {
                query.append_pair("pit", pit);
            }
            if let Some(view) = &self.view {
                query.append_pair("view", view);
            }
            rendered.push('?');
            rendered.push_str(&query.finish());
        }
        rendered
    }
}

pub(crate) fn parse_doc_uri(input: &str) -> Result<DocUri> {
    if input.is_empty() || input.trim() != input {
        bail!("fetch URI must be a nonempty canonical `legal://` URI");
    }
    let parsed = Url::parse(input).context("fetch URI must be a valid URL")?;
    if parsed.scheme() != "legal" {
        bail!("fetch URI must use the `legal` scheme");
    }
    if !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.port().is_some()
        || parsed.fragment().is_some()
    {
        bail!("fetch URI contains unsupported authority or fragment fields");
    }
    let source_text = parsed
        .host_str()
        .ok_or_else(|| anyhow!("fetch URI is missing its legal source"))?;
    let source: SourceId = source_text
        .parse()
        .map_err(|error| anyhow!("invalid fetch URI source `{source_text}`: {error}"))?;
    let encoded_native_id = parsed
        .path()
        .strip_prefix('/')
        .ok_or_else(|| anyhow!("fetch URI has an invalid document path"))?;
    if encoded_native_id.is_empty() || encoded_native_id.contains('/') {
        bail!("fetch URI must contain one percent-encoded native document id");
    }
    let native_id = decode_path_segment(encoded_native_id)?;
    let document = DocumentId::new(source, native_id)
        .context("fetch URI contains an invalid document identity")?;

    let mut pit = None;
    let mut view = None;
    if let Some(query) = parsed.query() {
        if query.is_empty() {
            bail!("fetch URI query must not be empty");
        }
        for (key, value) in parsed.query_pairs() {
            if value.is_empty() {
                bail!("fetch URI query parameter `{key}` must not be empty");
            }
            match key.as_ref() {
                "pit" => {
                    if pit.replace(value.into_owned()).is_some() {
                        bail!("duplicate fetch URI query parameter `pit`");
                    }
                }
                "view" => {
                    if view.replace(value.into_owned()).is_some() {
                        bail!("duplicate fetch URI query parameter `view`");
                    }
                }
                other => {
                    bail!("unknown fetch URI query parameter `{other}`; supported: `pit`, `view`")
                }
            }
        }
    }
    let uri = DocUri::new(document, pit, view)?;
    if uri.to_uri_string() != input {
        bail!("fetch URI is not in canonical `legal://SOURCE/NATIVE_ID` form");
    }
    Ok(uri)
}

fn validate_qualifiers(source: &SourceId, pit: Option<&str>, view: Option<&str>) -> Result<()> {
    if (pit.is_some() || view.is_some()) && source.as_str() != "ato" {
        bail!("fetch URI qualifiers are not supported for source `{source}`");
    }
    if let Some(pit) = pit {
        if !(8..=14).contains(&pit.len())
            || !pit.bytes().all(|byte| byte.is_ascii_digit())
            || chrono::NaiveDate::parse_from_str(&pit[..8], "%Y%m%d").is_err()
        {
            bail!("invalid fetch URI `pit`; expected 8 to 14 digits beginning with a valid date");
        }
    }
    if let Some(view) = view {
        if view.len() > 32
            || !view
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            bail!("invalid fetch URI `view`");
        }
    }
    Ok(())
}

fn encode_path_segment(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            write!(&mut encoded, "%{byte:02X}").expect("writing to String cannot fail");
        }
    }
    encoded
}

fn decode_path_segment(value: &str) -> Result<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let pair = bytes
                .get(index + 1..index + 3)
                .ok_or_else(|| anyhow!("fetch URI contains invalid percent-encoding"))?;
            let hex = std::str::from_utf8(pair).expect("percent-encoding digits are ASCII");
            decoded.push(
                u8::from_str_radix(hex, 16)
                    .map_err(|_| anyhow!("fetch URI contains invalid percent-encoding"))?,
            );
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).map_err(|_| anyhow!("fetch URI document id is not valid UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ato(native_id: &str, pit: Option<&str>, view: Option<&str>) -> DocUri {
        DocUri::new(
            DocumentId::new("ato".parse().unwrap(), native_id).unwrap(),
            pit.map(str::to_string),
            view.map(str::to_string),
        )
        .unwrap()
    }

    #[test]
    fn canonical_legal_uri_round_trips_source_native_id_and_qualifiers() {
        for uri in [
            ato("JUD/2025ATC20-969/00002", None, None),
            ato("PAC/19360027/26", Some("19960320000001"), None),
            ato("PAC/19360027/26", None, Some("HISTFT")),
            ato(
                "JUD/example:one?point=✓",
                Some("19960320000001"),
                Some("HISTFT"),
            ),
        ] {
            let rendered = uri.to_uri_string();
            assert_eq!(parse_doc_uri(&rendered).unwrap(), uri);
            assert!(!rendered.contains("/JUD/"));
        }
    }

    #[test]
    fn rejects_alternate_schemes_noncanonical_paths_and_queries() {
        for input in [
            "ato:JUD/2025ATC20-969/00002",
            "JUD/2025ATC20-969/00002",
            "legal://ato/JUD/2025ATC20-969/00002",
            "legal://ato/JUD%2fX",
            "legal://ato/JUD%2FX?",
            "legal://ato/JUD%2FX?wat=1",
            "legal://ato/JUD%2FX?pit=20250101&pit=20250102",
        ] {
            assert!(parse_doc_uri(input).is_err(), "accepted {input}");
        }
    }
}
