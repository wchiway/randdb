use std::{path::PathBuf, sync::Arc};

use anyhow::Result;
use rmcp::{
    RoleServer, ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolResult, ContentBlock as Content, ErrorData, Implementation, ServerCapabilities,
        ServerConfig,
    },
    service::RequestContext,
    tool, tool_handler, tool_router,
};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::engine::SearchRequest;

#[derive(Clone)]
pub struct RandDbServer {
    home: PathBuf,
    tool_router: ToolRouter<Self>,
    // One active indexing/retrieval pipeline per MCP process bounds aggregate memory.
    gate: Arc<Semaphore>,
    shutdown: CancellationToken,
}

impl RandDbServer {
    pub fn new(home: PathBuf, shutdown: CancellationToken) -> Self {
        Self {
            home,
            tool_router: Self::tool_router(),
            gate: Arc::new(Semaphore::new(1)),
            shutdown,
        }
    }
}

#[tool_router]
impl RandDbServer {
    #[tool(
        name = "codebase-retrieval",
        description = "Find code relevant to a natural-language question. Automatically updates a local repository index and returns file paths, line numbers, and bounded code snippets. Optional technical_terms strengthen lexical matching. Use your file-reading or text-search tools for exhaustive exploration."
    )]
    async fn retrieve(
        &self,
        Parameters(request): Parameters<SearchRequest>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        request
            .validate()
            .map_err(|e| ErrorData::invalid_params(e.to_string(), None))?;
        let permit = tokio::select! {
            _ = context.ct.cancelled() => return Ok(CallToolResult::error(vec![Content::text("Request cancelled")])),
            _ = self.shutdown.cancelled() => return Ok(CallToolResult::error(vec![Content::text("Server shutting down")])),
            permit = self.gate.clone().acquire_owned() => permit.map_err(|_| ErrorData::internal_error("Server shutting down",None))?,
        };
        let cancel = context.ct.child_token();
        let _cancel_on_drop = cancel.clone().drop_guard();
        let home = self.home.clone();
        let worker_cancel = cancel.clone();
        let mut task = tokio::spawn(async move {
            let _permit = permit;
            crate::search(home, request, worker_cancel).await
        });
        let result = tokio::select! {
            result=&mut task => result,
            _ = self.shutdown.cancelled() => { cancel.cancel(); task.await },
        };
        match result {
            Ok(Ok(output)) => Ok(CallToolResult::success(vec![Content::text(output)])),
            Ok(Err(error)) => Ok(CallToolResult::error(vec![Content::text(format!(
                "RandDB: {error:#}"
            ))])),
            Err(_) => Err(ErrorData::internal_error("RandDB worker failed", None)),
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for RandDbServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_server_info(
            Implementation::new("randdb", env!("CARGO_PKG_VERSION")).with_title("RandDB"),
        )
    }
}

pub async fn serve(home: PathBuf, shutdown: CancellationToken) -> Result<()> {
    let service = RandDbServer::new(home, shutdown.clone())
        .serve(rmcp::transport::stdio())
        .await?;
    let token = service.cancellation_token();
    tokio::spawn(async move {
        shutdown.cancelled().await;
        token.cancel();
    });
    service.waiting().await?;
    Ok(())
}
