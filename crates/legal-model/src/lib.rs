//! Stable source-qualified identities shared by acquisition, indexing, retrieval,
//! and MCP transport.

use percent_encoding::{percent_decode_str, percent_encode, AsciiSet, CONTROLS};
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fmt;
use std::str::FromStr;

const PUBLIC_ID_ENCODE_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'%')
    .add(b':')
    .add(b'?')
    .add(b'#')
    .add(b'[')
    .add(b']');
const MAX_PUBLIC_COMPONENT_BYTES: usize = 256;

pub fn encode_public_component(value: &str) -> String {
    percent_encode(value.as_bytes(), PUBLIC_ID_ENCODE_SET).to_string()
}

pub fn is_canonical_public_component(value: &str) -> bool {
    if value.is_empty() || value.len() > MAX_PUBLIC_COMPONENT_BYTES {
        return false;
    }
    percent_decode_str(value)
        .decode_utf8()
        .is_ok_and(|decoded| encode_public_component(&decoded) == value)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IdentityError(String);

impl IdentityError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for IdentityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for IdentityError {}

#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct SourceId(String);

impl SourceId {
    pub fn new(value: impl Into<String>) -> Result<Self, IdentityError> {
        let value = value.into();
        if value.is_empty()
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        {
            return Err(IdentityError::new(format!(
                "invalid legal source id `{value}`"
            )));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SourceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for SourceId {
    type Err = IdentityError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl TryFrom<String> for SourceId {
    type Error = IdentityError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<SourceId> for String {
    fn from(value: SourceId) -> Self {
        value.0
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SourceDescriptor {
    pub id: SourceId,
    pub display_name: String,
}

impl SourceDescriptor {
    pub fn new(id: SourceId, display_name: impl Into<String>) -> Result<Self, IdentityError> {
        let display_name = display_name.into();
        if display_name.trim().is_empty() {
            return Err(IdentityError::new("source display name must be nonempty"));
        }
        Ok(Self { id, display_name })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct DocumentId {
    pub source: SourceId,
    pub native_id: String,
}

impl DocumentId {
    pub fn new(source: SourceId, native_id: impl Into<String>) -> Result<Self, IdentityError> {
        let native_id = native_id.into();
        validate_component("native document id", &native_id)?;
        Ok(Self { source, native_id })
    }

    pub fn public_ref(&self) -> String {
        format!(
            "{}:{}",
            self.source,
            encode_public_component(&self.native_id)
        )
    }
}

impl fmt::Display for DocumentId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.public_ref())
    }
}

impl FromStr for DocumentId {
    type Err = IdentityError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (source, encoded_native_id) = value
            .split_once(':')
            .ok_or_else(|| IdentityError::new("document reference must be SOURCE:NATIVE_ID"))?;
        let native_id = percent_decode_str(encoded_native_id)
            .decode_utf8()
            .map_err(|_| IdentityError::new("document reference contains invalid UTF-8"))?
            .into_owned();
        if encode_public_component(&native_id) != encoded_native_id {
            return Err(IdentityError::new("document reference is not canonical"));
        }
        Self::new(source.parse()?, native_id)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct ChunkRef {
    pub generation: String,
    pub source: SourceId,
    pub chunk_id: u64,
}

impl ChunkRef {
    pub fn new(
        generation: impl Into<String>,
        source: SourceId,
        chunk_id: u64,
    ) -> Result<Self, IdentityError> {
        let generation = generation.into();
        if generation.is_empty()
            || !generation
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        {
            return Err(IdentityError::new("invalid corpus generation key"));
        }
        Ok(Self {
            generation,
            source,
            chunk_id,
        })
    }

    pub fn public_ref(&self) -> String {
        format!("{}:{}:{}", self.generation, self.source, self.chunk_id)
    }
}

impl fmt::Display for ChunkRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.public_ref())
    }
}

impl FromStr for ChunkRef {
    type Err = IdentityError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let mut fields = value.split(':');
        let generation = fields
            .next()
            .ok_or_else(|| IdentityError::new("chunk reference is missing its generation"))?;
        let source = fields
            .next()
            .ok_or_else(|| IdentityError::new("chunk reference is missing its source"))?;
        let chunk_id = fields
            .next()
            .ok_or_else(|| IdentityError::new("chunk reference is missing its chunk id"))?;
        if fields.next().is_some() {
            return Err(IdentityError::new(
                "chunk reference must be GENERATION:SOURCE:CHUNK_ID",
            ));
        }
        Self::new(
            generation,
            source.parse()?,
            chunk_id
                .parse()
                .map_err(|_| IdentityError::new("chunk id must be an unsigned integer"))?,
        )
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct AssetRef {
    pub source: SourceId,
    pub asset_id: String,
}

impl AssetRef {
    pub fn new(source: SourceId, asset_id: impl Into<String>) -> Result<Self, IdentityError> {
        let asset_id = asset_id.into();
        validate_component("asset id", &asset_id)?;
        Ok(Self { source, asset_id })
    }

    pub fn public_ref(&self) -> String {
        format!(
            "{}:{}",
            self.source,
            encode_public_component(&self.asset_id)
        )
    }
}

impl fmt::Display for AssetRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.public_ref())
    }
}

impl FromStr for AssetRef {
    type Err = IdentityError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (source, encoded_asset_id) = value
            .split_once(':')
            .ok_or_else(|| IdentityError::new("asset reference must be SOURCE:ASSET_ID"))?;
        let asset_id = percent_decode_str(encoded_asset_id)
            .decode_utf8()
            .map_err(|_| IdentityError::new("asset reference contains invalid UTF-8"))?
            .into_owned();
        if encode_public_component(&asset_id) != encoded_asset_id {
            return Err(IdentityError::new("asset reference is not canonical"));
        }
        Self::new(source.parse()?, asset_id)
    }
}

fn validate_component(name: &str, value: &str) -> Result<(), IdentityError> {
    if value.is_empty()
        || value.chars().any(char::is_control)
        || encode_public_component(value).len() > MAX_PUBLIC_COMPONENT_BYTES
    {
        return Err(IdentityError::new(format!(
            "{name} must be nonempty, bounded, and contain no control characters"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_ids_are_canonical_and_serde_validated() {
        let source: SourceId = "commonwealth-legislation".parse().expect("valid source");
        assert_eq!(source.as_str(), "commonwealth-legislation");
        assert_eq!(
            serde_json::to_string(&source).unwrap(),
            r#""commonwealth-legislation""#
        );
        assert!("Commonwealth".parse::<SourceId>().is_err());
        assert!(serde_json::from_str::<SourceId>(r#""bad/source""#).is_err());
    }

    #[test]
    fn document_refs_round_trip_reserved_and_unicode_native_ids() {
        let id = DocumentId::new("ato".parse().unwrap(), "JUD/example:one?point=✓").unwrap();
        let rendered = id.to_string();
        assert_eq!(rendered, "ato:JUD/example%3Aone%3Fpoint=%E2%9C%93");
        assert_eq!(rendered.parse::<DocumentId>().unwrap(), id);
    }

    #[test]
    fn public_references_reject_noncanonical_escapes_and_oversize_components() {
        assert!("ato:JUD%2fONE".parse::<DocumentId>().is_err());
        assert!("ato:JUD%2FONE".parse::<DocumentId>().is_err());
        assert!("frl:asset%41".parse::<AssetRef>().is_err());
        assert!(DocumentId::new("ato".parse().unwrap(), "x".repeat(257)).is_err());
        assert!(AssetRef::new("frl".parse().unwrap(), "✓".repeat(29)).is_err());
    }

    #[test]
    fn chunk_refs_bind_generation_source_and_internal_id() {
        let reference = ChunkRef::new("2026.07.11", "ato".parse().unwrap(), 42).unwrap();
        assert_eq!(reference.to_string(), "2026.07.11:ato:42");
        assert_eq!(
            reference.to_string().parse::<ChunkRef>().unwrap(),
            reference
        );
        assert!("2026.07.11:ato:-1".parse::<ChunkRef>().is_err());
        assert!("../live:ato:1".parse::<ChunkRef>().is_err());
    }

    #[test]
    fn asset_refs_are_source_qualified_and_reversible() {
        let reference =
            AssetRef::new("commonwealth-legislation".parse().unwrap(), "image:1.png").unwrap();
        assert_eq!(
            reference.to_string(),
            "commonwealth-legislation:image%3A1.png"
        );
        assert_eq!(
            reference.to_string().parse::<AssetRef>().unwrap(),
            reference
        );
    }

    #[test]
    fn source_descriptors_require_a_display_name() {
        assert!(SourceDescriptor::new("ato".parse().unwrap(), "ATO").is_ok());
        assert!(SourceDescriptor::new("ato".parse().unwrap(), "  ").is_err());
    }
}
