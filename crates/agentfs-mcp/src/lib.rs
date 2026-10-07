//! Authenticated MCP transport for the shared AgentFS service facade.
mod client;
mod gateway;
mod server;
pub use client::{Client, stdio_proxy};
pub use gateway::{Credential, GatewayConfig, router};
pub use server::{McpServer, tool_catalog};
