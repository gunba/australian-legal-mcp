//! Allocation-light implementation of the exact BERT WordPiece tokenizer used
//! by the pinned embedding model.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use tokenizers::Tokenizer;
use unicode_categories::UnicodeCategories;
use unicode_normalization::UnicodeNormalization;

const CLS_ID: i64 = 101;
const SEP_ID: i64 = 102;
const UNK_ID: i64 = 100;
const MAX_WORD_CHARS: usize = 100;
const SPECIAL_TOKENS: [(&str, i64); 5] = [
    ("[PAD]", 0),
    ("[UNK]", UNK_ID),
    ("[CLS]", CLS_ID),
    ("[SEP]", SEP_ID),
    ("[MASK]", 103),
];

/// The pinned tokenizer is the standard uncased BERT normalizer,
/// pre-tokenizer, and greedy WordPiece model. Hugging Face's general-purpose
/// offset-tracking pipeline allocates heavily even though this application
/// needs only token IDs. This specialized implementation deliberately accepts
/// no other tokenizer shape.
pub(crate) struct BertWordPieceTokenizer {
    initial_vocab: HashMap<String, i64>,
    continuation_vocab: HashMap<String, i64>,
    // Preserve exact added-token behavior for the rare input that contains a
    // literal BERT special token. Ordinary legal text never takes this path.
    reference: Tokenizer,
}

impl BertWordPieceTokenizer {
    pub(crate) fn from_file(path: &Path) -> Result<Self> {
        let bytes =
            std::fs::read(path).with_context(|| format!("reading tokenizer {}", path.display()))?;
        let root: Value = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing tokenizer {}", path.display()))?;
        validate_configuration(&root)?;
        let vocab = root
            .pointer("/model/vocab")
            .and_then(Value::as_object)
            .ok_or_else(|| anyhow!("tokenizer model.vocab must be an object"))?;
        let mut initial_vocab = HashMap::with_capacity(vocab.len());
        let mut continuation_vocab = HashMap::with_capacity(vocab.len());
        for (token, id) in vocab {
            let id = id
                .as_u64()
                .and_then(|id| i64::try_from(id).ok())
                .ok_or_else(|| anyhow!("tokenizer vocabulary ID for {token:?} is invalid"))?;
            initial_vocab.insert(token.clone(), id);
            if let Some(suffix) = token.strip_prefix("##") {
                continuation_vocab.insert(suffix.to_string(), id);
            }
        }
        let mut reference = Tokenizer::from_bytes(bytes)
            .map_err(|error| anyhow!("loading reference tokenizer: {error}"))?;
        reference
            .with_truncation(None)
            .map_err(|error| anyhow!("disabling reference tokenizer truncation: {error}"))?;
        reference.with_padding(None);
        Ok(Self {
            initial_vocab,
            continuation_vocab,
            reference,
        })
    }

    pub(crate) fn encode(&self, input: &str) -> Result<Vec<i64>> {
        if SPECIAL_TOKENS
            .iter()
            .any(|(token, _)| input.contains(token))
        {
            return self
                .reference
                .encode(input, true)
                .map(|encoding| encoding.get_ids().iter().map(|id| i64::from(*id)).collect())
                .map_err(|error| anyhow!("tokenizing special-token input: {error}"));
        }

        let mut ids = Vec::with_capacity(input.len() / 4 + 2);
        ids.push(CLS_ID);
        let mut word = String::new();
        for character in input.chars() {
            if character.is_ascii() {
                if character.is_ascii_control() {
                    if matches!(character, '\t' | '\n' | '\r') {
                        self.flush_word(&mut word, &mut ids);
                    }
                    continue;
                }
                if character == ' ' {
                    self.flush_word(&mut word, &mut ids);
                } else if character.is_ascii_punctuation() {
                    self.flush_word(&mut word, &mut ids);
                    word.push(character);
                    self.flush_word(&mut word, &mut ids);
                } else {
                    word.push(character.to_ascii_lowercase());
                }
                continue;
            }
            if is_removed_control(character) {
                continue;
            }
            if is_bert_whitespace(character) {
                self.flush_word(&mut word, &mut ids);
                continue;
            }
            if is_chinese_character(character) {
                self.flush_word(&mut word, &mut ids);
                self.push_normalized_character(character, &mut word, &mut ids);
                self.flush_word(&mut word, &mut ids);
                continue;
            }
            self.push_normalized_character(character, &mut word, &mut ids);
        }
        self.flush_word(&mut word, &mut ids);
        ids.push(SEP_ID);
        Ok(ids)
    }

    fn push_normalized_character(&self, character: char, word: &mut String, ids: &mut Vec<i64>) {
        for decomposed in std::iter::once(character).nfd() {
            if decomposed.is_mark_nonspacing() {
                continue;
            }
            for lowercase in decomposed.to_lowercase() {
                if lowercase.is_ascii() {
                    if lowercase.is_ascii_control() {
                        if matches!(lowercase, '\t' | '\n' | '\r') {
                            self.flush_word(word, ids);
                        }
                    } else if lowercase == ' ' {
                        self.flush_word(word, ids);
                    } else if lowercase.is_ascii_punctuation() {
                        self.flush_word(word, ids);
                        word.push(lowercase);
                        self.flush_word(word, ids);
                    } else {
                        word.push(lowercase);
                    }
                } else if is_bert_punctuation(lowercase) {
                    self.flush_word(word, ids);
                    word.push(lowercase);
                    self.flush_word(word, ids);
                } else if lowercase.is_whitespace() {
                    self.flush_word(word, ids);
                } else {
                    word.push(lowercase);
                }
            }
        }
    }

    fn flush_word(&self, word: &mut String, ids: &mut Vec<i64>) {
        if word.is_empty() {
            return;
        }
        self.tokenize_word(word, ids);
        word.clear();
    }

    fn tokenize_word(&self, word: &str, ids: &mut Vec<i64>) {
        let mut boundaries = Vec::with_capacity(word.len().min(MAX_WORD_CHARS) + 1);
        boundaries.extend(word.char_indices().map(|(offset, _)| offset));
        if boundaries.len() > MAX_WORD_CHARS {
            ids.push(UNK_ID);
            return;
        }
        boundaries.push(word.len());

        let output_start = ids.len();
        let mut start_index = 0usize;
        while start_index + 1 < boundaries.len() {
            let start = boundaries[start_index];
            let vocab = if start == 0 {
                &self.initial_vocab
            } else {
                &self.continuation_vocab
            };
            let mut end_index = boundaries.len() - 1;
            let mut matched = None;
            while end_index > start_index {
                let candidate = &word[start..boundaries[end_index]];
                if let Some(&id) = vocab.get(candidate) {
                    matched = Some((end_index, id));
                    break;
                }
                end_index -= 1;
            }
            let Some((next_index, id)) = matched else {
                ids.truncate(output_start);
                ids.push(UNK_ID);
                return;
            };
            ids.push(id);
            start_index = next_index;
        }
    }
}

fn validate_configuration(root: &Value) -> Result<()> {
    let expected = [
        ("/normalizer/type", "BertNormalizer"),
        ("/pre_tokenizer/type", "BertPreTokenizer"),
        ("/model/type", "WordPiece"),
        ("/model/unk_token", "[UNK]"),
        ("/model/continuing_subword_prefix", "##"),
        ("/post_processor/type", "TemplateProcessing"),
    ];
    for (pointer, value) in expected {
        if root.pointer(pointer).and_then(Value::as_str) != Some(value) {
            bail!("unsupported pinned tokenizer configuration at {pointer}");
        }
    }
    let bools = [
        ("/normalizer/clean_text", true),
        ("/normalizer/handle_chinese_chars", true),
        ("/normalizer/lowercase", true),
    ];
    for (pointer, value) in bools {
        if root.pointer(pointer).and_then(Value::as_bool) != Some(value) {
            bail!("unsupported pinned tokenizer configuration at {pointer}");
        }
    }
    if !root
        .pointer("/normalizer/strip_accents")
        .is_some_and(Value::is_null)
    {
        bail!("unsupported pinned tokenizer strip_accents configuration");
    }
    if root
        .pointer("/model/max_input_chars_per_word")
        .and_then(Value::as_u64)
        != Some(MAX_WORD_CHARS as u64)
    {
        bail!("unsupported pinned tokenizer maximum WordPiece length");
    }
    let vocab = root
        .pointer("/model/vocab")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("tokenizer model.vocab must be an object"))?;
    if vocab.len() != 30_522 {
        bail!("pinned tokenizer has an unexpected vocabulary size");
    }
    for (token, expected_id) in SPECIAL_TOKENS {
        if vocab.get(token).and_then(Value::as_i64) != Some(expected_id) {
            bail!("pinned tokenizer has unexpected ID for {token}");
        }
    }
    for (token, expected_id) in [("[CLS]", CLS_ID), ("[SEP]", SEP_ID)] {
        let pointer = format!("/post_processor/special_tokens/{token}/ids/0");
        if root.pointer(&pointer).and_then(Value::as_i64) != Some(expected_id) {
            bail!("pinned tokenizer has unexpected post-processor ID for {token}");
        }
    }
    Ok(())
}

fn is_bert_whitespace(character: char) -> bool {
    matches!(character, '\t' | '\n' | '\r') || character.is_whitespace()
}

fn is_removed_control(character: char) -> bool {
    character == '\0'
        || character == '\u{fffd}'
        || (!matches!(character, '\t' | '\n' | '\r') && character.is_other())
}

fn is_bert_punctuation(character: char) -> bool {
    character.is_ascii_punctuation() || character.is_punctuation()
}

fn is_chinese_character(character: char) -> bool {
    matches!(
        character as u32,
        0x4E00..=0x9FFF
            | 0x3400..=0x4DBF
            | 0x20000..=0x2A6DF
            | 0x2A700..=0x2B73F
            | 0x2B740..=0x2B81F
            | 0x2B920..=0x2CEAF
            | 0xF900..=0xFAFF
            | 0x2F800..=0x2FA1F
    )
}

#[cfg(test)]
mod tests {
    use super::BertWordPieceTokenizer;
    use anyhow::{Context, Result};
    use std::path::PathBuf;

    #[test]
    #[ignore = "requires LEGAL_MCP_TEST_MODEL_DIR"]
    fn special_token_inputs_are_neither_truncated_nor_padded() -> Result<()> {
        let model = PathBuf::from(
            std::env::var("LEGAL_MCP_TEST_MODEL_DIR")
                .context("LEGAL_MCP_TEST_MODEL_DIR is required")?,
        );
        let tokenizer = BertWordPieceTokenizer::from_file(&model.join("tokenizer.json"))?;

        let long = tokenizer.encode(&format!("{} [MASK]", "!".repeat(600)))?;
        assert!(long.len() > 512, "special-token input was truncated");
        let short = tokenizer.encode("short [MASK]")?;
        assert!(short.len() < 128, "special-token input was padded");
        assert!(!short.contains(&0), "special-token input contains PAD IDs");
        Ok(())
    }
}
