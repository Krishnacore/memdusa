//! Embedding provider over HTTP: the OpenAI-compatible `POST {base}/embeddings` API, which OpenAI,
//! Ollama (`http://localhost:11434/v1`) and many other servers speak. Configured from the environment
//! of the process that owns the store (on a server: the server's env); off unless `MEMDUSA_EMBED_URL` is set.

use std::sync::Arc;
use std::time::Duration;

use memdusa_core::embed::{BATCH, EmbedFuture, Embedder};
use serde::Deserialize;

pub const DEFAULT_MODEL: &str = "text-embedding-3-small";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// How much of an error body is kept in a warning.
const ERROR_BODY_CHARS: usize = 200;

pub struct HttpEmbedder {
    client: reqwest::Client,
    /// `{base}/embeddings`
    endpoint: String,
    model: String,
    api_key: Option<String>,
    batch: usize,
}

#[derive(Deserialize)]
struct Response {
    data: Vec<Item>,
}

#[derive(Deserialize)]
struct Item {
    #[serde(default)]
    index: Option<usize>,
    embedding: Vec<f32>,
}

/// The key to send: the explicit one, else `OPENAI_API_KEY` but only when talking to api.openai.com,
/// so that key never leaks to another provider.
fn api_key(url: &str, own: Option<String>, openai: Option<String>) -> Option<String> {
    own.or_else(|| {
        let host = reqwest::Url::parse(url).ok()?.host_str()?.to_ascii_lowercase();
        if host == "api.openai.com" { openai } else { None }
    })
}

impl HttpEmbedder {
    pub fn new(base_url: &str, model: &str, api_key: Option<String>, timeout: Duration) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder().timeout(timeout).build()?;
        Ok(Self {
            client,
            endpoint: format!("{}/embeddings", base_url.trim_end_matches('/')),
            model: model.to_owned(),
            api_key: api_key.filter(|k| !k.trim().is_empty()),
            batch: BATCH,
        })
    }

    /// The provider configured by `MEMDUSA_EMBED_URL` / `MEMDUSA_EMBED_MODEL` / `MEMDUSA_EMBED_API_KEY`
    /// (`OPENAI_API_KEY` is used only for api.openai.com); `None` when semantic search is off.
    pub fn from_env() -> anyhow::Result<Option<Arc<dyn Embedder>>> {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> anyhow::Result<Option<Arc<dyn Embedder>>> {
        let val = |k: &str| get(k).map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());
        let Some(url) = val("MEMDUSA_EMBED_URL") else { return Ok(None) };
        let model = val("MEMDUSA_EMBED_MODEL").unwrap_or_else(|| DEFAULT_MODEL.into());
        let api_key = api_key(&url, val("MEMDUSA_EMBED_API_KEY"), val("OPENAI_API_KEY"));
        Ok(Some(Arc::new(Self::new(&url, &model, api_key, REQUEST_TIMEOUT)?)))
    }

    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        let mut req =
            self.client.post(&self.endpoint).json(&serde_json::json!({ "model": self.model, "input": texts }));
        if let Some(k) = &self.api_key {
            req = req.bearer_auth(k);
        }
        let resp = req.send().await.map_err(|e| {
            // reqwest errors can carry the URL; it holds no secret (the key is a header)
            if e.is_timeout() { "request timed out".to_owned() } else { format!("request failed: {e}") }
        })?;
        let status = resp.status();
        let body = resp.text().await.map_err(|e| format!("reading the response failed: {e}"))?;
        if !status.is_success() {
            let snippet: String = body.chars().take(ERROR_BODY_CHARS).collect();
            return Err(format!("HTTP {status}: {}", snippet.trim()));
        }
        let mut data = serde_json::from_str::<Response>(&body)
            .map_err(|e| format!("unexpected response (no data[].embedding): {e}"))?
            .data;
        if data.len() != texts.len() {
            return Err(format!("sent {} text(s), got {} embedding(s)", texts.len(), data.len()));
        }
        // the API returns `index`; honour it in case the order differs
        if data.iter().all(|d| d.index.is_some()) {
            data.sort_by_key(|d| d.index);
        }
        Ok(data.into_iter().map(|d| d.embedding).collect())
    }
}

impl Embedder for HttpEmbedder {
    fn model(&self) -> &str {
        &self.model
    }

    fn embed<'a>(&'a self, texts: &'a [String]) -> EmbedFuture<'a> {
        Box::pin(async move {
            let mut out = Vec::with_capacity(texts.len());
            for chunk in texts.chunks(self.batch) {
                out.extend(self.embed_batch(chunk).await?);
            }
            Ok(out)
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::post;
    use axum::{Json, Router};
    use serde_json::{Value, json};

    use super::*;

    type Seen = Arc<Mutex<Vec<(Option<String>, Value)>>>;

    /// A tiny OpenAI-compatible server; `mode` picks the behaviour. Returns its base URL.
    async fn server(mode: &'static str) -> (String, Seen) {
        let seen: Seen = Default::default();
        async fn handler(
            State((mode, seen)): State<(&'static str, Seen)>,
            headers: HeaderMap,
            Json(body): Json<Value>,
        ) -> (StatusCode, Json<Value>) {
            let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).map(String::from);
            seen.lock().unwrap().push((auth, body.clone()));
            let inputs = body["input"].as_array().cloned().unwrap_or_default();
            match mode {
                "error" => (StatusCode::UNAUTHORIZED, Json(json!({ "error": { "message": "bad key" } }))),
                "slow" => {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    (StatusCode::OK, Json(json!({ "data": [] })))
                }
                "short" => (StatusCode::OK, Json(json!({ "data": [] }))),
                // reversed order, so the `index` field matters; the vector is [len, index]
                _ => {
                    let data: Vec<Value> = inputs
                        .iter()
                        .enumerate()
                        .rev()
                        .map(|(i, t)| {
                            json!({ "object": "embedding", "index": i, "embedding": [t.as_str().unwrap().len(), i] })
                        })
                        .collect();
                    (StatusCode::OK, Json(json!({ "data": data })))
                }
            }
        }
        let app = Router::new().route("/v1/embeddings", post(handler)).with_state((mode, seen.clone()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}/v1/"), seen)
    }

    fn texts(n: usize) -> Vec<String> {
        (0..n).map(|i| "x".repeat(i + 1)).collect()
    }

    #[tokio::test]
    async fn maps_request_and_response_in_batches() {
        let (url, seen) = server("ok").await;
        let e = HttpEmbedder::new(&url, "my-model", Some("sekret".into()), Duration::from_secs(5)).unwrap();
        assert_eq!(e.model(), "my-model");
        let out = e.embed(&texts(70)).await.unwrap();
        assert_eq!(out.len(), 70);
        // ordered by index within each batch of 64, despite the server's reversed order
        assert_eq!(out[0], vec![1.0, 0.0]);
        assert_eq!(out[63], vec![64.0, 63.0]);
        assert_eq!(out[64], vec![65.0, 0.0], "second batch restarts at index 0");

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "64 + 6");
        assert_eq!(seen[0].0.as_deref(), Some("Bearer sekret"));
        assert_eq!(seen[0].1["model"], "my-model");
        assert_eq!(seen[0].1["input"].as_array().unwrap().len(), 64);
        assert_eq!(seen[1].1["input"].as_array().unwrap().len(), 6);
    }

    #[tokio::test]
    async fn errors_are_clear_one_liners() {
        let (url, _) = server("error").await;
        let e = HttpEmbedder::new(&url, "m", Some("sekret".into()), Duration::from_secs(5)).unwrap();
        let err = e.embed(&texts(1)).await.unwrap_err();
        assert!(err.starts_with("HTTP 401"), "{err}");
        assert!(err.contains("bad key"), "{err}");
        assert!(!err.contains("sekret"), "{err}");
        assert!(!err.contains('\n'), "{err}");

        let (url, _) = server("short").await;
        let e = HttpEmbedder::new(&url, "m", None, Duration::from_secs(5)).unwrap();
        assert!(e.embed(&texts(2)).await.unwrap_err().contains("got 0 embedding"));

        let e = HttpEmbedder::new("http://127.0.0.1:1/v1", "m", None, Duration::from_secs(2)).unwrap();
        // unreachable: refused at once on Unix; Windows retries a refused connection for ~2s, so it may time out
        let err = e.embed(&texts(1)).await.unwrap_err();
        assert!(err.starts_with("request failed") || err == "request timed out", "{err}");
    }

    #[tokio::test]
    async fn timeout_is_an_error() {
        let (url, _) = server("slow").await;
        let e = HttpEmbedder::new(&url, "m", None, Duration::from_millis(200)).unwrap();
        assert_eq!(e.embed(&texts(1)).await.unwrap_err(), "request timed out");
    }

    #[test]
    fn config_from_env() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
        };
        assert!(HttpEmbedder::from_lookup(env(&[("OPENAI_API_KEY", "k")])).unwrap().is_none(), "off by default");
        assert!(HttpEmbedder::from_lookup(env(&[("MEMDUSA_EMBED_URL", "  ")])).unwrap().is_none());
        let e = HttpEmbedder::from_lookup(env(&[("MEMDUSA_EMBED_URL", "http://localhost:11434/v1")])).unwrap().unwrap();
        assert_eq!(e.model(), DEFAULT_MODEL);
        let e = HttpEmbedder::from_lookup(env(&[
            ("MEMDUSA_EMBED_URL", "http://localhost:11434/v1"),
            ("MEMDUSA_EMBED_MODEL", "nomic-embed-text"),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(e.model(), "nomic-embed-text");
    }

    #[test]
    fn api_key_fallback_only_for_openai() {
        let s = |v: &str| Some(v.to_owned());
        assert_eq!(api_key("https://api.openai.com/v1", None, s("oa")), s("oa"));
        assert_eq!(api_key("https://api.openai.com/v1", s("own"), s("oa")), s("own"));
        assert_eq!(api_key("http://localhost:11434/v1", None, s("oa")), None);
        assert_eq!(api_key("http://localhost:11434/v1", s("own"), s("oa")), s("own"));
        assert_eq!(api_key("https://api.openai.com.evil.example/v1", None, s("oa")), None);
    }
}
