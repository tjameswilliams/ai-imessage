//! Embedding providers: turn chunk text into vectors, locally by default.
//!
//! Providers:
//! - `embedded` — bundled ONNX model via fastembed; everything stays on
//!   this machine (the model weights themselves are downloaded once, into
//!   the index directory — no user data is ever sent anywhere).
//! - `openai-compatible` — POST /embeddings to a configured endpoint.
//!   Non-loopback endpoints require `privacy.allow_remote_embedding_endpoint`.
//! - `debug-hash` — deterministic bag-of-words vectors for tests; no model,
//!   no network, no semantic quality.

use std::path::Path;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::config::Config;

/// BGE models want queries (not passages) prefixed with an instruction.
const BGE_QUERY_PREFIX: &str = "Represent this sentence for searching relevant passages: ";

pub trait Embedder {
    /// Identity of the provider+model, stored with the vectors; a change
    /// invalidates every stored embedding.
    fn id(&self) -> String;
    fn dims(&self) -> usize;
    /// Embed passages (chunk texts).
    fn embed_docs(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
    /// Embed a search query (may apply a model-specific prefix).
    fn embed_query(&mut self, text: &str) -> Result<Vec<f32>>;
}

/// Build the configured provider. `model_cache_dir` is where the embedded
/// provider keeps downloaded model weights.
pub fn make_embedder(config: &Config, model_cache_dir: &Path) -> Result<Box<dyn Embedder>> {
    match config.embeddings.provider.as_str() {
        "embedded" => Ok(Box::new(FastembedProvider::new(
            &config.embeddings.model,
            model_cache_dir,
            config.embeddings.batch_size as usize,
        )?)),
        "openai-compatible" => {
            let base_url =
                config.embeddings.base_url.as_deref().context(
                    "embeddings.base_url is required for the openai-compatible provider",
                )?;
            if !is_loopback_url(base_url) && !config.privacy.allow_remote_embedding_endpoint {
                bail!(
                    "embeddings.base_url {base_url} is not a loopback address; \
                     sending message content there requires \
                     privacy.allow_remote_embedding_endpoint = true"
                );
            }
            Ok(Box::new(OpenAiCompatProvider {
                base_url: base_url.trim_end_matches('/').to_string(),
                model: config.embeddings.model.clone(),
                api_key: config.embeddings.api_key.clone(),
                dims: None,
            }))
        }
        // Deterministic and offline; for tests and demos only.
        "debug-hash" => Ok(Box::new(DebugHashProvider)),
        other => bail!(
            "unknown embeddings.provider \"{other}\" \
             (expected \"embedded\" or \"openai-compatible\")"
        ),
    }
}

fn is_loopback_url(url: &str) -> bool {
    let rest = url.split("//").nth(1).unwrap_or(url);
    // Bracketed IPv6 hosts contain colons: take everything up to `]`.
    let host = if let Some(stripped) = rest.strip_prefix('[') {
        stripped.split(']').next().unwrap_or("")
    } else {
        rest.split(['/', ':']).next().unwrap_or("")
    };
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

// ---------------------------------------------------------------- embedded

struct FastembedProvider {
    model_name: String,
    inner: fastembed::TextEmbedding,
    batch_size: usize,
}

impl FastembedProvider {
    fn new(model_name: &str, cache_dir: &Path, batch_size: usize) -> Result<Self> {
        use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};
        let model = match model_name {
            "bge-small-en-v1.5" => EmbeddingModel::BGESmallENV15,
            "all-MiniLM-L6-v2" => EmbeddingModel::AllMiniLML6V2,
            other => bail!(
                "unsupported embedded model \"{other}\" \
                 (supported: bge-small-en-v1.5, all-MiniLM-L6-v2)"
            ),
        };
        std::fs::create_dir_all(cache_dir)
            .with_context(|| format!("could not create model cache {}", cache_dir.display()))?;
        let inner = TextEmbedding::try_new(
            TextInitOptions::new(model)
                .with_cache_dir(cache_dir.to_path_buf())
                .with_show_download_progress(true),
        )
        .context("could not load the embedded model (first run downloads it)")?;
        Ok(Self {
            model_name: model_name.to_string(),
            inner,
            batch_size: batch_size.max(1),
        })
    }
}

impl Embedder for FastembedProvider {
    fn id(&self) -> String {
        format!("embedded/{}", self.model_name)
    }

    fn dims(&self) -> usize {
        match self.model_name.as_str() {
            "bge-small-en-v1.5" | "all-MiniLM-L6-v2" => 384,
            _ => 384,
        }
    }

    fn embed_docs(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        self.inner.embed(texts, Some(self.batch_size))
    }

    fn embed_query(&mut self, text: &str) -> Result<Vec<f32>> {
        let prefixed = if self.model_name.starts_with("bge-") {
            format!("{BGE_QUERY_PREFIX}{text}")
        } else {
            text.to_string()
        };
        let mut out = self.inner.embed(&[prefixed], None)?;
        out.pop().context("model returned no embedding")
    }
}

// ------------------------------------------------------- openai-compatible

struct OpenAiCompatProvider {
    base_url: String,
    model: String,
    api_key: Option<String>,
    dims: Option<usize>,
}

impl OpenAiCompatProvider {
    fn request(&mut self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
        #[derive(serde::Deserialize)]
        struct Resp {
            data: Vec<Item>,
        }
        #[derive(serde::Deserialize)]
        struct Item {
            index: usize,
            embedding: Vec<f32>,
        }

        let mut req = ureq::post(&format!("{}/embeddings", self.base_url));
        if let Some(key) = &self.api_key {
            req = req.set("Authorization", &format!("Bearer {key}"));
        }
        let resp: Resp = req
            .send_json(serde_json::json!({ "model": self.model, "input": inputs }))
            .with_context(|| format!("embeddings request to {} failed", self.base_url))?
            .into_json()
            .context("embeddings endpoint returned unexpected JSON")?;

        let mut items = resp.data;
        items.sort_by_key(|i| i.index);
        if items.len() != inputs.len() {
            bail!(
                "embeddings endpoint returned {} vectors for {} inputs",
                items.len(),
                inputs.len()
            );
        }
        if let Some(first) = items.first() {
            self.dims.get_or_insert(first.embedding.len());
        }
        Ok(items.into_iter().map(|i| i.embedding).collect())
    }
}

impl Embedder for OpenAiCompatProvider {
    fn id(&self) -> String {
        format!("openai-compatible/{}", self.model)
    }

    fn dims(&self) -> usize {
        self.dims.unwrap_or(0)
    }

    fn embed_docs(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        self.request(texts)
    }

    fn embed_query(&mut self, text: &str) -> Result<Vec<f32>> {
        let mut out = self.request(&[text.to_string()])?;
        out.pop().context("endpoint returned no embedding")
    }
}

// ------------------------------------------------------------- debug-hash

const DEBUG_DIMS: usize = 64;

/// Bag-of-words into a fixed number of hash buckets, L2-normalized.
/// Deterministic across runs and platforms.
struct DebugHashProvider;

fn debug_hash_vector(text: &str) -> Vec<f32> {
    let mut v = vec![0f32; DEBUG_DIMS];
    for token in text.to_lowercase().split_whitespace() {
        let digest = Sha256::digest(token.as_bytes());
        let bucket = usize::from(digest[0]) % DEBUG_DIMS;
        v[bucket] += 1.0;
    }
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in &mut v {
            *x /= norm;
        }
    }
    v
}

impl Embedder for DebugHashProvider {
    fn id(&self) -> String {
        "debug-hash".into()
    }

    fn dims(&self) -> usize {
        DEBUG_DIMS
    }

    fn embed_docs(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|t| debug_hash_vector(t)).collect())
    }

    fn embed_query(&mut self, text: &str) -> Result<Vec<f32>> {
        Ok(debug_hash_vector(text))
    }
}

// ----------------------------------------------------------------- vectors

/// Cosine similarity; assumes nothing about normalization.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let (mut dot, mut na, mut nb) = (0f32, 0f32, 0f32);
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na.sqrt() * nb.sqrt())
    }
}

pub fn vector_to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

pub fn blob_to_vector(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with(provider: &str, base_url: Option<&str>, allow_remote: bool) -> Config {
        let mut c = Config::default();
        c.embeddings.provider = provider.into();
        c.embeddings.base_url = base_url.map(String::from);
        c.privacy.allow_remote_embedding_endpoint = allow_remote;
        c
    }

    #[test]
    fn debug_hash_is_deterministic_and_normalized() {
        let mut p = DebugHashProvider;
        let a = p.embed_query("hello world").unwrap();
        let b = p.embed_query("hello world").unwrap();
        assert_eq!(a, b);
        let norm: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5);
    }

    #[test]
    fn debug_hash_scores_shared_words_higher() {
        let mut p = DebugHashProvider;
        let docs = p
            .embed_docs(&[
                "pizza dinner tonight".into(),
                "quarterly budget review".into(),
            ])
            .unwrap();
        let q = p.embed_query("pizza tonight").unwrap();
        assert!(cosine(&q, &docs[0]) > cosine(&q, &docs[1]));
    }

    #[test]
    fn unknown_provider_is_an_error() {
        let cfg = config_with("telepathy", None, false);
        let dir = tempfile::tempdir().unwrap();
        assert!(make_embedder(&cfg, dir.path()).is_err());
    }

    #[test]
    fn remote_endpoint_requires_the_privacy_flag() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config_with(
            "openai-compatible",
            Some("https://api.example.com/v1"),
            false,
        );
        let err = match make_embedder(&cfg, dir.path()) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("expected the privacy guard to reject a remote endpoint"),
        };
        assert!(err.contains("allow_remote_embedding_endpoint"));

        let cfg = config_with(
            "openai-compatible",
            Some("https://api.example.com/v1"),
            true,
        );
        assert!(make_embedder(&cfg, dir.path()).is_ok());
    }

    #[test]
    fn loopback_endpoints_never_need_the_privacy_flag() {
        let dir = tempfile::tempdir().unwrap();
        for url in [
            "http://localhost:1234/v1",
            "http://127.0.0.1:8080/v1",
            "http://[::1]:11434/v1",
        ] {
            let cfg = config_with("openai-compatible", Some(url), false);
            assert!(make_embedder(&cfg, dir.path()).is_ok(), "{url}");
        }
    }

    #[test]
    fn missing_base_url_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config_with("openai-compatible", None, false);
        assert!(make_embedder(&cfg, dir.path()).is_err());
    }

    #[test]
    fn blob_roundtrip_preserves_vectors() {
        let v = vec![0.25f32, -1.5, 3.25, 0.0];
        assert_eq!(blob_to_vector(&vector_to_blob(&v)), v);
    }

    #[test]
    fn cosine_of_identical_vectors_is_one() {
        let v = vec![0.6f32, 0.8];
        assert!((cosine(&v, &v) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_handles_mismatched_or_zero_vectors() {
        assert_eq!(cosine(&[1.0, 0.0], &[1.0]), 0.0);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 0.0]), 0.0);
        assert_eq!(cosine(&[], &[]), 0.0);
    }
}
