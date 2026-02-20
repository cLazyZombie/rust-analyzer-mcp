use anyhow::Result;
use log::{debug, error, info};
use serde_json::json;
use std::{path::PathBuf, sync::Arc};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::Mutex,
};

use crate::{
    lsp::RustAnalyzerClient,
    protocol::mcp::{MCPError, MCPRequest, MCPResponse},
};

pub struct RustAnalyzerMCPServer {
    pub(super) client: Option<RustAnalyzerClient>,
    pub(super) workspace_root: PathBuf,
}

impl Default for RustAnalyzerMCPServer {
    fn default() -> Self {
        Self::new()
    }
}

impl RustAnalyzerMCPServer {
    pub fn new() -> Self {
        Self {
            client: None,
            workspace_root: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        }
    }

    pub fn with_workspace(workspace_root: PathBuf) -> Self {
        // Ensure the workspace root is absolute.
        let workspace_root = workspace_root.canonicalize().unwrap_or_else(|_| {
            // If canonicalize fails, try to make it absolute.
            if workspace_root.is_absolute() {
                workspace_root.clone()
            } else {
                std::env::current_dir()
                    .unwrap_or_else(|_| PathBuf::from("."))
                    .join(&workspace_root)
            }
        });

        Self {
            client: None,
            workspace_root,
        }
    }

    pub(super) async fn ensure_client_started(&mut self) -> Result<()> {
        if self.client.is_none() {
            let mut client = RustAnalyzerClient::new(self.workspace_root.clone());
            client.start().await?;
            self.client = Some(client);
        }
        Ok(())
    }

    pub(super) async fn open_document_if_needed(&mut self, file_path: &str) -> Result<String> {
        let absolute_path = self.workspace_root.join(file_path);
        // Ensure we have an absolute path for the URI.
        let absolute_path = absolute_path
            .canonicalize()
            .unwrap_or_else(|_| absolute_path.clone());
        let uri = format!("file://{}", absolute_path.display());
        let content = tokio::fs::read_to_string(&absolute_path)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to read file {}: {}", file_path, e))?;

        let Some(client) = &mut self.client else {
            return Err(anyhow::anyhow!("Client not initialized"));
        };

        client.open_document(&uri, &content).await?;
        Ok(uri)
    }

    pub async fn run(&mut self) -> Result<()> {
        let stdin = tokio::io::stdin();
        let stdout = tokio::io::stdout();
        self.run_with_streams(stdin, stdout).await
    }

    pub async fn run_with_streams<R, W>(&mut self, reader: R, writer: W) -> Result<()>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        info!("Starting rust-analyzer MCP server");

        let mut transport = super::transport::StdioTransport::new(reader, writer);

        // Handle shutdown signals.
        let running = Arc::new(Mutex::new(true));
        let running_clone = Arc::clone(&running);

        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            info!("Received shutdown signal");
            *running_clone.lock().await = false;
        });

        loop {
            // Check if we should stop.
            if !*running.lock().await {
                break;
            }

            let Some((request_text, framing)) = (match transport.read_message().await {
                Ok(message) => message,
                Err(e) => {
                    error!("Error reading MCP message: {e}");
                    break;
                }
            }) else {
                break;
            };

            let request_text = request_text.trim();
            if request_text.is_empty() {
                continue;
            }

            let Ok(request) = serde_json::from_str::<MCPRequest>(request_text) else {
                debug!("Failed to parse request: {request_text}");
                continue;
            };

            debug!("Received request: {}", request.method);
            log::debug!("{request:#?}");

            // requests without an id are notifications and must not receive a response!
            if request.id.is_some() {
                let response = self.handle_request(request).await;
                let response_json = serde_json::to_string(&response)?;
                if let Err(err) = transport.write_message(&response_json, framing).await {
                    error!("Error writing MCP response: {err}");
                    break;
                }
            }
        }

        // Cleanup.
        info!("Shutting down");
        if let Some(client) = &mut self.client {
            let _ = client.shutdown().await;
        }

        Ok(())
    }

    async fn handle_request(&mut self, request: MCPRequest) -> MCPResponse {
        log::debug!("{request:#?}");
        match request.method.as_str() {
            "initialize" => {
                let protocol_version = request
                    .params
                    .as_ref()
                    .and_then(|params| params.get("protocolVersion"))
                    .and_then(|version| version.as_str())
                    .unwrap_or("2024-11-05");

                MCPResponse::Success {
                    jsonrpc: "2.0".to_string(),
                    id: request.id,
                    result: json!({
                        "protocolVersion": protocol_version,
                        "serverInfo": {
                            "name": "rust-analyzer-mcp",
                            "version": env!("CARGO_PKG_VERSION")
                        },
                        "capabilities": {
                            "tools": {}
                        }
                    }),
                }
            }
            "ping" => MCPResponse::Success {
                jsonrpc: "2.0".to_string(),
                id: request.id,
                result: json!({}),
            },
            "tools/list" => MCPResponse::Success {
                jsonrpc: "2.0".to_string(),
                id: request.id,
                result: json!({
                    "tools": super::tools::get_tools()
                }),
            },
            "tools/call" => {
                let Some(params) = request.params else {
                    return MCPResponse::Error {
                        jsonrpc: "2.0".to_string(),
                        id: request.id,
                        error: MCPError {
                            code: -32602,
                            message: "Invalid params".to_string(),
                            data: None,
                        },
                    };
                };

                let Some(tool_name) = params["name"].as_str() else {
                    return MCPResponse::Error {
                        jsonrpc: "2.0".to_string(),
                        id: request.id,
                        error: MCPError {
                            code: -32602,
                            message: "Missing tool name".to_string(),
                            data: None,
                        },
                    };
                };

                let args = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));

                match super::handlers::handle_tool_call(self, tool_name, args).await {
                    Ok(result) => MCPResponse::Success {
                        jsonrpc: "2.0".to_string(),
                        id: request.id,
                        result: serde_json::to_value(result).unwrap(),
                    },
                    Err(e) => {
                        error!("Tool call error: {}", e);
                        MCPResponse::Error {
                            jsonrpc: "2.0".to_string(),
                            id: request.id,
                            error: MCPError {
                                code: -1,
                                message: e.to_string(),
                                data: None,
                            },
                        }
                    }
                }
            }
            _ => MCPResponse::Error {
                jsonrpc: "2.0".to_string(),
                id: request.id,
                error: MCPError {
                    code: -32601,
                    message: format!("Method not found: {}", request.method),
                    data: None,
                },
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use anyhow::{anyhow, Result};
    use serde_json::{json, Value};
    use std::time::Duration;
    use tokio::io::{duplex, split, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
    use tokio::time::timeout;

    use super::RustAnalyzerMCPServer;

    #[tokio::test]
    async fn test_content_length_requests_are_handled_without_eof() -> Result<()> {
        let (client_io, server_io) = duplex(16 * 1024);
        let (server_reader, server_writer) = split(server_io);
        let mut server = RustAnalyzerMCPServer::new();

        let server_task =
            tokio::spawn(
                async move { server.run_with_streams(server_reader, server_writer).await },
            );

        let (mut client_reader, mut client_writer) = split(client_io);

        let initialize = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "test", "version": "1.0.0" }
            }
        });
        write_content_length_message(&mut client_writer, &initialize.to_string()).await?;

        let init_response = timeout(
            Duration::from_secs(1),
            read_content_length_message(&mut client_reader),
        )
        .await??;
        let init_response: Value = serde_json::from_str(&init_response)?;
        assert_eq!(init_response["id"], 1);
        assert_eq!(
            init_response["result"]["serverInfo"]["name"],
            "rust-analyzer-mcp"
        );

        let tools_list = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {}
        });
        write_content_length_message(&mut client_writer, &tools_list.to_string()).await?;

        let tools_response = timeout(
            Duration::from_secs(1),
            read_content_length_message(&mut client_reader),
        )
        .await??;
        let tools_response: Value = serde_json::from_str(&tools_response)?;
        assert_eq!(tools_response["id"], 2);
        assert!(tools_response["result"]["tools"].is_array());

        client_writer.shutdown().await?;
        drop(client_writer);
        drop(client_reader);
        server_task.await??;

        Ok(())
    }

    #[tokio::test]
    async fn test_notification_does_not_generate_response() -> Result<()> {
        let (client_io, server_io) = duplex(16 * 1024);
        let (server_reader, server_writer) = split(server_io);
        let mut server = RustAnalyzerMCPServer::new();

        let server_task =
            tokio::spawn(
                async move { server.run_with_streams(server_reader, server_writer).await },
            );

        let (mut client_reader, mut client_writer) = split(client_io);

        let notification = json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {}
        });
        write_content_length_message(&mut client_writer, &notification.to_string()).await?;

        let notification_result = timeout(Duration::from_millis(200), async {
            let mut byte = [0u8; 1];
            client_reader.read_exact(&mut byte).await
        })
        .await;
        assert!(
            notification_result.is_err(),
            "Notification should not emit any response bytes"
        );

        let tools_list = json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/list",
            "params": {}
        });
        write_content_length_message(&mut client_writer, &tools_list.to_string()).await?;

        let tools_response = timeout(
            Duration::from_secs(1),
            read_content_length_message(&mut client_reader),
        )
        .await??;
        let tools_response: Value = serde_json::from_str(&tools_response)?;
        assert_eq!(tools_response["id"], 7);
        assert!(tools_response["result"]["tools"].is_array());

        client_writer.shutdown().await?;
        drop(client_writer);
        drop(client_reader);
        server_task.await??;

        Ok(())
    }

    async fn write_content_length_message<W>(writer: &mut W, body: &str) -> Result<()>
    where
        W: AsyncWrite + Unpin,
    {
        let frame = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
        writer.write_all(frame.as_bytes()).await?;
        writer.flush().await?;
        Ok(())
    }

    async fn read_content_length_message<R>(reader: &mut R) -> Result<String>
    where
        R: AsyncRead + Unpin,
    {
        let mut header = Vec::new();
        loop {
            let mut byte = [0u8; 1];
            reader.read_exact(&mut byte).await?;
            header.push(byte[0]);

            if header.ends_with(b"\r\n\r\n") {
                break;
            }

            if header.len() > 4096 {
                return Err(anyhow!("MCP header too large"));
            }
        }

        let header_text = String::from_utf8(header)?;
        let content_length = header_text
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                if name.eq_ignore_ascii_case("Content-Length") {
                    value.trim().parse::<usize>().ok()
                } else {
                    None
                }
            })
            .ok_or_else(|| anyhow!("Missing Content-Length header"))?;

        let mut body = vec![0u8; content_length];
        reader.read_exact(&mut body).await?;
        Ok(String::from_utf8(body)?)
    }
}
