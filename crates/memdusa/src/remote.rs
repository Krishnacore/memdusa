//! REST client for a remote `memdusa serve`, used by hooks and the CLI in
//! remote mode.

use std::time::Duration;

use anyhow::{Context, bail};
use memdusa_core::model::{ApiKey, CreatedKey, GitState, Memory, MemoryQuery, NewMemory, User};
use reqwest::{Method, RequestBuilder};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::config::Remote;
use crate::hooks::{HookEvent, HookRequest, HookResponse};

pub struct Client {
    http: reqwest::Client,
    remote: Remote,
}

impl Client {
    pub fn new(remote: Remote, timeout: Duration) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .connect_timeout(timeout.min(Duration::from_secs(3)))
            .user_agent(concat!("memdusa/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self { http, remote })
    }

    fn request(&self, method: Method, path: &str) -> RequestBuilder {
        let req = self.http.request(method, format!("{}{path}", self.remote.url));
        match &self.remote.token {
            Some(t) => req.bearer_auth(t),
            None => req,
        }
    }

    async fn send<T: DeserializeOwned>(&self, req: RequestBuilder) -> anyhow::Result<T> {
        let resp = req.send().await.with_context(|| format!("contacting {}", self.remote.url))?;
        let status = resp.status();
        if !status.is_success() {
            let body: Value = resp.json().await.unwrap_or_default();
            let msg = body.get("error").and_then(Value::as_str).unwrap_or("no details");
            bail!("{} returned {status}: {msg}", self.remote.url);
        }
        Ok(resp.json().await?)
    }

    pub async fn whoami(&self) -> anyhow::Result<Value> {
        self.send(self.request(Method::GET, "/v1/whoami")).await
    }

    pub async fn hook(&self, event: HookEvent, req: &HookRequest) -> anyhow::Result<HookResponse> {
        self.send(self.request(Method::POST, &format!("/v1/hooks/{}", event.arg())).json(req)).await
    }

    /// The brief as JSON (its `markdown` field holds the rendered text).
    pub async fn resume(&self, project: &str, git: Option<&GitState>) -> anyhow::Result<Value> {
        let mut q = vec![("project", project.to_owned())];
        if let Some(g) = git {
            q.push(("git", serde_json::to_string(g)?));
        }
        self.send(self.request(Method::GET, "/v1/resume").query(&q)).await
    }

    pub async fn memory_save(&self, m: &NewMemory) -> anyhow::Result<Memory> {
        self.send(self.request(Method::POST, "/v1/memories").json(m)).await
    }

    pub async fn memory_search(&self, q: &MemoryQuery) -> anyhow::Result<Vec<Memory>> {
        self.send(self.request(Method::GET, "/v1/memories").query(&Query(q))).await
    }

    // ------------------------------------------------------------ users & keys

    pub async fn user_list(&self) -> anyhow::Result<Vec<User>> {
        self.send(self.request(Method::GET, "/v1/admin/users")).await
    }

    pub async fn user_add(
        &self,
        name: &str,
        admin: bool,
        key_name: &str,
    ) -> anyhow::Result<crate::http::NewUserResponse> {
        let body = json!({ "name": name, "admin": admin, "key_name": key_name });
        self.send(self.request(Method::POST, "/v1/admin/users").json(&body)).await
    }

    pub async fn user_set_disabled(&self, name: &str, disabled: bool) -> anyhow::Result<User> {
        let action = if disabled { "disable" } else { "enable" };
        self.send(self.request(Method::POST, &format!("/v1/admin/users/{name}/{action}"))).await
    }

    /// `user: None` = the caller's own keys.
    pub async fn key_list(&self, user: Option<&str>) -> anyhow::Result<Vec<ApiKey>> {
        match user {
            None => self.send(self.request(Method::GET, "/v1/me/keys")).await,
            Some(u) => self.send(self.request(Method::GET, "/v1/admin/keys").query(&[("user", u)])).await,
        }
    }

    pub async fn key_create(&self, user: Option<&str>, name: &str) -> anyhow::Result<CreatedKey> {
        let path = match user {
            None => "/v1/me/keys".to_string(),
            Some(u) => format!("/v1/admin/users/{u}/keys"),
        };
        self.send(self.request(Method::POST, &path).json(&json!({ "name": name }))).await
    }

    /// Admins can revoke anyone's key; others only their own.
    pub async fn key_revoke(&self, key: &str) -> anyhow::Result<ApiKey> {
        let admin = self.whoami().await?["admin"].as_bool().unwrap_or(false);
        let path = if admin { format!("/v1/admin/keys/{key}") } else { format!("/v1/me/keys/{key}") };
        self.send(self.request(Method::DELETE, &path)).await
    }
}

/// `MemoryQuery` as URL query parameters (enums as their snake_case names).
struct Query<'a>(&'a MemoryQuery);

impl Serialize for Query<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let q = self.0;
        let mut m = s.serialize_map(None)?;
        if let Some(v) = &q.query {
            m.serialize_entry("query", v)?;
        }
        if let Some(v) = &q.project {
            m.serialize_entry("project", v)?;
        }
        if let Some(v) = q.kind {
            m.serialize_entry("kind", v.as_str())?;
        }
        if let Some(v) = q.limit {
            m.serialize_entry("limit", &v)?;
        }
        m.end()
    }
}
