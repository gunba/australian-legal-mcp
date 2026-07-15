//! mdbr-leaf-ir ONNX embedding runtime: model files, tokenizer, session, batched
//! encode, and int8 quantisation.

use crate::bert_tokenizer::BertWordPieceTokenizer;
use crate::config::{model_path, tokenizer_path};
use crate::{
    DOCUMENT_EMBEDDING_PREFIX, EMBEDDING_DIM, EMBEDDING_INPUT_MAX_TOKENS, QUERY_EMBEDDING_PREFIX,
};
use anyhow::{anyhow, bail, Context, Result};
#[cfg(feature = "cuda")]
use ort::ep;
use ort::session::{
    builder::{GraphOptimizationLevel, SessionBuilder},
    OutputSelector, RunOptions, Session,
};
use ort::value::TensorRef;
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use std::sync::OnceLock;
use std::time::Duration;

// Keep the exported standard attention graph intact for TensorRT partitioning
// and avoid expensive online transformer rewrites on every CLI/MCP process.
// Level1 performs only cheap semantics-preserving cleanup.
pub(crate) const ONLINE_MODEL_OPTIMIZATION_LEVEL: GraphOptimizationLevel =
    GraphOptimizationLevel::Level1;
pub(crate) const EMBEDDING_BATCH_SIZE: usize = 64;

pub(crate) struct EmbeddingModelFile {
    pub(crate) path: &'static str,
    pub(crate) output_name: &'static str,
    pub(crate) sha256: &'static str,
    pub(crate) size: u64,
}

pub(crate) const EMBEDDING_MODEL_FILES: &[EmbeddingModelFile] = &[
    EmbeddingModelFile {
        path: "onnx/model.onnx",
        output_name: "model.onnx",
        sha256: "242a1d386f2f63a7daec443399b32d35b4b155b0820ee19b7c81c50436f95e11",
        size: 91_555_023,
    },
    EmbeddingModelFile {
        path: "tokenizer.json",
        output_name: "tokenizer.json",
        sha256: "da0e79933b9ed51798a3ae27893d3c5fa4a201126cef75586296df9b4d2c62a0",
        size: 711_661,
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
        let tokenizer = tokenizer_path()?;
        for path in [&model, &tokenizer] {
            if !path.is_file() {
                bail!(
                    "missing installed embedding model file at {}",
                    path.display()
                );
            }
        }
        Ok(Self { model, tokenizer })
    }

    pub(crate) fn from_model_dir(model_dir: &Path) -> Result<Self> {
        for file in EMBEDDING_MODEL_FILES {
            let path = model_dir.join(file.path);
            validate_embedding_model_file(&path, file)?;
        }
        let model = model_dir.join("onnx").join("model.onnx");
        let tokenizer = model_dir.join("tokenizer.json");
        Ok(Self { model, tokenizer })
    }
}

pub(crate) fn validate_embedding_model_file(path: &Path, file: &EmbeddingModelFile) -> Result<()> {
    if !path.is_file() {
        bail!("missing embedding model file at {}", path.display());
    }
    let size = path.metadata()?.len();
    if size != file.size {
        bail!(
            "size mismatch for embedding model file {}: got {}, expected {}",
            path.display(),
            size,
            file.size
        );
    }
    let actual = crate::build::sha256_file(path)
        .with_context(|| format!("verifying embedding model file {}", path.display()))?;
    if actual != file.sha256 {
        bail!(
            "SHA-256 mismatch for embedding model file {}",
            path.display()
        );
    }
    Ok(())
}

pub(crate) fn dot_i8(query: &[i8; EMBEDDING_DIM], document: &[u8]) -> Result<f64> {
    if document.len() != EMBEDDING_DIM {
        bail!(
            "invalid stored embedding length: got {}, expected {}",
            document.len(),
            EMBEDDING_DIM
        );
    }
    // SQLite exposes BLOB bytes as u8; conversion to i8 restores each stored
    // two's-complement component before accumulation.
    let raw = query
        .iter()
        .zip(document)
        .map(|(&query_component, &stored_component)| {
            i64::from(query_component) * i64::from(stored_component as i8)
        })
        .sum::<i64>();
    Ok(raw as f64 / (127.0 * 127.0))
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
}

#[cfg(target_os = "linux")]
fn initialize_packaged_ort() -> Result<()> {
    static INITIALIZED: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    match INITIALIZED.get_or_init(|| {
        let result = (|| -> Result<()> {
            let library = if let Some(configured) = std::env::var_os("ORT_DYLIB_PATH") {
                PathBuf::from(configured)
            } else {
                let executable = std::env::current_exe().context("locating legal-mcp executable")?;
                executable
                    .parent()
                    .ok_or_else(|| anyhow!("legal-mcp executable has no parent directory"))?
                    .join("libonnxruntime.so")
            };
            let metadata = std::fs::symlink_metadata(&library).with_context(|| {
                format!(
                    "ONNX Runtime library not found at {}; set ORT_DYLIB_PATH to a real libonnxruntime.so",
                    library.display()
                )
            })?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                bail!(
                    "ONNX Runtime library must be a real file: {}",
                    library.display()
                );
            }
            ort::init_from(&library)
                .map_err(|error| anyhow!("loading {}: {error}", library.display()))?
                .commit();
            Ok(())
        })();
        result.map_err(|error| format!("{error:#}"))
    }) {
        Ok(()) => Ok(()),
        Err(message) => bail!("{message}"),
    }
}

#[cfg(not(target_os = "linux"))]
fn initialize_packaged_ort() -> Result<()> {
    Ok(())
}

pub(crate) struct SemanticRuntime {
    tokenizer: BertWordPieceTokenizer,
    session: Session,
    has_token_type_ids: bool,
    has_sentence_embedding: bool,
}

impl SemanticRuntime {
    pub(crate) fn load(use_gpu: bool, model_paths: &SemanticModelPaths) -> Result<Self> {
        eprintln!(
            "legal-mcp semantic: loading {} execution backend",
            if use_gpu { "CUDA" } else { "CPU" }
        );
        initialize_packaged_ort()?;
        let tokenizer = BertWordPieceTokenizer::from_file(&model_paths.tokenizer)?;

        // TensorRT must see the standard attention graph before ORT's extended
        // transformer fusions replace it with provider-specific contrib nodes.
        let optimization_level = ONLINE_MODEL_OPTIMIZATION_LEVEL;
        let mut builder = Session::builder()
            .map_err(|err| anyhow!("creating ONNX Runtime session: {err}"))?
            .with_optimization_level(optimization_level)
            .map_err(|err| anyhow!("configuring ONNX Runtime session: {err}"))?;
        if use_gpu {
            // CPU is the default runtime; maintainer GPU builds
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
        let has_sentence_embedding = session
            .outputs()
            .iter()
            .any(|output| output.name() == "sentence_embedding");

        Ok(Self {
            tokenizer,
            session,
            has_token_type_ids,
            has_sentence_embedding,
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

    pub(crate) fn encode_queries_with_stats(
        &mut self,
        queries: &[String],
    ) -> Result<(Vec<[i8; EMBEDDING_DIM]>, SemanticEncodeStats)> {
        self.encode_with_stats(queries, QUERY_EMBEDDING_PREFIX)
    }

    pub(crate) fn encode_documents_with_stats(
        &mut self,
        documents: &[String],
    ) -> Result<(Vec<[i8; EMBEDDING_DIM]>, SemanticEncodeStats)> {
        self.encode_with_stats(documents, DOCUMENT_EMBEDDING_PREFIX)
    }

    pub(crate) fn encode_document_token_ids_with_stats(
        &mut self,
        token_ids: &[&[i64]],
    ) -> Result<(Vec<[i8; EMBEDDING_DIM]>, SemanticEncodeStats)> {
        self.encode_token_ids_with_stats(token_ids)
    }

    fn encode_with_stats(
        &mut self,
        inputs: &[String],
        prefix: &str,
    ) -> Result<(Vec<[i8; EMBEDDING_DIM]>, SemanticEncodeStats)> {
        if inputs.is_empty() {
            return Ok((Vec::new(), SemanticEncodeStats::default()));
        }
        let prefixed = inputs
            .iter()
            .map(|input| format!("{prefix}{input}"))
            .collect::<Vec<_>>();
        let started = std::time::Instant::now();
        let encodings = prefixed
            .iter()
            .map(|input| self.tokenizer.encode(input))
            .collect::<Result<Vec<_>>>()?;
        let tokenize = started.elapsed();
        if encodings.len() != inputs.len() {
            bail!(
                "tokenizer returned {} encodings for {} inputs",
                encodings.len(),
                inputs.len()
            );
        }
        let slices = encodings.iter().map(Vec::as_slice).collect::<Vec<_>>();
        let (embeddings, mut stats) = self.encode_token_ids_with_stats(&slices)?;
        stats.tokenize += tokenize;
        Ok((embeddings, stats))
    }

    fn encode_token_ids_with_stats(
        &mut self,
        token_ids: &[&[i64]],
    ) -> Result<(Vec<[i8; EMBEDDING_DIM]>, SemanticEncodeStats)> {
        if token_ids.is_empty() {
            return Ok((Vec::new(), SemanticEncodeStats::default()));
        }
        let counts = token_ids.iter().map(|ids| ids.len()).collect::<Vec<_>>();
        ensure_token_counts_within_limit(&counts, EMBEDDING_INPUT_MAX_TOKENS)?;
        let batch = token_ids.len();
        let seq_len = counts.iter().copied().max().unwrap_or(0);
        if seq_len == 0 {
            bail!("semantic input produced no tokens");
        }
        let mut stats = SemanticEncodeStats::default();
        let started = std::time::Instant::now();
        let mut input_ids = Vec::with_capacity(batch * seq_len);
        let mut attention_mask = Vec::with_capacity(batch * seq_len);
        let mut active_tokens = 0usize;
        for ids in token_ids {
            input_ids.extend_from_slice(ids);
            attention_mask.resize(attention_mask.len() + ids.len(), 1);
            active_tokens += ids.len();
            input_ids.resize(input_ids.len() + seq_len - ids.len(), 0);
            attention_mask.resize(attention_mask.len() + seq_len - ids.len(), 0);
        }
        stats.record_batch(batch, seq_len, active_tokens);

        let input_ids_tensor =
            TensorRef::from_array_view(([batch, seq_len], input_ids.as_slice()))?;
        let attention_mask_tensor =
            TensorRef::from_array_view(([batch, seq_len], attention_mask.as_slice()))?;
        stats.prepare += started.elapsed();
        let started = std::time::Instant::now();
        let output_selector = self
            .has_sentence_embedding
            .then(|| OutputSelector::no_default().with("sentence_embedding"));
        let run_options = output_selector
            .map(|outputs| RunOptions::new().map(|options| options.with_outputs(outputs)))
            .transpose()?;
        let outputs = if self.has_token_type_ids {
            let token_type_ids = vec![0i64; batch * seq_len];
            let token_type_ids_tensor =
                TensorRef::from_array_view(([batch, seq_len], token_type_ids.as_slice()))?;
            let inputs = ort::inputs! {
                "input_ids" => input_ids_tensor,
                "attention_mask" => attention_mask_tensor,
                "token_type_ids" => token_type_ids_tensor,
            };
            if let Some(options) = &run_options {
                self.session.run_with_options(inputs, options)?
            } else {
                self.session.run(inputs)?
            }
        } else {
            let inputs = ort::inputs! {
                "input_ids" => input_ids_tensor,
                "attention_mask" => attention_mask_tensor,
            };
            if let Some(options) = &run_options {
                self.session.run_with_options(inputs, options)?
            } else {
                self.session.run(inputs)?
            }
        };
        stats.run += started.elapsed();
        let started = std::time::Instant::now();
        let output = outputs
            .get("sentence_embedding")
            .unwrap_or_else(|| &outputs[0]);
        // Prefer sentence_embedding when present; otherwise
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
    let cache = std::env::var_os("LEGAL_MCP_TENSORRT_CACHE_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("CUDA corpus builds require LEGAL_MCP_TENSORRT_CACHE_DIR"))?;
    std::fs::create_dir_all(&cache)
        .with_context(|| format!("creating TensorRT cache directory {}", cache.display()))?;
    let cache = cache
        .to_str()
        .ok_or_else(|| anyhow!("TensorRT cache path is not valid UTF-8"))?;
    let profile_min = "input_ids:1x1,attention_mask:1x1,token_type_ids:1x1";
    let profile_opt = format!(
        "input_ids:{}x384,attention_mask:{}x384,token_type_ids:{}x384",
        EMBEDDING_BATCH_SIZE / 2,
        EMBEDDING_BATCH_SIZE / 2,
        EMBEDDING_BATCH_SIZE / 2
    );
    let profile_max = format!(
        "input_ids:{EMBEDDING_BATCH_SIZE}x{EMBEDDING_INPUT_MAX_TOKENS},attention_mask:{EMBEDDING_BATCH_SIZE}x{EMBEDDING_INPUT_MAX_TOKENS},token_type_ids:{EMBEDDING_BATCH_SIZE}x{EMBEDDING_INPUT_MAX_TOKENS}"
    );
    let tensorrt = ep::TensorRT::default()
        .with_device_id(0)
        .with_fp16(true)
        .with_engine_cache(true)
        .with_engine_cache_path(cache)
        .with_timing_cache(true)
        .with_timing_cache_path(cache)
        .with_profile_min_shapes(profile_min)
        .with_profile_opt_shapes(profile_opt)
        .with_profile_max_shapes(profile_max)
        .build()
        .error_on_failure();
    let cuda = ep::CUDA::default()
        .with_device_id(0)
        .with_conv_algorithm_search(ep::cuda::ConvAlgorithmSearch::Heuristic)
        .build()
        .error_on_failure();
    builder
        .with_execution_providers([tensorrt, cuda])
        .map_err(|err| anyhow!("registering TensorRT/CUDA execution providers: {err}"))
}

#[cfg(not(feature = "cuda"))]
pub(crate) fn configure_cuda_execution_provider(
    _builder: SessionBuilder,
) -> Result<SessionBuilder> {
    bail!("GPU build requested but this legal-mcp binary was built without CUDA support; rebuild with `cargo build --release --features cuda`")
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
    // After L2 normalisation, values are clipped, scaled by 127,
    // rounded, and stored as int8 bytes.
    let mut out = [0i8; EMBEDDING_DIM];
    for (idx, value) in values.iter().enumerate() {
        out[idx] = ((*value / norm).clamp(-1.0, 1.0) * 127.0).round() as i8;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{ensure_token_counts_within_limit, SemanticModelPaths, SemanticRuntime};
    use anyhow::{Context, Result};

    #[test]
    fn actual_token_count_validation_rejects_the_first_oversize_input() {
        let error = match ensure_token_counts_within_limit(&[512, 513, 1024], 512) {
            Ok(()) => panic!("oversize semantic input was accepted"),
            Err(error) => error,
        };
        assert_eq!(
            error.to_string(),
            "semantic input 1 contains 513 tokens, exceeding the 512-token model limit"
        );
    }

    #[test]
    fn actual_token_count_validation_accepts_the_exact_limit() {
        assert!(ensure_token_counts_within_limit(&[0, 1, 512], 512).is_ok());
    }

    #[test]
    #[ignore = "requires LEGAL_MCP_BENCH_DB and LEGAL_MCP_TEST_MODEL_DIR"]
    fn benchmark_cuda_document_throughput() -> Result<()> {
        let db = std::env::var("LEGAL_MCP_BENCH_DB").context("LEGAL_MCP_BENCH_DB is required")?;
        let model = std::env::var("LEGAL_MCP_TEST_MODEL_DIR")
            .context("LEGAL_MCP_TEST_MODEL_DIR is required")?;
        let source =
            std::env::var("LEGAL_MCP_BENCH_SOURCE").unwrap_or_else(|_| "federal-court".to_string());
        let requested = std::env::var("LEGAL_MCP_BENCH_SAMPLES")
            .ok()
            .map(|value| value.parse::<usize>())
            .transpose()?
            .unwrap_or(5_000);
        let mut runtime = SemanticRuntime::load(
            true,
            &SemanticModelPaths::from_model_dir(std::path::Path::new(&model))?,
        )?;
        let connection = rusqlite::Connection::open_with_flags(
            db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let mut statement = connection
            .prepare("SELECT text FROM chunks WHERE source_id = ?1 ORDER BY chunk_id LIMIT ?2")?;
        let rows = statement.query_map(
            rusqlite::params![source, i64::try_from(requested * 8)?],
            |row| row.get::<_, Vec<u8>>(0),
        )?;
        let mut texts = Vec::with_capacity(requested);
        for row in rows {
            let text = crate::db::decompress_text(row?)?;
            let tokens = runtime.tokenizer.encode(text.as_str())?.len();
            if tokens <= crate::EMBEDDING_INPUT_MAX_TOKENS {
                texts.push((tokens, text));
                if texts.len() == requested {
                    break;
                }
            }
        }
        texts.sort_by_key(|(tokens, text)| (*tokens, text.len()));
        let started = std::time::Instant::now();
        let mut active_tokens = 0usize;
        let mut padded_tokens = 0usize;
        for batch in texts.chunks(64) {
            let inputs = batch
                .iter()
                .map(|(_, text)| text.clone())
                .collect::<Vec<_>>();
            let (_, stats) = runtime.encode_documents_with_stats(&inputs)?;
            active_tokens += stats.active_tokens;
            padded_tokens += stats.padded_tokens;
        }
        let elapsed = started.elapsed().as_secs_f64();
        eprintln!(
            "EMBED_BENCH inputs={} active_tokens={} elapsed_s={elapsed:.3} inputs_per_s={:.1} active_tokens_per_s={:.0} padding_efficiency={:.3}",
            texts.len(),
            active_tokens,
            texts.len() as f64 / elapsed,
            active_tokens as f64 / elapsed,
            active_tokens as f64 / padded_tokens as f64,
        );
        Ok(())
    }
}
