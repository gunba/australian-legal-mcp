//! Granite ONNX embedding runtime: model files, tokenizer, session, batched
//! encode, and int8 quantisation.

use crate::config::{model_data_path, model_path, tokenizer_path};
use crate::{EMBEDDING_DIM, EMBEDDING_INPUT_MAX_TOKENS, EMBEDDING_TEXT_PREFIX};
use anyhow::{anyhow, bail, Context, Result};
#[cfg(feature = "cuda")]
use ort::ep;
use ort::session::{
    builder::{GraphOptimizationLevel, SessionBuilder},
    Session,
};
use ort::value::TensorRef;
#[allow(unused_imports)]
use simsimd::SpatialSimilarity as _;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokenizers::{PaddingParams, Tokenizer, TruncationParams};

extern "C" {
    fn simsimd_dot_i8(a: *const i8, b: *const i8, n: usize, out: *mut f64);
}

// Avoid expensive online transformer graph rewrites on every fresh CLI/MCP
// process. The ONNX models are shipped pre-quantized; Level1 keeps cheap
// semantics-preserving cleanup without the high startup cost of Level2/All.
pub(crate) const ONLINE_MODEL_OPTIMIZATION_LEVEL: GraphOptimizationLevel =
    GraphOptimizationLevel::Level1;

pub(crate) struct HfModelFile {
    pub(crate) path: &'static str,
    pub(crate) output_name: &'static str,
    pub(crate) sha256: &'static str,
    pub(crate) size: u64,
}

pub(crate) const EMBEDDING_MODEL_HF_FILES: &[HfModelFile] = &[
    HfModelFile {
        path: "onnx/model_fp16.onnx",
        output_name: "model_fp16.onnx",
        sha256: "ee200de55cb2f94e858aabca54be7697a9c0805a14c858ee26ad0922b05f57d7",
        size: 200_792,
    },
    HfModelFile {
        path: "onnx/model_fp16.onnx_data",
        output_name: "model_fp16.onnx_data",
        sha256: "28d16e29cd623f25cc6fa0968700c5bc31036466091a5fa06d1353c1777f050e",
        size: 97_402_880,
    },
    HfModelFile {
        path: "tokenizer.json",
        output_name: "tokenizer.json",
        sha256: "feeb83348dcb033bc6b9d2e1f7906ca9eb2d122845000c9416d894d7c2927149",
        size: 2_128_614,
    },
];

#[derive(Clone, Debug)]
pub(crate) struct SemanticModelPaths {
    pub(crate) model: PathBuf,
    pub(crate) tokenizer: PathBuf,
}

impl SemanticModelPaths {
    pub(crate) fn live() -> Result<Self> {
        let model = model_path()?;
        let model_data = model_data_path()?;
        let tokenizer = tokenizer_path()?;
        for path in [&model, &model_data, &tokenizer] {
            if !path.is_file() {
                bail!("missing installed Granite model file at {}", path.display());
            }
        }
        Ok(Self { model, tokenizer })
    }

    pub(crate) fn from_model_dir(model_dir: &Path) -> Result<Self> {
        for file in EMBEDDING_MODEL_HF_FILES {
            let path = model_dir.join(file.path);
            validate_embedding_model_file(&path, file)?;
        }
        let model = model_dir.join("onnx").join("model_fp16.onnx");
        let tokenizer = model_dir.join("tokenizer.json");
        Ok(Self { model, tokenizer })
    }
}

pub(crate) fn validate_embedding_model_file(path: &Path, file: &HfModelFile) -> Result<()> {
    if !path.is_file() {
        bail!("missing Granite model file at {}", path.display());
    }
    let size = path.metadata()?.len();
    if size != file.size {
        bail!(
            "size mismatch for Granite model file {}: got {}, expected {}",
            path.display(),
            size,
            file.size
        );
    }
    crate::verify_sha256_file(path, file.sha256)
        .with_context(|| format!("verifying Granite model file {}", path.display()))
}

pub(crate) fn dot_i8(query: &[i8; EMBEDDING_DIM], document: &[u8]) -> Result<f64> {
    if document.len() != EMBEDDING_DIM {
        bail!(
            "invalid stored embedding length: got {}, expected {}",
            document.len(),
            EMBEDDING_DIM
        );
    }
    // Reinterpret the stored u8 BLOB as i8 by casting the pointer
    // directly. The bit pattern is identical; the BLOB just happens to be
    // loaded with rusqlite's default unsigned typing.
    let mut raw = 0.0f64;
    // Safety: both pointers reference EMBEDDING_DIM-sized slices we just
    // bounds-checked; simsimd_dot_i8 reads exactly `n` bytes from each.
    unsafe {
        simsimd_dot_i8(
            query.as_ptr(),
            document.as_ptr() as *const i8,
            EMBEDDING_DIM,
            &mut raw,
        );
    }
    Ok(raw / (127.0 * 127.0))
}

#[cfg(test)]
pub(crate) fn dot_i8_scalar_reference(query: &[i8; EMBEDDING_DIM], document: &[u8]) -> Result<f64> {
    if document.len() != EMBEDDING_DIM {
        bail!(
            "invalid stored embedding length: got {}, expected {}",
            document.len(),
            EMBEDDING_DIM
        );
    }
    let mut dot = 0i32;
    for (q, d) in query.iter().zip(document.iter()) {
        dot += i32::from(*q) * i32::from(*d as i8);
    }
    Ok(dot as f64 / (127.0 * 127.0))
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SemanticEncodeStats {
    pub(crate) tokenize: Duration,
    pub(crate) prepare: Duration,
    pub(crate) run: Duration,
    pub(crate) postprocess: Duration,
    pub(crate) batches: usize,
    pub(crate) inputs: usize,
    pub(crate) active_tokens: usize,
    pub(crate) padded_tokens: usize,
    pub(crate) max_batch: usize,
    pub(crate) max_seq_len: usize,
}

impl SemanticEncodeStats {
    pub(crate) fn record_batch(&mut self, batch: usize, seq_len: usize, active_tokens: usize) {
        self.batches += 1;
        self.inputs += batch;
        self.active_tokens += active_tokens;
        self.padded_tokens += batch * seq_len;
        self.max_batch = self.max_batch.max(batch);
        self.max_seq_len = self.max_seq_len.max(seq_len);
    }

    pub(crate) fn merge(&mut self, other: Self) {
        self.tokenize += other.tokenize;
        self.prepare += other.prepare;
        self.run += other.run;
        self.postprocess += other.postprocess;
        self.batches += other.batches;
        self.inputs += other.inputs;
        self.active_tokens += other.active_tokens;
        self.padded_tokens += other.padded_tokens;
        self.max_batch = self.max_batch.max(other.max_batch);
        self.max_seq_len = self.max_seq_len.max(other.max_seq_len);
    }
}

pub(crate) struct SemanticRuntime {
    tokenizer: Tokenizer,
    validation_tokenizer: Tokenizer,
    session: Session,
    has_token_type_ids: bool,
}

impl SemanticRuntime {
    pub(crate) fn load(use_gpu: bool, model_paths: &SemanticModelPaths) -> Result<Self> {
        let validation_tokenizer = Tokenizer::from_file(&model_paths.tokenizer)
            .map_err(|err| anyhow!("loading tokenizer: {err}"))?;
        let mut tokenizer = validation_tokenizer.clone();
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: EMBEDDING_INPUT_MAX_TOKENS,
                ..TruncationParams::default()
            }))
            .map_err(|err| anyhow!("configuring tokenizer truncation: {err}"))?;
        tokenizer.with_padding(Some(PaddingParams::default()));

        let optimization_level = if use_gpu {
            GraphOptimizationLevel::All
        } else {
            ONLINE_MODEL_OPTIMIZATION_LEVEL
        };
        let mut builder = Session::builder()
            .map_err(|err| anyhow!("creating ONNX Runtime session: {err}"))?
            .with_optimization_level(optimization_level)
            .map_err(|err| anyhow!("configuring ONNX Runtime session: {err}"))?;
        if use_gpu {
            // [EM-01] CPU is the default runtime; maintainer GPU builds
            // require the cuda feature and fail if CUDA EP registration fails.
            builder = configure_cuda_execution_provider(builder)?;
        }
        let session = builder
            .commit_from_file(&model_paths.model)
            .map_err(|err| anyhow!("loading ONNX model: {err}"))?;
        let has_token_type_ids = session
            .inputs()
            .iter()
            .any(|input| input.name() == "token_type_ids");

        Ok(Self {
            tokenizer,
            validation_tokenizer,
            session,
            has_token_type_ids,
        })
    }

    pub(crate) fn encode_query(&mut self, query: &str) -> Result<[i8; EMBEDDING_DIM]> {
        let mut embeddings = self.encode_queries(&[query.to_string()])?;
        embeddings
            .pop()
            .ok_or_else(|| anyhow!("semantic encoder returned no query embedding"))
    }

    pub(crate) fn encode_queries(
        &mut self,
        queries: &[String],
    ) -> Result<Vec<[i8; EMBEDDING_DIM]>> {
        let (embeddings, _stats) = self.encode_queries_with_stats(queries)?;
        Ok(embeddings)
    }

    fn validate_input_token_counts(&self, prefixed: &[String]) -> Result<()> {
        let encodings = self
            .validation_tokenizer
            .encode_batch(prefixed.to_vec(), true)
            .map_err(|err| anyhow!("validating semantic input token counts: {err}"))?;
        let counts = encodings
            .iter()
            .map(|encoding| encoding.get_ids().len())
            .collect::<Vec<_>>();
        ensure_token_counts_within_limit(&counts, EMBEDDING_INPUT_MAX_TOKENS)
    }

    pub(crate) fn encode_queries_with_stats(
        &mut self,
        queries: &[String],
    ) -> Result<(Vec<[i8; EMBEDDING_DIM]>, SemanticEncodeStats)> {
        if queries.is_empty() {
            return Ok((Vec::new(), SemanticEncodeStats::default()));
        }
        let prefixed: Vec<String> = queries
            .iter()
            .map(|query| format!("{EMBEDDING_TEXT_PREFIX}{query}"))
            .collect();
        self.validate_input_token_counts(&prefixed)?;
        let mut stats = SemanticEncodeStats::default();
        let started = std::time::Instant::now();
        let encodings = self
            .tokenizer
            .encode_batch(prefixed, true)
            .map_err(|err| anyhow!("tokenizing queries: {err}"))?;
        stats.tokenize += started.elapsed();
        let batch = encodings.len();
        if batch != queries.len() {
            bail!(
                "tokenizer returned {} encodings for {} inputs",
                batch,
                queries.len()
            );
        }
        let seq_len = encodings
            .first()
            .map(|encoding| encoding.get_ids().len())
            .unwrap_or(0);
        if seq_len == 0 {
            bail!("semantic search unavailable: query produced no tokens");
        }
        let started = std::time::Instant::now();
        let mut input_ids = Vec::with_capacity(batch * seq_len);
        let mut attention_mask = Vec::with_capacity(batch * seq_len);
        let mut active_tokens = 0usize;
        for encoding in &encodings {
            if encoding.get_ids().len() != seq_len {
                bail!(
                    "tokenizer produced ragged encodings: expected {seq_len}, got {}",
                    encoding.get_ids().len()
                );
            }
            input_ids.extend(encoding.get_ids().iter().map(|id| i64::from(*id)));
            for mask in encoding.get_attention_mask() {
                active_tokens += usize::try_from(*mask).unwrap_or(0);
                attention_mask.push(i64::from(*mask));
            }
        }
        stats.record_batch(batch, seq_len, active_tokens);

        let input_ids_tensor =
            TensorRef::from_array_view(([batch, seq_len], input_ids.as_slice()))?;
        let attention_mask_tensor =
            TensorRef::from_array_view(([batch, seq_len], attention_mask.as_slice()))?;
        stats.prepare += started.elapsed();
        let started = std::time::Instant::now();
        let outputs = if self.has_token_type_ids {
            let token_type_ids = vec![0i64; batch * seq_len];
            let token_type_ids_tensor =
                TensorRef::from_array_view(([batch, seq_len], token_type_ids.as_slice()))?;
            self.session.run(ort::inputs! {
                "input_ids" => input_ids_tensor,
                "attention_mask" => attention_mask_tensor,
                "token_type_ids" => token_type_ids_tensor,
            })?
        } else {
            self.session.run(ort::inputs! {
                "input_ids" => input_ids_tensor,
                "attention_mask" => attention_mask_tensor,
            })?
        };
        stats.run += started.elapsed();
        let started = std::time::Instant::now();
        let output = outputs
            .get("sentence_embedding")
            .unwrap_or_else(|| &outputs[0]);
        // [EM-04] Prefer sentence_embedding when present; otherwise
        // pooled_embeddings mean-pools 3D token outputs with the attention mask.
        let (shape, data) = output.try_extract_tensor::<f32>()?;
        let embeddings = pooled_embeddings(shape, data, &attention_mask, batch, seq_len)?;
        let embeddings = embeddings
            .iter()
            .map(|embedding| quantize_embedding(embedding))
            .collect::<Result<Vec<_>>>()?;
        stats.postprocess += started.elapsed();
        Ok((embeddings, stats))
    }
}

fn ensure_token_counts_within_limit(counts: &[usize], max_tokens: usize) -> Result<()> {
    if let Some((index, actual)) = counts
        .iter()
        .copied()
        .enumerate()
        .find(|(_, count)| *count > max_tokens)
    {
        bail!(
            "semantic input {index} contains {actual} tokens, exceeding the {max_tokens}-token model limit"
        );
    }
    Ok(())
}

#[cfg(feature = "cuda")]
pub(crate) fn configure_cuda_execution_provider(builder: SessionBuilder) -> Result<SessionBuilder> {
    let cuda = ep::CUDA::default()
        .with_device_id(0)
        .with_conv_algorithm_search(ep::cuda::ConvAlgorithmSearch::Heuristic)
        .build()
        .error_on_failure();
    builder
        .with_execution_providers([cuda])
        .map_err(|err| anyhow!("registering CUDA execution provider: {err}"))
}

#[cfg(not(feature = "cuda"))]
pub(crate) fn configure_cuda_execution_provider(
    _builder: SessionBuilder,
) -> Result<SessionBuilder> {
    bail!("GPU build requested but this ato-mcp binary was built without CUDA support; rebuild with `cargo build --release --features cuda`")
}

pub(crate) fn encode_query_embedding(query: &str) -> Result<[i8; EMBEDDING_DIM]> {
    let model_paths = SemanticModelPaths::live()?;
    let mut runtime = SemanticRuntime::load(false, &model_paths)?;
    runtime.encode_query(query)
}

pub(crate) fn pooled_embeddings(
    shape: &[i64],
    data: &[f32],
    attention_mask: &[i64],
    batch: usize,
    seq_len: usize,
) -> Result<Vec<Vec<f32>>> {
    match shape {
        [out_batch, dims] => {
            let out_batch = *out_batch as usize;
            let dims = *dims as usize;
            if out_batch != batch {
                bail!("model output batch {out_batch} does not match input batch {batch}");
            }
            if data.len() < batch * dims {
                bail!("model output too short for shape {:?}", shape);
            }
            Ok((0..batch)
                .map(|idx| data[idx * dims..(idx + 1) * dims].to_vec())
                .collect())
        }
        [out_batch, out_seq_len, dims] => {
            let out_batch = *out_batch as usize;
            let out_seq_len = *out_seq_len as usize;
            let dims = *dims as usize;
            if out_batch != batch || out_seq_len != seq_len {
                bail!(
                    "model output shape {:?} does not match input batch={batch} seq_len={seq_len}",
                    shape
                );
            }
            if data.len() < batch * seq_len * dims {
                bail!("model output too short for shape {:?}", shape);
            }
            let mut out = Vec::with_capacity(batch);
            for batch_idx in 0..batch {
                let mut pooled = vec![0.0f32; dims];
                let mut denom = 0.0f32;
                for token_idx in 0..seq_len {
                    let mask = attention_mask[batch_idx * seq_len + token_idx] as f32;
                    denom += mask;
                    let offset = (batch_idx * seq_len + token_idx) * dims;
                    for dim in 0..dims {
                        pooled[dim] += data[offset + dim] * mask;
                    }
                }
                let denom = denom.max(1e-6);
                for value in &mut pooled {
                    *value /= denom;
                }
                out.push(pooled);
            }
            Ok(out)
        }
        _ => bail!("unsupported model output shape {:?}", shape),
    }
}

pub(crate) fn quantize_embedding(values: &[f32]) -> Result<[i8; EMBEDDING_DIM]> {
    if values.len() < EMBEDDING_DIM {
        bail!(
            "model output has {} dimensions, expected at least {}",
            values.len(),
            EMBEDDING_DIM
        );
    }
    let values = &values[..EMBEDDING_DIM];
    if values.iter().any(|value| !value.is_finite()) {
        bail!("model output contains non-finite embedding values");
    }
    let norm = values.iter().map(|value| value * value).sum::<f32>().sqrt();
    if !norm.is_finite() {
        bail!("model output produced a non-finite embedding norm");
    }
    if norm <= 1e-12 {
        return Ok([0; EMBEDDING_DIM]);
    }
    // [EM-06] After L2 normalisation, values are clipped, scaled by 127,
    // rounded, and stored as int8 bytes.
    let mut out = [0i8; EMBEDDING_DIM];
    for (idx, value) in values.iter().enumerate() {
        out[idx] = ((*value / norm).clamp(-1.0, 1.0) * 127.0).round() as i8;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::ensure_token_counts_within_limit;

    #[test]
    fn actual_token_count_validation_rejects_the_first_oversize_input() {
        let error = ensure_token_counts_within_limit(&[1024, 1025, 2048], 1024).unwrap_err();
        assert_eq!(
            error.to_string(),
            "semantic input 1 contains 1025 tokens, exceeding the 1024-token model limit"
        );
    }

    #[test]
    fn actual_token_count_validation_accepts_the_exact_limit() {
        ensure_token_counts_within_limit(&[0, 1, 1024], 1024).unwrap();
    }
}
