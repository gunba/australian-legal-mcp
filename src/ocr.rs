//! Tesseract OCR shell-out for scanned-PDF text extraction.
//!
//! Triggered from the AustLII fetch path when `pdf-extract` returns less
//! than a sensible amount of text — i.e. the PDF has no embedded text
//! layer. OCR results are cached in `<data_dir>/ocr_cache/<sha256>.txt`
//! so repeat fetches of the same scanned judgment don't pay the OCR cost
//! again.
//!
//! `tesseract` is not bundled — it must be on the user's `$PATH`. The
//! caller checks `is_tesseract_available()` before requesting OCR and
//! returns a useful error if it isn't.

use crate::config::data_dir;
use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

const OCR_LANGUAGE: &str = "eng";
const OCR_OEM: &str = "1";
const OCR_PSM: &str = "3";

/// True when `tesseract --version` runs and exits 0. Cheap probe used by
/// the fetcher before kicking off any temp-file work.
pub(crate) fn is_tesseract_available() -> bool {
    Command::new("tesseract")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// OCR a PDF and return the recognised text. Cache lookup is sha256-keyed
/// on the PDF bytes; misses run tesseract and write the result back.
pub(crate) fn ocr_pdf(pdf_bytes: &[u8]) -> Result<String> {
    let cache_path = ocr_cache_path(pdf_bytes)?;
    if let Ok(cached) = std::fs::read_to_string(&cache_path) {
        if !cached.trim().is_empty() {
            return Ok(cached);
        }
    }
    if !is_tesseract_available() {
        bail!(
            "tesseract not found on PATH. Install Tesseract \
             (https://github.com/tesseract-ocr/tesseract) or retry without allow_ocr."
        );
    }
    let tmp = tempfile::Builder::new()
        .prefix("ato-mcp-ocr-")
        .suffix(".pdf")
        .tempfile()
        .context("creating temp file for OCR input")?;
    {
        let mut file = tmp
            .as_file()
            .try_clone()
            .context("cloning temp file handle")?;
        file.write_all(pdf_bytes)
            .context("writing PDF bytes to temp file")?;
        file.sync_all().context("flushing temp file")?;
    }
    let input_path = tmp
        .path()
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("temp file path is not valid UTF-8"))?;
    let output = run_tesseract(input_path)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("tesseract failed: {stderr}");
    }
    let text = String::from_utf8_lossy(&output.stdout).to_string();
    if let Some(parent) = cache_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating OCR cache dir {}", parent.display()))?;
    }
    std::fs::write(&cache_path, &text)
        .with_context(|| format!("writing OCR cache {}", cache_path.display()))?;
    Ok(text)
}

fn run_tesseract(input_path: &str) -> Result<std::process::Output> {
    // tesseract <input> stdout -l eng --oem 1 --psm 3
    // tesseract has no `--timeout` flag and `std::process` has no
    // per-process timeout. The MCP client's request timeout
    // (recommended 120s for OCR-allowed calls; see README) is what
    // bounds us in practice.
    Command::new("tesseract")
        .args([input_path, "stdout", "-l", OCR_LANGUAGE, "--oem", OCR_OEM, "--psm", OCR_PSM])
        .output()
        .context("running tesseract")
}

fn ocr_cache_path(pdf_bytes: &[u8]) -> Result<PathBuf> {
    let mut hasher = Sha256::new();
    hasher.update(pdf_bytes);
    let hash = hex::encode(hasher.finalize());
    Ok(data_dir()?.join("ocr_cache").join(format!("{hash}.txt")))
}

mod hex {
    pub(super) fn encode(bytes: impl AsRef<[u8]>) -> String {
        let mut out = String::with_capacity(bytes.as_ref().len() * 2);
        for b in bytes.as_ref() {
            out.push_str(&format!("{b:02x}"));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_encode_is_lowercase() {
        assert_eq!(hex::encode([0x0a, 0xff]), "0aff");
        assert_eq!(hex::encode([0, 1, 2, 3]), "00010203");
    }

    #[test]
    fn ocr_cache_path_uses_sha256() {
        let bytes = b"hello world";
        // sha256("hello world") = b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9
        let path = ocr_cache_path(bytes).unwrap();
        assert!(
            path.to_string_lossy().ends_with(
                "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9.txt"
            ),
            "path = {}",
            path.display()
        );
    }
}
