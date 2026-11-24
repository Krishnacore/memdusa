//! `memdusa mcp` in remote mode: a local stdio MCP server that forwards every
//! request to a remote memdusa server's `/mcp` endpoint.
//!
//! Why a proxy instead of pointing the harness at the URL directly:
//! * every harness that can launch a stdio server works, with no per-harness
//!   HTTP/header configuration;
//! * the token stays in `~/.memdusa/config.json`, never in files that get
//!   committed (like a project's `.mcp.json`);
//! * the project is detected locally (from git) and sent as a header, so the
//!   agent never has to pass `project` explicitly.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context;
use futures::stream::BoxStream;
use http::{HeaderName, HeaderValue};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ClientJsonRpcMessage, ContentBlock,
    GetPromptRequestParams, GetPromptResponse, Implementation, ListPromptsResult, ListResourceTemplatesResult,
    ListResourcesResult, ListToolsResult, PaginatedRequestParams, ReadResourceRequestParams, ReadResourceResponse,
    ServerCapabilities, ServerConfig,
};
use rmcp::service::{RequestContext, RunningService, ServiceError};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::{
    SseError, StreamableHttpClient, StreamableHttpClientTransportConfig, StreamableHttpError,
    StreamableHttpPostResponse, StreamableHttpResponseLimits,
};
use rmcp::{ErrorData as McpError, RoleClient, RoleServer, ServerHandler, ServiceExt};
use sse_stream::Sse;

use crate::config::Remote;
use crate::mcp::{AGENT_HEADER, PROJECT_HEADER};

struct Proxy {
    upstream: RunningService<RoleClient, ()>,
    instructions: Option<String>,
    /// Project detected from the directory this proxy runs in.
    project: Option<String>,
}

// --------------------------------------------------------- OAuth token source

#[derive(Debug, thiserror::Error)]
enum ProxyHttpError {
    #[error(transparent)]
    Reqwest(#[from] reqwest::Error),
    #[error("{0}")]
    Refresh(String),
}

type HttpResult<T> = Result<T, StreamableHttpError<ProxyHttpError>>;

/// A reqwest transport that asks for a fresh access token on every request
/// (refreshing under a lock when near expiry) and retries once on a 401.
#[derive(Clone)]
struct RefreshingHttp {
    inner: reqwest::Client,
    url: String,
}

impl RefreshingHttp {
    async fn token(&self, force: bool) -> Result<String, ProxyHttpError> {
        crate::cli_auth::access_token(&self.url, force)
            .await
            .map_err(|e| ProxyHttpError::Refresh(format!("{e}; run `memdusa login` again")))
    }
}

fn map_http_error(e: StreamableHttpError<reqwest::Error>) -> StreamableHttpError<ProxyHttpError> {
    use StreamableHttpError as E;
    match e {
        E::Sse(e) => E::Sse(e),
        E::Io(e) => E::Io(e),
        E::Client(e) => E::Client(e.into()),
        E::HttpStatus(e) => E::HttpStatus(e),
        E::UnexpectedEndOfStream => E::UnexpectedEndOfStream,
        E::UnexpectedServerResponse(s) => E::UnexpectedServerResponse(s),
        E::ResponseBodyTooLarge { max_size } => E::ResponseBodyTooLarge { max_size },
        E::UnexpectedContentType(c) => E::UnexpectedContentType(c),
        E::ServerDoesNotSupportSse => E::ServerDoesNotSupportSse,
        E::ServerDoesNotSupportDeleteSession => E::ServerDoesNotSupportDeleteSession,
        E::TokioJoinError(e) => E::TokioJoinError(e),
        E::Deserialize(e) => E::Deserialize(e),
        E::TransportChannelClosed => E::TransportChannelClosed,
        E::MissingSessionIdInResponse => E::MissingSessionIdInResponse,
        E::AuthRequired(e) => E::AuthRequired(e),
        E::InsufficientScope(e) => E::InsufficientScope(e),
        E::ReservedHeaderConflict(s) => E::ReservedHeaderConflict(s),
        E::SessionExpired => E::SessionExpired,
        E::SessionRecoveryTimeout => E::SessionRecoveryTimeout,
        E::ReinitializationFailed(e) => E::ReinitializationFailed(e),
        E::ControlRequestTimeout => E::ControlRequestTimeout,
        other => E::UnexpectedServerResponse(format!("unsupported transport error: {other}").into()),
    }
}

fn auth_error(e: ProxyHttpError) -> StreamableHttpError<ProxyHttpError> {
    StreamableHttpError::Client(e)
}

impl StreamableHttpClient for RefreshingHttp {
    type Error = ProxyHttpError;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> HttpResult<StreamableHttpPostResponse> {
        let token = self.token(false).await.map_err(auth_error)?;
        let result = self
            .inner
            .post_message(uri.clone(), message.clone(), session_id.clone(), Some(token), custom_headers.clone())
            .await;
        if matches!(&result, Err(StreamableHttpError::AuthRequired(_))) {
            let token = self.token(true).await.map_err(auth_error)?;
            return self
                .inner
                .post_message(uri, message, session_id, Some(token), custom_headers)
                .await
                .map_err(map_http_error);
        }
        result.map_err(map_http_error)
    }

    async fn post_message_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> HttpResult<StreamableHttpPostResponse> {
        let token = self.token(false).await.map_err(auth_error)?;
        self.inner
            .post_message_with_max_sse_event_size(
                uri,
                message,
                session_id,
                Some(token),
                custom_headers,
                max_sse_event_size,
            )
            .await
            .map_err(map_http_error)
    }

    async fn post_message_with_response_limits(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        limits: StreamableHttpResponseLimits,
    ) -> HttpResult<StreamableHttpPostResponse> {
        let token = self.token(false).await.map_err(auth_error)?;
        self.inner
            .post_message_with_response_limits(uri, message, session_id, Some(token), custom_headers, limits)
            .await
            .map_err(map_http_error)
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> HttpResult<()> {
        let token = self.token(false).await.map_err(auth_error)?;
        self.inner.delete_session(uri, session_id, Some(token), custom_headers).await.map_err(map_http_error)
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> HttpResult<BoxStream<'static, Result<Sse, SseError>>> {
        let token = self.token(false).await.map_err(auth_error)?;
        self.inner.get_stream(uri, session_id, last_event_id, Some(token), custom_headers).await.map_err(map_http_error)
    }

    async fn get_stream_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> HttpResult<BoxStream<'static, Result<Sse, SseError>>> {
        let token = self.token(false).await.map_err(auth_error)?;
        self.inner
            .get_stream_with_max_sse_event_size(
                uri,
                session_id,
                last_event_id,
                Some(token),
                custom_headers,
                max_sse_event_size,
            )
            .await
            .map_err(map_http_error)
    }
}

/// Upstream protocol errors (e.g. resource not found) pass through unchanged;
/// transport failures become a readable error.
fn upstream_err(e: ServiceError) -> McpError {
    match e {
        ServiceError::McpError(e) => e,
        e => McpError::internal_error(format!("memdusa server unreachable: {e}"), None),
    }
}

impl ServerHandler for Proxy {
    fn get_info(&self) -> ServerConfig {
        let info =
            ServerConfig::new(ServerCapabilities::builder().enable_tools().enable_resources().enable_prompts().build())
                .with_server_info(Implementation::new("memdusa", env!("CARGO_PKG_VERSION")));
        match &self.instructions {
            Some(i) => info.with_instructions(i.clone()),
            None => info,
        }
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        self.upstream
            .list_tools(request)
            .await
            .map_err(|e| McpError::internal_error(format!("memdusa server unreachable: {e}"), None))
    }

    async fn list_resources(
        &self,
        request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        self.upstream.list_resources(request).await.map_err(upstream_err)
    }

    async fn list_resource_templates(
        &self,
        request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, McpError> {
        self.upstream.list_resource_templates(request).await.map_err(upstream_err)
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, McpError> {
        self.upstream.read_resource(request).await.map(Into::into).map_err(upstream_err)
    }

    async fn list_prompts(
        &self,
        request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, McpError> {
        self.upstream.list_prompts(request).await.map_err(upstream_err)
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<GetPromptResponse, McpError> {
        self.upstream.get_prompt(request).await.map(Into::into).map_err(upstream_err)
    }

    async fn call_tool(
        &self,
        mut request: CallToolRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        // The server can't see this machine's repo: attach its code state.
        // Only when the call targets this directory's project, not another one.
        let target = request.arguments.as_ref().and_then(|a| a.get("project")).and_then(|p| p.as_str());
        if matches!(request.name.as_ref(), "checkpoint" | "resume")
            && let Some(project) = target.or(self.project.as_deref())
            && let Some(git) = crate::project::git_state_for(project)
            && let Ok(value) = serde_json::to_value(git)
        {
            request.arguments.get_or_insert_with(Default::default).entry("git").or_insert(value);
        }
        // A delegated worker's task (`MEMDUSA_TASK`) is this machine's environment too; an explicit argument wins.
        if matches!(request.name.as_ref(), "checkpoint" | "resume")
            && let Some(task) =
                std::env::var("MEMDUSA_TASK").ok().map(|t| t.trim().to_owned()).filter(|t| !t.is_empty())
        {
            request.arguments.get_or_insert_with(Default::default).entry("task").or_insert(task.into());
        }
        // Likewise a worker's finishing tools: its run and task come from this machine's environment.
        if matches!(request.name.as_ref(), "task_complete" | "task_fail") {
            for (arg, var) in [("task", "MEMDUSA_TASK"), ("run", "MEMDUSA_RUN")] {
                if let Some(v) = std::env::var(var).ok().map(|v| v.trim().to_owned()).filter(|v| !v.is_empty()) {
                    request.arguments.get_or_insert_with(Default::default).entry(arg).or_insert(v.into());
                }
            }
        }
        Ok(match self.upstream.call_tool(request).await {
            Ok(result) => result.into(),
            // Surface transport failures to the agent as a readable tool error.
            Err(e) => {
                CallToolResult::error(vec![ContentBlock::text(format!("memdusa server unreachable: {e}"))]).into()
            }
        })
    }
}

/// reqwest transport for the authenticated MCP upstream. `https_only` blocks
/// http redirects (including https→http downgrades) when the configured URL is
/// https. The redirect policy otherwise mirrors rmcp's default: only
/// same-origin 307/308, so headers and session ids are never replayed elsewhere.
fn upstream_http_client(url: &str) -> anyhow::Result<reqwest::Client> {
    let mut builder =
        reqwest::Client::builder().pool_max_idle_per_host(0).redirect(reqwest::redirect::Policy::custom(|attempt| {
            let preserves_body = matches!(
                attempt.status(),
                reqwest::StatusCode::TEMPORARY_REDIRECT | reqwest::StatusCode::PERMANENT_REDIRECT
            );
            let same_origin =
                attempt.previous().first().is_some_and(|original| original.origin() == attempt.url().origin());
            if !preserves_body || !same_origin {
                attempt.stop()
            } else if attempt.previous().len() > 10 {
                attempt.error("too many redirects")
            } else {
                attempt.follow()
            }
        }));
    if url.starts_with("https://") {
        builder = builder.https_only(true);
    }
    Ok(builder.build()?)
}

pub async fn run(remote: &Remote, project: Option<String>, agent: &str) -> anyhow::Result<()> {
    let mut headers = HashMap::new();
    headers.insert(HeaderName::from_static(AGENT_HEADER), HeaderValue::from_str(agent)?);
    if let Some(p) = &project {
        headers.insert(HeaderName::from_static(PROJECT_HEADER), HeaderValue::from_str(p)?);
    }
    let mut config =
        StreamableHttpClientTransportConfig::with_uri(format!("{}/mcp", remote.url)).custom_headers(headers);
    if let Some(t) = &remote.token {
        config = config.auth_header(t.clone());
    }
    let http = upstream_http_client(&remote.url)?;
    let oauth = remote.oauth.is_some();
    let upstream = if oauth {
        // Static headers can't rotate: the transport asks our client for the
        // current access token on every HTTP request instead.
        let refreshing = RefreshingHttp { inner: http, url: remote.url.clone() };
        ().serve(StreamableHttpClientTransport::with_client(refreshing, config))
            .await
            .with_context(|| format!("connecting to {}/mcp", remote.url))?
    } else {
        ().serve(StreamableHttpClientTransport::with_client(http, config))
            .await
            .with_context(|| format!("connecting to {}/mcp", remote.url))?
    };
    let instructions = upstream.peer_info().and_then(|i| i.instructions.clone());
    tracing::info!(url = remote.url, ?project, agent, "memdusa MCP proxy on stdio");

    let server = Proxy { upstream, instructions, project }.serve(rmcp::transport::stdio()).await?;
    server.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::cli_auth::tests::{Home, login_and_tokens, spawn, test_config};

    #[tokio::test]
    async fn mcp_transport_refreshes_oauth_token() {
        let _home = Home::new();
        let mut cfg = test_config();
        cfg.access_secs = 1; // always "near expiry", so every request refreshes
        let (url, store) = spawn(cfg).await;
        login_and_tokens(&url, &store, "proxy-user").await;
        tokio::time::sleep(Duration::from_millis(1100)).await;

        let http = RefreshingHttp { inner: reqwest::Client::new(), url: url.clone() };
        let config = StreamableHttpClientTransportConfig::with_uri(format!("{url}/mcp"));
        let upstream = ().serve(StreamableHttpClientTransport::with_client(http, config)).await.unwrap();
        let tools = upstream.list_tools(None).await;
        assert!(tools.is_ok(), "MCP through a refreshing token should work: {tools:?}");
    }
}
