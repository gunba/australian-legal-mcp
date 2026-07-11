//! Validated, source-qualified data exchanged by legal source adapters.

use legal_model::{AssetRef, DocumentId};
use serde::{de::Error as _, Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use url::Url;

const NORMALIZED_HASH_VERSION: u8 = 1;

/// A validation failure in the source adapter data contract.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidationError {
    field: &'static str,
    message: String,
}

impl ValidationError {
    fn new(field: &'static str, message: impl Into<String>) -> Self {
        Self {
            field,
            message: message.into(),
        }
    }

    /// The contract field that failed validation.
    pub fn field(&self) -> &'static str {
        self.field
    }

    /// The reason the field failed validation.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.field, self.message)
    }
}

impl Error for ValidationError {}

/// One integrity-pinned source inventory entry.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SourceInventoryRecord {
    pub document: DocumentId,
    pub upstream_version: Option<String>,
    pub canonical_url: String,
    pub document_type: String,
    pub title: String,
    pub date: Option<String>,
    pub payload_path: String,
    pub payload_sha256: String,
    pub payload_size: u64,
    pub media_type: String,
}

impl SourceInventoryRecord {
    /// Constructs and validates an inventory record.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        document: DocumentId,
        upstream_version: Option<String>,
        canonical_url: impl Into<String>,
        document_type: impl Into<String>,
        title: impl Into<String>,
        date: Option<String>,
        payload_path: impl Into<String>,
        payload_sha256: impl Into<String>,
        payload_size: u64,
        media_type: impl Into<String>,
    ) -> Result<Self, ValidationError> {
        let record = Self {
            document,
            upstream_version,
            canonical_url: canonical_url.into(),
            document_type: document_type.into(),
            title: title.into(),
            date,
            payload_path: payload_path.into(),
            payload_sha256: payload_sha256.into(),
            payload_size,
            media_type: media_type.into(),
        };
        record.validate()?;
        Ok(record)
    }

    /// Validates all inventory metadata and payload integrity fields.
    pub fn validate(&self) -> Result<(), ValidationError> {
        validate_identity_component("document.native_id", &self.document.native_id)?;
        validate_optional_text("upstream_version", self.upstream_version.as_deref())?;
        validate_https_url("canonical_url", &self.canonical_url)?;
        validate_required_text("document_type", &self.document_type)?;
        validate_required_text("title", &self.title)?;
        validate_optional_text("date", self.date.as_deref())?;
        validate_payload_path(&self.payload_path)?;
        validate_sha256("payload_sha256", &self.payload_sha256)?;
        if self.payload_size == 0 {
            return Err(ValidationError::new(
                "payload_size",
                "must be greater than zero",
            ));
        }
        validate_media_type("media_type", &self.media_type)?;
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceInventoryRecordWire {
    document: DocumentId,
    upstream_version: Option<String>,
    canonical_url: String,
    document_type: String,
    title: String,
    date: Option<String>,
    payload_path: String,
    payload_sha256: String,
    payload_size: u64,
    media_type: String,
}

impl<'de> Deserialize<'de> for SourceInventoryRecord {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = SourceInventoryRecordWire::deserialize(deserializer)?;
        Self::new(
            wire.document,
            wire.upstream_version,
            wire.canonical_url,
            wire.document_type,
            wire.title,
            wire.date,
            wire.payload_path,
            wire.payload_sha256,
            wire.payload_size,
            wire.media_type,
        )
        .map_err(D::Error::custom)
    }
}

/// One binary asset retained by a normalized document.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct NormalizedAsset {
    pub asset: AssetRef,
    pub media_type: String,
    pub alt: Option<String>,
    pub title: Option<String>,
    pub sha256: String,
    pub data: Vec<u8>,
}

impl NormalizedAsset {
    /// Constructs and validates a normalized asset.
    pub fn new(
        asset: AssetRef,
        media_type: impl Into<String>,
        alt: Option<String>,
        title: Option<String>,
        sha256: impl Into<String>,
        data: Vec<u8>,
    ) -> Result<Self, ValidationError> {
        let asset = Self {
            asset,
            media_type: media_type.into(),
            alt,
            title,
            sha256: sha256.into(),
            data,
        };
        asset.validate()?;
        Ok(asset)
    }

    /// Validates asset identity, metadata, and bytes against its digest.
    pub fn validate(&self) -> Result<(), ValidationError> {
        validate_identity_component("asset.asset_id", &self.asset.asset_id)?;
        validate_media_type("media_type", &self.media_type)?;
        validate_optional_text("alt", self.alt.as_deref())?;
        validate_optional_text("title", self.title.as_deref())?;
        validate_sha256("sha256", &self.sha256)?;
        if self.data.is_empty() {
            return Err(ValidationError::new("data", "must be nonempty"));
        }
        if sha256_bytes(&self.data) != self.sha256 {
            return Err(ValidationError::new(
                "sha256",
                "does not match the asset data",
            ));
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NormalizedAssetWire {
    asset: AssetRef,
    media_type: String,
    alt: Option<String>,
    title: Option<String>,
    sha256: String,
    data: Vec<u8>,
}

impl<'de> Deserialize<'de> for NormalizedAsset {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = NormalizedAssetWire::deserialize(deserializer)?;
        Self::new(
            wire.asset,
            wire.media_type,
            wire.alt,
            wire.title,
            wire.sha256,
            wire.data,
        )
        .map_err(D::Error::custom)
    }
}

/// A source document after shared normalization.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct NormalizedDocument {
    pub inventory: SourceInventoryRecord,
    pub html: String,
    pub assets: Vec<NormalizedAsset>,
}

impl NormalizedDocument {
    /// Constructs and validates a normalized document.
    pub fn new(
        inventory: SourceInventoryRecord,
        html: impl Into<String>,
        assets: Vec<NormalizedAsset>,
    ) -> Result<Self, ValidationError> {
        let document = Self {
            inventory,
            html: html.into(),
            assets,
        };
        document.validate()?;
        Ok(document)
    }

    /// Validates the inventory, normalized HTML, and source-qualified assets.
    pub fn validate(&self) -> Result<(), ValidationError> {
        self.inventory.validate()?;
        validate_html(&self.html)?;

        let expected_source = &self.inventory.document.source;
        let mut asset_ids = BTreeSet::new();
        for asset in &self.assets {
            asset.validate()?;
            if &asset.asset.source != expected_source {
                return Err(ValidationError::new(
                    "assets",
                    format!(
                        "asset `{}` belongs to source `{}` rather than document source `{}`",
                        asset.asset, asset.asset.source, expected_source
                    ),
                ));
            }
            if !asset_ids.insert(asset.asset.clone()) {
                return Err(ValidationError::new(
                    "assets",
                    format!("duplicate asset `{}`", asset.asset),
                ));
            }
        }
        Ok(())
    }

    /// Returns the lowercase SHA-256 of the versioned normalized representation.
    ///
    /// Assets are ordered by their source-qualified identity before hashing, so
    /// adapter discovery order cannot change the digest.
    pub fn normalized_sha256(&self) -> Result<String, ValidationError> {
        self.validate()?;
        let mut assets = self.assets.iter().collect::<Vec<_>>();
        assets.sort_unstable_by(|left, right| left.asset.cmp(&right.asset));
        let projection = NormalizedHashProjection {
            version: NORMALIZED_HASH_VERSION,
            inventory: &self.inventory,
            html: &self.html,
            assets,
        };
        let bytes = serde_json::to_vec(&projection).map_err(|error| {
            ValidationError::new(
                "normalized_document",
                format!("could not serialize normalized hash projection: {error}"),
            )
        })?;
        Ok(sha256_bytes(&bytes))
    }
}

#[derive(Serialize)]
struct NormalizedHashProjection<'a> {
    version: u8,
    inventory: &'a SourceInventoryRecord,
    html: &'a str,
    assets: Vec<&'a NormalizedAsset>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NormalizedDocumentWire {
    inventory: SourceInventoryRecord,
    html: String,
    assets: Vec<NormalizedAsset>,
}

impl<'de> Deserialize<'de> for NormalizedDocument {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = NormalizedDocumentWire::deserialize(deserializer)?;
        Self::new(wire.inventory, wire.html, wire.assets).map_err(D::Error::custom)
    }
}

/// Computes a lowercase, 64-character SHA-256 digest.
pub fn sha256_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Reads validated inventory JSONL without consuming more than `max_bytes`.
///
/// Empty lines are ignored. Duplicate document identities and malformed UTF-8
/// or JSON are rejected.
pub fn read_inventory_jsonl(
    path: impl AsRef<Path>,
    max_bytes: u64,
) -> io::Result<Vec<SourceInventoryRecord>> {
    let path = path.as_ref();
    let file = File::open(path)?;
    let file_len = file.metadata()?.len();
    if file_len > max_bytes {
        return Err(limit_error(path, max_bytes));
    }

    let mut reader = BufReader::new(file).take(max_bytes.saturating_add(1));
    let mut line = Vec::new();
    let mut line_number = 0usize;
    let mut total_bytes = 0u64;
    let mut records = Vec::new();
    let mut documents = BTreeSet::new();

    loop {
        line.clear();
        let bytes_read = reader.read_until(b'\n', &mut line)?;
        if bytes_read == 0 {
            break;
        }
        line_number += 1;
        total_bytes = total_bytes
            .checked_add(bytes_read as u64)
            .ok_or_else(|| limit_error(path, max_bytes))?;
        if total_bytes > max_bytes {
            return Err(limit_error(path, max_bytes));
        }

        if line.last() == Some(&b'\n') {
            line.pop();
        }
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        let text = std::str::from_utf8(&line).map_err(|error| {
            invalid_data(format!(
                "{} line {line_number} is not UTF-8: {error}",
                path.display()
            ))
        })?;
        if text.trim().is_empty() {
            continue;
        }

        let record: SourceInventoryRecord = serde_json::from_str(text).map_err(|error| {
            invalid_data(format!(
                "invalid inventory record at {} line {line_number}: {error}",
                path.display()
            ))
        })?;
        if !documents.insert(record.document.clone()) {
            return Err(invalid_data(format!(
                "duplicate inventory document `{}` at {} line {line_number}",
                record.document,
                path.display()
            )));
        }
        records.push(record);
    }

    Ok(records)
}

/// Atomically writes validated inventory JSONL sorted by `DocumentId`.
///
/// The replacement is created in the destination directory, flushed and
/// synced before it is persisted. Existing file permissions are retained.
pub fn write_inventory_jsonl_atomic(
    path: impl AsRef<Path>,
    records: &[SourceInventoryRecord],
) -> io::Result<()> {
    let path = path.as_ref();
    let parent = destination_parent(path)?;

    let mut sorted = records.iter().collect::<Vec<_>>();
    sorted.sort_unstable_by(|left, right| left.document.cmp(&right.document));
    for (index, record) in sorted.iter().enumerate() {
        record.validate().map_err(|error| {
            invalid_data(format!(
                "invalid inventory record {} (`{}`): {error}",
                index + 1,
                record.document
            ))
        })?;
        if index > 0 && sorted[index - 1].document == record.document {
            return Err(invalid_data(format!(
                "duplicate inventory document `{}`",
                record.document
            )));
        }
    }

    let existing_permissions = match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "inventory destination is not a regular file: {}",
                        path.display()
                    ),
                ));
            }
            Some(metadata.permissions())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };

    let mut temporary = tempfile::NamedTempFile::new_in(&parent)?;
    if let Some(permissions) = existing_permissions {
        temporary.as_file().set_permissions(permissions)?;
    }

    {
        let mut writer = BufWriter::new(temporary.as_file_mut());
        for record in sorted {
            let json = serde_json::to_vec(record).map_err(|error| {
                invalid_data(format!(
                    "could not serialize inventory document `{}`: {error}",
                    record.document
                ))
            })?;
            writer.write_all(&json)?;
            writer.write_all(b"\n")?;
        }
        writer.flush()?;
    }
    temporary.as_file().sync_all()?;

    let persisted = temporary.persist(path).map_err(|error| error.error)?;
    persisted.sync_all()?;
    sync_directory(&parent)?;
    Ok(())
}

fn validate_required_text(field: &'static str, value: &str) -> Result<(), ValidationError> {
    if value.trim().is_empty() {
        return Err(ValidationError::new(field, "must be nonempty"));
    }
    if value.trim() != value {
        return Err(ValidationError::new(
            field,
            "must not have leading or trailing whitespace",
        ));
    }
    if value.chars().any(char::is_control) {
        return Err(ValidationError::new(
            field,
            "must not contain control characters",
        ));
    }
    Ok(())
}

fn validate_optional_text(field: &'static str, value: Option<&str>) -> Result<(), ValidationError> {
    if let Some(value) = value {
        validate_required_text(field, value)?;
    }
    Ok(())
}

fn validate_identity_component(field: &'static str, value: &str) -> Result<(), ValidationError> {
    if value.trim().is_empty() {
        return Err(ValidationError::new(field, "must be nonempty"));
    }
    if value.trim() != value {
        return Err(ValidationError::new(
            field,
            "must not have leading or trailing whitespace",
        ));
    }
    if value.chars().any(char::is_control) {
        return Err(ValidationError::new(
            field,
            "must not contain control characters",
        ));
    }
    Ok(())
}

fn validate_https_url(field: &'static str, value: &str) -> Result<(), ValidationError> {
    validate_required_text(field, value)?;
    if value.chars().any(char::is_whitespace) {
        return Err(ValidationError::new(
            field,
            "must not contain unescaped whitespace",
        ));
    }
    let Some(authority_and_path) = value.strip_prefix("https://") else {
        return Err(ValidationError::new(field, "must use the https scheme"));
    };
    if authority_and_path.is_empty() || authority_and_path.starts_with('/') {
        return Err(ValidationError::new(field, "must include a host"));
    }
    let parsed = Url::parse(value)
        .map_err(|error| ValidationError::new(field, format!("is not a valid URL: {error}")))?;
    if parsed.scheme() != "https" {
        return Err(ValidationError::new(field, "must use the https scheme"));
    }
    if parsed.host_str().is_none() {
        return Err(ValidationError::new(field, "must include a host"));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(ValidationError::new(
            field,
            "must not contain user credentials",
        ));
    }
    Ok(())
}

fn validate_media_type(field: &'static str, value: &str) -> Result<(), ValidationError> {
    validate_required_text(field, value)?;
    value
        .parse::<mime::Mime>()
        .map_err(|error| ValidationError::new(field, format!("is not a media type: {error}")))?;
    Ok(())
}

fn validate_payload_path(value: &str) -> Result<(), ValidationError> {
    validate_required_text("payload_path", value)?;
    if Path::new(value).is_absolute() || value.starts_with('/') {
        return Err(ValidationError::new(
            "payload_path",
            "must be a relative path",
        ));
    }
    if value.contains('\\') || value.contains(':') {
        return Err(ValidationError::new(
            "payload_path",
            "must use portable relative path syntax",
        ));
    }
    if value
        .split('/')
        .any(|component| component.is_empty() || matches!(component, "." | ".."))
    {
        return Err(ValidationError::new(
            "payload_path",
            "must not contain empty, current-directory, or parent-directory components",
        ));
    }
    Ok(())
}

fn validate_sha256(field: &'static str, value: &str) -> Result<(), ValidationError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(ValidationError::new(
            field,
            "must be exactly 64 lowercase hexadecimal characters",
        ));
    }
    Ok(())
}

fn validate_html(value: &str) -> Result<(), ValidationError> {
    if value.trim().is_empty() {
        return Err(ValidationError::new("html", "must be nonempty"));
    }
    if value.chars().any(|character| {
        character == '\0' || (character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    }) {
        return Err(ValidationError::new(
            "html",
            "must not contain unsafe control characters",
        ));
    }
    Ok(())
}

fn destination_parent(path: &Path) -> io::Result<PathBuf> {
    if path.file_name().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("inventory destination has no file name: {}", path.display()),
        ));
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let metadata = fs::metadata(parent)?;
    if !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "inventory destination parent is not a directory: {}",
                parent.display()
            ),
        ));
    }
    Ok(parent.to_path_buf())
}

fn limit_error(path: &Path, max_bytes: u64) -> io::Error {
    invalid_data(format!(
        "inventory {} exceeds the {max_bytes}-byte read limit",
        path.display()
    ))
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use legal_model::SourceId;
    use std::fs;
    use tempfile::tempdir;

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    fn source(value: &str) -> TestResult<SourceId> {
        Ok(SourceId::new(value)?)
    }

    fn document(source_id: &str, native_id: &str) -> TestResult<DocumentId> {
        Ok(DocumentId::new(source(source_id)?, native_id)?)
    }

    fn inventory_for(source_id: &str, native_id: &str) -> TestResult<SourceInventoryRecord> {
        Ok(SourceInventoryRecord::new(
            document(source_id, native_id)?,
            Some("version-1".to_string()),
            format!("https://example.gov.au/document/{native_id}"),
            "ruling",
            format!("Document {native_id}"),
            Some("2026-07-12".to_string()),
            format!("payloads/{native_id}.html"),
            sha256_bytes(b"raw payload"),
            11,
            "text/html; charset=utf-8",
        )?)
    }

    fn normalized_asset(
        source_id: &str,
        asset_id: &str,
        data: &[u8],
    ) -> TestResult<NormalizedAsset> {
        Ok(NormalizedAsset::new(
            AssetRef::new(source(source_id)?, asset_id)?,
            "image/png",
            Some(format!("Alt {asset_id}")),
            None,
            sha256_bytes(data),
            data.to_vec(),
        )?)
    }

    #[test]
    fn sha256_bytes_uses_canonical_lowercase_hex() -> TestResult {
        assert_eq!(
            sha256_bytes(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        Ok(())
    }

    #[test]
    fn inventory_constructor_and_serde_round_trip() -> TestResult {
        let record = inventory_for("ato", "JUD/example")?;
        let json = serde_json::to_string(&record)?;
        let decoded: SourceInventoryRecord = serde_json::from_str(&json)?;
        assert_eq!(decoded, record);
        assert!(json.contains("\"document\""));
        assert!(json.contains("\"payload_sha256\""));
        Ok(())
    }

    #[test]
    fn inventory_rejects_empty_and_unsafe_metadata() -> TestResult {
        let mut record = inventory_for("ato", "JUD/example")?;
        for invalid in ["", "   ", " leading", "trailing ", "line\nbreak"] {
            record.title = invalid.to_string();
            assert!(record.validate().is_err(), "accepted title {invalid:?}");
        }
        record = inventory_for("ato", "JUD/example")?;
        record.document_type.clear();
        assert_eq!(record.validate().unwrap_err().field(), "document_type");
        record = inventory_for("ato", "JUD/example")?;
        record.upstream_version = Some(String::new());
        assert_eq!(record.validate().unwrap_err().field(), "upstream_version");
        record = inventory_for("ato", "JUD/example")?;
        record.date = Some("bad\0date".to_string());
        assert_eq!(record.validate().unwrap_err().field(), "date");
        record = inventory_for("ato", "JUD/example")?;
        record.media_type = "not a media type".to_string();
        assert_eq!(record.validate().unwrap_err().field(), "media_type");
        record = inventory_for("ato", "JUD/example")?;
        record.payload_size = 0;
        assert_eq!(record.validate().unwrap_err().field(), "payload_size");
        record = inventory_for("ato", "JUD/example")?;
        record.document.native_id = "  ".to_string();
        assert_eq!(record.validate().unwrap_err().field(), "document.native_id");
        Ok(())
    }

    #[test]
    fn inventory_rejects_non_https_or_unsafe_canonical_urls() -> TestResult {
        let mut record = inventory_for("ato", "JUD/example")?;
        for invalid in [
            "http://example.gov.au/document",
            "ftp://example.gov.au/document",
            "https:///missing-host",
            "https://user:secret@example.gov.au/document",
            "https://example.gov.au/white space",
        ] {
            record.canonical_url = invalid.to_string();
            assert!(
                record.validate().is_err(),
                "accepted canonical URL {invalid:?}"
            );
        }
        Ok(())
    }

    #[test]
    fn inventory_rejects_absolute_traversing_and_nonportable_payload_paths() -> TestResult {
        let mut record = inventory_for("ato", "JUD/example")?;
        for invalid in [
            "",
            "/absolute/file.html",
            "../secret.html",
            "payloads/../secret.html",
            "payloads/./file.html",
            "payloads//file.html",
            "payloads/file.html/",
            r"C:\payloads\file.html",
            r"\\server\share\file.html",
            "payloads:stream/file.html",
        ] {
            record.payload_path = invalid.to_string();
            assert!(
                record.validate().is_err(),
                "accepted payload path {invalid:?}"
            );
        }
        record.payload_path = "payloads/nested/file.html".to_string();
        record.validate()?;
        Ok(())
    }

    #[test]
    fn inventory_requires_exact_lowercase_sha256() -> TestResult {
        let mut record = inventory_for("ato", "JUD/example")?;
        for invalid in [
            "",
            "abc",
            "BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD",
            "ga7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad0",
        ] {
            record.payload_sha256 = invalid.to_string();
            assert!(record.validate().is_err(), "accepted SHA-256 {invalid:?}");
        }
        Ok(())
    }

    #[test]
    fn serde_deserialization_validates_and_rejects_unknown_fields() -> TestResult {
        let record = inventory_for("ato", "JUD/example")?;
        let mut value = serde_json::to_value(&record)?;
        value["canonical_url"] = serde_json::json!("http://example.gov.au");
        assert!(serde_json::from_value::<SourceInventoryRecord>(value).is_err());

        let mut value = serde_json::to_value(&record)?;
        value["unexpected"] = serde_json::json!(true);
        assert!(serde_json::from_value::<SourceInventoryRecord>(value).is_err());
        Ok(())
    }

    #[test]
    fn normalized_asset_validates_bytes_and_metadata() -> TestResult {
        let bytes = b"png bytes";
        let mut asset = normalized_asset("ato", "JUD/example/0", bytes)?;
        asset.validate()?;

        asset.sha256 = sha256_bytes(b"different");
        assert_eq!(asset.validate().unwrap_err().field(), "sha256");
        asset.sha256 = sha256_bytes(bytes);
        asset.data.clear();
        assert_eq!(asset.validate().unwrap_err().field(), "data");
        asset.data = bytes.to_vec();
        asset.alt = Some("  ".to_string());
        assert_eq!(asset.validate().unwrap_err().field(), "alt");
        asset.alt = None;
        asset.media_type = "invalid".to_string();
        assert_eq!(asset.validate().unwrap_err().field(), "media_type");
        Ok(())
    }

    #[test]
    fn normalized_document_rejects_empty_or_unsafe_html() -> TestResult {
        let inventory = inventory_for("ato", "JUD/example")?;
        for invalid in ["", "  \n\t", "<p>bad\0html</p>", "<p>bad\u{7f}html</p>"] {
            assert!(NormalizedDocument::new(inventory.clone(), invalid, vec![]).is_err());
        }
        NormalizedDocument::new(inventory, "<article>valid</article>\n", vec![])?;
        Ok(())
    }

    #[test]
    fn normalized_document_rejects_asset_source_mismatches_and_duplicates() -> TestResult {
        let inventory = inventory_for("ato", "JUD/example")?;
        let foreign = normalized_asset("high-court", "JUD/example/0", b"image")?;
        let error = NormalizedDocument::new(
            inventory.clone(),
            "<article>document</article>",
            vec![foreign],
        )
        .unwrap_err();
        assert_eq!(error.field(), "assets");

        let asset = normalized_asset("ato", "JUD/example/0", b"image")?;
        let error = NormalizedDocument::new(
            inventory,
            "<article>document</article>",
            vec![asset.clone(), asset],
        )
        .unwrap_err();
        assert_eq!(error.field(), "assets");
        assert!(error.message().contains("duplicate"));
        Ok(())
    }

    #[test]
    fn normalized_hash_is_stable_across_asset_order_and_sensitive_to_content() -> TestResult {
        let inventory = inventory_for("ato", "JUD/example")?;
        let first = normalized_asset("ato", "JUD/example/1", b"first")?;
        let second = normalized_asset("ato", "JUD/example/2", b"second")?;
        let document = NormalizedDocument::new(
            inventory.clone(),
            "<article>document</article>",
            vec![second.clone(), first.clone()],
        )?;
        let reordered = NormalizedDocument::new(
            inventory,
            "<article>document</article>",
            vec![first, second],
        )?;
        assert_eq!(
            document.normalized_sha256()?,
            reordered.normalized_sha256()?
        );
        assert_eq!(
            document.normalized_sha256()?,
            "eac56bc56d592f1be630a960d052b05e1788f22374d1e45275311e7af4c8fe60"
        );

        let changed = NormalizedDocument::new(
            reordered.inventory.clone(),
            "<article>changed</article>",
            reordered.assets.clone(),
        )?;
        assert_ne!(reordered.normalized_sha256()?, changed.normalized_sha256()?);
        Ok(())
    }

    #[test]
    fn normalized_document_deserialization_enforces_cross_record_invariants() -> TestResult {
        let document = NormalizedDocument::new(
            inventory_for("ato", "JUD/example")?,
            "<article>document</article>",
            vec![normalized_asset("ato", "JUD/example/0", b"image")?],
        )?;
        let mut value = serde_json::to_value(document)?;
        value["assets"][0]["asset"]["source"] = serde_json::json!("high-court");
        assert!(serde_json::from_value::<NormalizedDocument>(value).is_err());
        Ok(())
    }

    #[test]
    fn jsonl_write_is_sorted_atomic_and_round_trips() -> TestResult {
        let directory = tempdir()?;
        let path = directory.path().join("inventory.jsonl");
        let zulu = inventory_for("ato", "zulu")?;
        let alpha = inventory_for("ato", "alpha")?;

        write_inventory_jsonl_atomic(&path, &[zulu.clone(), alpha.clone()])?;
        let text = fs::read_to_string(&path)?;
        let lines = text.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("alpha"));
        assert!(lines[1].contains("zulu"));
        assert!(text.ends_with('\n'));
        assert_eq!(
            read_inventory_jsonl(&path, text.len() as u64)?,
            vec![alpha, zulu]
        );

        let files = fs::read_dir(directory.path())?
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<io::Result<Vec<_>>>()?;
        assert_eq!(files, vec![std::ffi::OsString::from("inventory.jsonl")]);
        Ok(())
    }

    #[test]
    fn jsonl_read_is_byte_bounded_and_requires_utf8() -> TestResult {
        let directory = tempdir()?;
        let path = directory.path().join("inventory.jsonl");
        write_inventory_jsonl_atomic(&path, &[inventory_for("ato", "alpha")?])?;
        let size = fs::metadata(&path)?.len();
        assert_eq!(read_inventory_jsonl(&path, size)?.len(), 1);
        let error = read_inventory_jsonl(&path, size - 1).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);

        fs::write(&path, b"{\"title\":\"\xff\"}\n")?;
        let error = read_inventory_jsonl(&path, 1024).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("not UTF-8"));
        Ok(())
    }

    #[test]
    fn jsonl_read_accepts_crlf_and_blank_lines() -> TestResult {
        let directory = tempdir()?;
        let path = directory.path().join("inventory.jsonl");
        let json = serde_json::to_string(&inventory_for("ato", "alpha")?)?;
        fs::write(&path, format!("\r\n{json}\r\n  \r\n"))?;
        assert_eq!(read_inventory_jsonl(&path, 4096)?.len(), 1);
        Ok(())
    }

    #[test]
    fn jsonl_helpers_reject_duplicates_without_replacing_destination() -> TestResult {
        let directory = tempdir()?;
        let path = directory.path().join("inventory.jsonl");
        fs::write(&path, "original\n")?;
        let duplicate = inventory_for("ato", "alpha")?;
        let error = write_inventory_jsonl_atomic(&path, &[duplicate.clone(), duplicate.clone()])
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(fs::read_to_string(&path)?, "original\n");

        let json = serde_json::to_string(&duplicate)?;
        fs::write(&path, format!("{json}\n{json}\n"))?;
        let error = read_inventory_jsonl(&path, 16 * 1024).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("duplicate"));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn atomic_jsonl_write_preserves_existing_permissions() -> TestResult {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempdir()?;
        let path = directory.path().join("inventory.jsonl");
        fs::write(&path, "old\n")?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640))?;
        write_inventory_jsonl_atomic(&path, &[inventory_for("ato", "alpha")?])?;
        assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o777, 0o640);
        Ok(())
    }
}
