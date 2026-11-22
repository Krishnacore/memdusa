//! Built-in local embeddings: a small retrieval-trained sentence transformer, run in-process on
//! ONNX Runtime. No server, no API key, no data leaves the machine.
//!
//! Compiled only with the `local-embed` cargo feature. Selected at run time with
//! `MEMDUSA_EMBED_URL=local` (or `MEMDUSA_EMBED_PROVIDER=local`).
//!
//! The model files are fetched once, on explicit request only (`memdusa embed --download-model`),
//! into `<MEMDUSA_HOME>/models/<model>/` (default `~/.memdusa/models/<model>/`). Loading an embedder
//! — which happens in every hook — never downloads: a hook with a missing model simply fails to
//! embed and falls back to lexical search.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use fastembed::{InitOptionsUserDefined, Pooling, TextEmbedding, TokenizerFiles, UserDefinedEmbeddingModel};
use memdusa_core::embed::{EmbedFuture, Embedder};
use tokio::sync::{Mutex as AsyncMutex, OnceCell};

/// Prefix of the vector-space name stored with the embeddings (never mixed with HTTP models).
pub const MODEL_PREFIX: &str = "local:";
/// Default local model: sentence-transformers/all-MiniLM-L6-v2, ~90 MB, 384 dimensions.
pub const DEFAULT_MODEL: &str = "all-MiniLM-L6-v2";
/// How long a first model download may take.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);
/// Hugging Face repo and files for each supported model. All files are Apache-2.0/MIT licensed
/// model artifacts; nothing is used at run time beyond these five files.
const HF_BASE: &str = "https://huggingface.co";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSpec {
    /// Config key (`MEMDUSA_EMBED_MODEL`) and directory name.
    pub key: &'static str,
    repo: &'static str,
    onnx: &'static str,
    pooling: Pooling,
    /// Approximate download size, for the one-line progress message.
    approx_mb: u32,
}

/// Supported models. MiniLM is the default (smallest, fast enough for prompt recall); BGE small
/// is the higher-quality alternative. Both are sentence-transformers retrieval models.
pub const MODELS: &[ModelSpec] = &[
    ModelSpec {
        key: "all-MiniLM-L6-v2",
        repo: "Qdrant/all-MiniLM-L6-v2-onnx",
        onnx: "model.onnx",
        pooling: Pooling::Mean,
        approx_mb: 91,
    },
    ModelSpec {
        key: "bge-small-en-v1.5",
        repo: "Xenova/bge-small-en-v1.5",
        onnx: "onnx/model.onnx",
        pooling: Pooling::Cls,
        approx_mb: 133,
    },
];

/// Resolve a user-facing model name (aliases included) to a spec.
pub fn spec(name: &str) -> Option<&'static ModelSpec> {
    let n = name.trim().trim_start_matches(MODEL_PREFIX).to_ascii_lowercase();
    MODELS.iter().find(|m| {
        m.key.eq_ignore_ascii_case(&n)
            || (n == "minilm" && m.key == DEFAULT_MODEL)
            || (n == "bge-small" && m.key == "bge-small-en-v1.5")
    })
}

/// The vector-space name stored in the database.
pub fn model_id(spec: &ModelSpec) -> String {
    format!("{MODEL_PREFIX}{}", spec.key)
}

/// Local file names in `<models>/<key>/` (the ONNX file is always called `model.onnx` locally).
fn files() -> [&'static str; 5] {
    ["model.onnx", "config.json", "special_tokens_map.json", "tokenizer.json", "tokenizer_config.json"]
}

pub struct LocalEmbedder {
    spec: &'static ModelSpec,
    /// Stable vector-space name, borrowed by the `Embedder` trait.
    id: String,
    dir: PathBuf,
    /// The tokenizer + ONNX session; loaded at most once per process.
    model: OnceCell<Arc<Mutex<TextEmbedding>>>,
    /// Two commands asking for a download at the same time must not race.
    downloading: AsyncMutex<()>,
}

impl LocalEmbedder {
    /// Build an embedder without touching the network. `dir` is `<MEMDUSA_HOME>/models`.
    pub fn new(model_name: &str, dir: PathBuf) -> anyhow::Result<Self> {
        let spec = spec(model_name)
            .with_context(|| format!("unknown local embedding model {model_name:?} (try {})", MODELS[0].key))?;
        Ok(Self { id: model_id(spec), spec, dir, model: OnceCell::new(), downloading: AsyncMutex::new(()) })
    }

    pub fn spec(&self) -> &'static ModelSpec {
        self.spec
    }

    pub fn model_dir(&self) -> PathBuf {
        self.dir.join(self.spec.key)
    }

    /// Whether the model is fully present on disk (no network involved).
    pub fn is_ready(&self) -> bool {
        files().iter().all(|f| self.model_dir().join(f).metadata().is_ok_and(|m| m.is_file() && m.len() > 0))
    }

    /// Fetch the model if it is missing. Only called by an explicit CLI command, never by a hook.
    pub async fn prepare(&self) -> anyhow::Result<()> {
        let _guard = self.downloading.lock().await;
        if !self.is_ready() {
            let dir = self.model_dir();
            eprintln!(
                "memdusa: downloading local embedding model {} (~{} MB) into {}",
                self.spec.key,
                self.spec.approx_mb,
                dir.display()
            );
            self.download().await?;
            eprintln!("memdusa: local embedding model {} ready (offline from now on)", self.spec.key);
        }
        // Fail here, with a clear message, on a corrupt/truncated download rather than at the next hook.
        self.load().await.map_err(anyhow::Error::msg)?;
        Ok(())
    }

    /// Download each file into a temporary name, then rename, so an interrupted download is never
    /// mistaken for a cached model.
    async fn download(&self) -> anyhow::Result<()> {
        let dir = self.model_dir();
        tokio::fs::create_dir_all(&dir).await.with_context(|| format!("creating {}", dir.display()))?;
        let client = reqwest::Client::builder().timeout(DOWNLOAD_TIMEOUT).build()?;
        for file in files() {
            let url = format!("{HF_BASE}/{}/resolve/main/{}", self.spec.repo, self.remote_path(file));
            let dest = dir.join(file);
            let tmp = dir.join(format!("{file}.part"));
            let resp = client
                .get(&url)
                .send()
                .await
                .with_context(|| format!("downloading {url}"))?
                .error_for_status()
                .with_context(|| format!("downloading {url}"))?;
            let bytes = resp.bytes().await.with_context(|| format!("reading {url}"))?;
            tokio::fs::write(&tmp, &bytes).await.with_context(|| format!("writing {}", tmp.display()))?;
            tokio::fs::rename(&tmp, &dest).await.with_context(|| format!("moving {}", dest.display()))?;
        }
        Ok(())
    }

    /// Path of a file in this model relative to its Hugging Face repo.
    fn remote_path<'a>(&self, local: &'a str) -> &'a str {
        if local == "model.onnx" { self.spec.onnx } else { local }
    }

    /// Read the cached files and build an ONNX session, once per process.
    async fn load(&self) -> std::result::Result<Arc<Mutex<TextEmbedding>>, String> {
        self.model
            .get_or_try_init(|| async {
                if !self.is_ready() {
                    return Err(format!(
                        "local embedding model {} is not downloaded; run `memdusa embed --download-model` \
                         (nothing is downloaded automatically during hooks)",
                        self.spec.key
                    ));
                }
                let dir = self.model_dir();
                let pooling = self.spec.pooling.clone();
                let model = tokio::task::spawn_blocking(move || {
                    let read = |name: &str| {
                        std::fs::read(dir.join(name)).map_err(|e| format!("reading {}/{}: {e}", dir.display(), name))
                    };
                    let model = UserDefinedEmbeddingModel::new(
                        read("model.onnx")?,
                        TokenizerFiles {
                            tokenizer_file: read("tokenizer.json")?,
                            config_file: read("config.json")?,
                            special_tokens_map_file: read("special_tokens_map.json")?,
                            tokenizer_config_file: read("tokenizer_config.json")?,
                        },
                    )
                    .with_pooling(pooling);
                    // Short texts only: 256 tokens covers a memory or a prompt; less work per query.
                    let opts = InitOptionsUserDefined::default().with_max_length(256);
                    TextEmbedding::try_new_from_user_defined(model, opts)
                        .map_err(|e| format!("loading local embedding model: {e}"))
                })
                .await
                .map_err(|e| format!("loading local embedding model: {e}"))??;
                Ok(Arc::new(Mutex::new(model)))
            })
            .await
            .cloned()
    }
}

impl Embedder for LocalEmbedder {
    fn model(&self) -> &str {
        &self.id
    }

    fn embed<'a>(&'a self, texts: &'a [String]) -> EmbedFuture<'a> {
        Box::pin(async move {
            let model = self.load().await?;
            let owned = texts.to_vec();
            tokio::task::spawn_blocking(move || {
                let mut guard = model.lock().map_err(|e| format!("local embedder lock poisoned: {e}"))?;
                guard.embed(&owned, None).map_err(|e| format!("local embedding failed: {e}"))
            })
            .await
            .map_err(|e| format!("local embedding task failed: {e}"))?
        })
    }
}

#[cfg(test)]
mod quality;

/// Used by `memdusa status` / `memdusa embed` to tell the user how to fetch the model.
pub fn missing_model_hint(spec: &ModelSpec) -> String {
    format!(
        "local model {} not installed; run `memdusa embed --download-model` (~{} MB, into <MEMDUSA_HOME>/models/)",
        spec.key, spec.approx_mb
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_names_and_paths() {
        assert_eq!(spec("local:all-MiniLM-L6-v2").unwrap().key, DEFAULT_MODEL);
        assert_eq!(spec(" MINILM ").unwrap().key, DEFAULT_MODEL);
        assert_eq!(spec("bge-small").unwrap().key, "bge-small-en-v1.5");
        assert!(spec("gpt-x").is_none());
        let e = LocalEmbedder::new("minilm", PathBuf::from("/tmp/models")).unwrap();
        assert_eq!(model_id(e.spec()), "local:all-MiniLM-L6-v2");
        assert_eq!(e.model_dir(), PathBuf::from("/tmp/models/all-MiniLM-L6-v2"));
        assert!(!e.is_ready(), "a fresh dir has no model");
    }

    #[tokio::test]
    async fn missing_model_never_downloads_in_a_hook() {
        let e = LocalEmbedder::new(DEFAULT_MODEL, PathBuf::from("/nonexistent-memdusa-models")).unwrap();
        let texts = vec!["hello".to_owned()];
        let err = e.embed(&texts).await.unwrap_err();
        assert!(err.contains("memdusa embed --download-model"), "{err}");
        assert!(!err.contains('\n'), "{err}");
    }
}
