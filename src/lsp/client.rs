use anyhow::{anyhow, Result};
use log::info;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::PathBuf,
    process::Stdio,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncWriteExt, BufWriter},
    process::{Child, Command},
    sync::{oneshot, Mutex},
};

use crate::{
    config::{DOCUMENT_OPEN_DELAY_MILLIS, LSP_REQUEST_TIMEOUT_SECS},
    protocol::lsp::LSPRequest,
};

#[derive(Debug, Clone)]
pub(super) struct OpenDocumentState {
    version: i32,
    content: String,
}

pub struct RustAnalyzerClient {
    pub(super) process: Option<Child>,
    pub(super) request_id: Arc<Mutex<u64>>,
    pub(super) workspace_root: PathBuf,
    pub(super) stdin: Option<BufWriter<tokio::process::ChildStdin>>,
    pub(super) pending_requests: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    pub(super) initialized: bool,
    pub(super) workspace_diagnostics_supported: bool,
    pub(super) open_documents: Arc<Mutex<HashMap<String, OpenDocumentState>>>,
    pub(super) diagnostics: Arc<Mutex<HashMap<String, Vec<Value>>>>,
}

impl RustAnalyzerClient {
    pub fn new(workspace_root: PathBuf) -> Self {
        // Ensure the workspace root is absolute.
        let workspace_root = workspace_root.canonicalize().unwrap_or_else(|_| {
            if workspace_root.is_absolute() {
                workspace_root.clone()
            } else {
                std::env::current_dir()
                    .unwrap_or_else(|_| PathBuf::from("."))
                    .join(&workspace_root)
            }
        });

        Self {
            process: None,
            request_id: Arc::new(Mutex::new(1)),
            workspace_root,
            stdin: None,
            pending_requests: Arc::new(Mutex::new(HashMap::new())),
            initialized: false,
            workspace_diagnostics_supported: false,
            open_documents: Arc::new(Mutex::new(HashMap::new())),
            diagnostics: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn start(&mut self) -> Result<()> {
        info!(
            "Starting rust-analyzer process in workspace: {}",
            self.workspace_root.display()
        );

        // Clear any existing diagnostics from previous sessions.
        self.diagnostics.lock().await.clear();

        // Find rust-analyzer executable.
        let rust_analyzer_path = find_rust_analyzer()?;
        info!("Using rust-analyzer at: {}", rust_analyzer_path.display());

        let mut cmd = Command::new(rust_analyzer_path);
        cmd.current_dir(&self.workspace_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // Pass through isolation environment variables if they're set.
        if let Ok(cache_home) = std::env::var("XDG_CACHE_HOME") {
            cmd.env("XDG_CACHE_HOME", cache_home);
        }
        if let Ok(target_dir) = std::env::var("CARGO_TARGET_DIR") {
            cmd.env("CARGO_TARGET_DIR", target_dir);
        }
        if let Ok(tmpdir) = std::env::var("TMPDIR") {
            cmd.env("TMPDIR", tmpdir);
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| anyhow!("Failed to start rust-analyzer: {}", e))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("Failed to get stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("Failed to get stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("Failed to get stderr"))?;

        self.stdin = Some(BufWriter::new(stdin));

        // Start connection handlers.
        super::connection::start_handlers(
            stdout,
            stderr,
            Arc::clone(&self.pending_requests),
            Arc::clone(&self.diagnostics),
        );

        self.process = Some(child);

        // Initialize LSP.
        self.initialize().await?;
        self.initialized = true;

        // Send workspace/didChangeConfiguration to ensure settings are applied.
        let config_params = json!({
            "settings": {
                "rust-analyzer": {
                    "checkOnSave": {
                        "enable": true,
                        "command": "check",
                        "allTargets": true
                    }
                }
            }
        });
        let _ = self
            .send_notification("workspace/didChangeConfiguration", Some(config_params))
            .await;

        info!("rust-analyzer client started and initialized");
        Ok(())
    }

    pub(super) async fn send_notification(
        &mut self,
        method: &str,
        params: Option<Value>,
    ) -> Result<()> {
        let notification = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params.unwrap_or(json!({}))
        });

        let content = serde_json::to_string(&notification)?;
        let message = format!("Content-Length: {}\r\n\r\n{}", content.len(), content);

        info!("Sending LSP notification: {}", method);

        let Some(stdin) = &mut self.stdin else {
            return Err(anyhow!("No stdin available"));
        };

        stdin.write_all(message.as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }

    pub(super) async fn send_request(
        &mut self,
        method: &str,
        params: Option<Value>,
    ) -> Result<Value> {
        let mut request_id_lock = self.request_id.lock().await;
        let id = *request_id_lock;
        *request_id_lock += 1;
        drop(request_id_lock);

        let request = LSPRequest {
            jsonrpc: "2.0".to_string(),
            id,
            method: method.to_string(),
            params: params.clone(),
        };

        let content = serde_json::to_string(&request)?;
        let message = format!("Content-Length: {}\r\n\r\n{}", content.len(), content);

        info!("Sending LSP request: {} with params: {:?}", method, params);

        let Some(stdin) = &mut self.stdin else {
            return Err(anyhow!("No stdin available"));
        };

        stdin.write_all(message.as_bytes()).await?;
        stdin.flush().await?;

        // Set up response channel.
        let (tx, rx) = oneshot::channel();
        self.pending_requests.lock().await.insert(id, tx);

        // Wait for response with timeout.
        tokio::time::timeout(Duration::from_secs(LSP_REQUEST_TIMEOUT_SECS), rx)
            .await
            .map_err(|_| anyhow!("Request timeout"))?
            .map_err(|_| anyhow!("Request cancelled"))
    }

    async fn initialize(&mut self) -> Result<()> {
        let init_params = json!({
            "processId": std::process::id(),
            "rootUri": format!("file://{}", self.workspace_root.display()),
            "initializationOptions": {
                "cargo": {
                    "buildScripts": {
                        "enable": true
                    }
                },
                "checkOnSave": {
                    "enable": true,
                    "command": "check",
                    "allTargets": true
                },
                "diagnostics": {
                    "enable": true,
                    "experimental": {
                        "enable": true
                    }
                },
                "procMacro": {
                    "enable": true
                }
            },
            "capabilities": {
                "textDocument": {
                    "hover": {
                        "contentFormat": ["markdown", "plaintext"]
                    },
                    "completion": {
                        "completionItem": {
                            "snippetSupport": true
                        }
                    },
                    "definition": {
                        "linkSupport": true
                    },
                    "references": {},
                    "documentSymbol": {},
                    "codeAction": {
                        "codeActionLiteralSupport": {
                            "codeActionKind": {
                                "valueSet": [
                                    "quickfix",
                                    "refactor",
                                    "refactor.extract",
                                    "refactor.inline",
                                    "refactor.rewrite",
                                    "source",
                                    "source.organizeImports"
                                ]
                            }
                        },
                        "resolveSupport": {
                            "properties": ["edit"]
                        }
                    },
                    "publishDiagnostics": {
                        "relatedInformation": true,
                        "tagSupport": {
                            "valueSet": [1, 2]
                        }
                    },
                    "formatting": {}
                },
                "workspace": {
                    "didChangeConfiguration": {
                        "dynamicRegistration": false
                    }
                }
            }
        });

        let init_response = self.send_request("initialize", Some(init_params)).await?;
        self.workspace_diagnostics_supported = init_response
            .get("capabilities")
            .and_then(|caps| caps.get("diagnosticProvider"))
            .and_then(|provider| provider.get("workspaceDiagnostics"))
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        info!(
            "workspace/diagnostic support: {}",
            self.workspace_diagnostics_supported
        );
        self.send_notification("initialized", Some(json!({})))
            .await?;

        // Request workspace reload to trigger cargo check.
        self.send_request("rust-analyzer/reloadWorkspace", None)
            .await
            .ok();

        Ok(())
    }

    pub async fn open_document(&mut self, uri: &str, content: &str) -> Result<()> {
        enum DocumentSyncAction {
            NoChange,
            Open { version: i32 },
            Change { version: i32 },
        }

        let action = {
            let mut open_docs = self.open_documents.lock().await;
            match open_docs.get_mut(uri) {
                Some(state) if state.content == content => {
                    info!("Document already open and up to date: {}", uri);
                    DocumentSyncAction::NoChange
                }
                Some(state) => {
                    state.version += 1;
                    state.content = content.to_string();
                    DocumentSyncAction::Change {
                        version: state.version,
                    }
                }
                None => {
                    open_docs.insert(
                        uri.to_string(),
                        OpenDocumentState {
                            version: 1,
                            content: content.to_string(),
                        },
                    );
                    DocumentSyncAction::Open { version: 1 }
                }
            }
        };

        if matches!(action, DocumentSyncAction::NoChange) {
            return Ok(());
        }

        // Clear existing diagnostics for this URI so callers don't see stale entries
        // while waiting for fresh publishDiagnostics updates.
        self.diagnostics.lock().await.remove(uri);

        match action {
            DocumentSyncAction::NoChange => {}
            DocumentSyncAction::Open { version } => {
                info!("Opening document: {}", uri);
                let params = json!({
                    "textDocument": {
                        "uri": uri,
                        "languageId": "rust",
                        "version": version,
                        "text": content
                    }
                });
                self.send_notification("textDocument/didOpen", Some(params))
                    .await?;
            }
            DocumentSyncAction::Change { version } => {
                info!("Document changed, sending didChange: {}", uri);
                let params = json!({
                    "textDocument": {
                        "uri": uri,
                        "version": version
                    },
                    "contentChanges": [
                        {
                            "text": content
                        }
                    ]
                });
                self.send_notification("textDocument/didChange", Some(params))
                    .await?;
            }
        }

        // Send didSave to trigger checkOnSave diagnostics refresh.
        let save_params = json!({
            "textDocument": {
                "uri": uri
            }
        });
        self.send_notification("textDocument/didSave", Some(save_params))
            .await?;

        // Give rust-analyzer time to process the document and run cargo check.
        tokio::time::sleep(Duration::from_millis(DOCUMENT_OPEN_DELAY_MILLIS)).await;

        Ok(())
    }

    pub async fn shutdown(&mut self) -> Result<()> {
        if self.initialized {
            let _ = self.send_request("shutdown", None).await;
            let _ = self.send_notification("exit", None).await;
        }

        if let Some(mut process) = self.process.take() {
            // Kill the process and wait for it to actually exit.
            let _ = process.kill().await;
            let _ = process.wait().await;
        }

        // Clear open documents and diagnostics.
        self.open_documents.lock().await.clear();
        self.diagnostics.lock().await.clear();
        self.initialized = false;
        self.workspace_diagnostics_supported = false;
        Ok(())
    }
}

fn find_rust_analyzer() -> Result<PathBuf> {
    which::which("rust-analyzer").or_else(|_| {
        // Try common installation locations if not in PATH.
        let home = std::env::var("HOME").unwrap_or_else(|_| String::from("~"));
        let cargo_bin = PathBuf::from(home).join(".cargo/bin/rust-analyzer");
        if cargo_bin.exists() {
            Ok(cargo_bin)
        } else {
            which::which("rust-analyzer")
        }
    })
    .map_err(|e| {
        anyhow!(
            "Failed to find rust-analyzer in PATH or ~/.cargo/bin: {}. Please ensure rust-analyzer is installed.",
            e
        )
    })
}
