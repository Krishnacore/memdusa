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

use anyhow::Context;
use http::{HeaderName, HeaderValue};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, GetPromptRequestParams, GetPromptResponse,
    Implementation, ListPromptsResult, ListResourceTemplatesResult, ListResourcesResult, ListToolsResult,
    PaginatedRequestParams, ReadResourceRequestParams, ReadResourceResponse, ServerCapabilities, ServerConfig,
};
use rmcp::service::{RequestContext, RunningService, ServiceError};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{ErrorData as McpError, RoleClient, RoleServer, ServerHandler, ServiceExt};

use crate::config::Remote;
use crate::mcp::{AGENT_HEADER, PROJECT_HEADER};

struct Proxy {
    upstream: RunningService<RoleClient, ()>,
    instructions: Option<String>,
    /// Project detected from the directory this proxy runs in.
    project: Option<String>,
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
        Ok(match self.upstream.call_tool(request).await {
            Ok(result) => result.into(),
            // Surface transport failures to the agent as a readable tool error.
            Err(e) => {
                CallToolResult::error(vec![ContentBlock::text(format!("memdusa server unreachable: {e}"))]).into()
            }
        })
    }
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
    let upstream =
        ().serve(StreamableHttpClientTransport::from_config(config))
            .await
            .with_context(|| format!("connecting to {}/mcp", remote.url))?;
    let instructions = upstream.peer_info().and_then(|i| i.instructions.clone());
    tracing::info!(url = remote.url, ?project, agent, "memdusa MCP proxy on stdio");

    let server = Proxy { upstream, instructions, project }.serve(rmcp::transport::stdio()).await?;
    server.waiting().await?;
    Ok(())
}
