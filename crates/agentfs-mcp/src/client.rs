use agentfs_model::{Error, ErrorCode, Result};
use rmcp::{
    ErrorData, RoleClient, RoleServer, ServerHandler, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, Implementation, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, ServerConfig,
    },
    service::{RequestContext, RunningService},
    transport::{
        StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use serde_json::Value;
use std::sync::Arc;

pub struct Client {
    service: RunningService<RoleClient, ()>,
}
impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client").finish_non_exhaustive()
    }
}
impl Client {
    pub async fn connect(endpoint: &str, token: String) -> Result<Arc<Self>> {
        let config = StreamableHttpClientTransportConfig::with_uri(endpoint).auth_header(token);
        let transport = StreamableHttpClientTransport::from_config(config);
        let service = ().serve(transport).await.map_err(remote_error)?;
        Ok(Arc::new(Self { service }))
    }
    pub async fn close(self: Arc<Self>) -> Result<()> {
        let mut client = Arc::try_unwrap(self)
            .map_err(|_| Error::new(ErrorCode::Busy, "client still has active owners"))?;
        client
            .service
            .close_with_timeout(std::time::Duration::from_secs(5))
            .await
            .map_err(remote_error)?;
        Ok(())
    }
    pub async fn tools(&self) -> Result<ListToolsResult> {
        self.service.list_tools(None).await.map_err(remote_error)
    }
    pub async fn call(&self, name: String, arguments: Value) -> Result<CallToolResult> {
        let arguments = arguments
            .as_object()
            .cloned()
            .ok_or_else(|| Error::invalid("tool arguments must be an object"))?;
        self.service
            .call_tool(CallToolRequestParams::new(name).with_arguments(arguments))
            .await
            .map_err(remote_error)
    }
}
fn remote_error(error: impl std::fmt::Display) -> Error {
    Error::new(ErrorCode::Unavailable, error.to_string()).retryable()
}

#[derive(Debug)]
struct Proxy {
    client: Arc<Client>,
}
impl ServerHandler for Proxy {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("agentfs", env!("CARGO_PKG_VERSION")))
    }
    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> std::result::Result<ListToolsResult, ErrorData> {
        self.client
            .tools()
            .await
            .map_err(|error| ErrorData::internal_error(error.to_string(), None))
    }
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResponse, ErrorData> {
        self.client
            .call(
                request.name.to_string(),
                Value::Object(request.arguments.unwrap_or_default()),
            )
            .await
            .map(Into::into)
            .map_err(|error| ErrorData::internal_error(error.to_string(), None))
    }
}
pub async fn stdio_proxy(client: Arc<Client>) -> Result<()> {
    let service = Proxy { client }
        .serve(rmcp::transport::stdio())
        .await
        .map_err(remote_error)?;
    service.waiting().await.map_err(remote_error)?;
    Ok(())
}
