use anyhow::Result;
use log::info;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use super::client::RustAnalyzerClient;

const MAX_WORKSPACE_DIAGNOSTIC_FILES: usize = 128;
const SKIPPED_WORKSPACE_DIRS: [&str; 5] = [".git", "target", "node_modules", ".idea", ".vscode"];

impl RustAnalyzerClient {
    pub async fn hover(&mut self, uri: &str, line: u32, character: u32) -> Result<Value> {
        let params = json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character }
        });

        self.send_request("textDocument/hover", Some(params)).await
    }

    pub async fn definition(&mut self, uri: &str, line: u32, character: u32) -> Result<Value> {
        let params = json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character }
        });

        self.send_request("textDocument/definition", Some(params))
            .await
    }

    pub async fn references(&mut self, uri: &str, line: u32, character: u32) -> Result<Value> {
        let params = json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character },
            "context": { "includeDeclaration": true }
        });

        self.send_request("textDocument/references", Some(params))
            .await
    }

    pub async fn completion(&mut self, uri: &str, line: u32, character: u32) -> Result<Value> {
        let params = json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character }
        });

        self.send_request("textDocument/completion", Some(params))
            .await
    }

    pub async fn document_symbols(&mut self, uri: &str) -> Result<Value> {
        let params = json!({
            "textDocument": { "uri": uri }
        });

        self.send_request("textDocument/documentSymbol", Some(params))
            .await
    }

    pub async fn formatting(&mut self, uri: &str) -> Result<Value> {
        let params = json!({
            "textDocument": { "uri": uri },
            "options": {
                "tabSize": 4,
                "insertSpaces": true
            }
        });

        self.send_request("textDocument/formatting", Some(params))
            .await
    }

    pub async fn diagnostics(&mut self, uri: &str) -> Result<Value> {
        // First check if we have stored diagnostics from publishDiagnostics.
        let diag_lock = self.diagnostics.lock().await;
        info!("Looking for diagnostics for URI: {}", uri);
        info!(
            "Available URIs with diagnostics: {:?}",
            diag_lock.keys().collect::<Vec<_>>()
        );
        if let Some(diags) = diag_lock.get(uri) {
            info!("Found {} stored diagnostics for {}", diags.len(), uri);
            return Ok(json!(diags));
        }
        drop(diag_lock);

        info!("No stored diagnostics for {}, trying pull model", uri);
        // If no stored diagnostics, try the pull model as fallback.
        let params = json!({
            "textDocument": { "uri": uri }
        });

        let response = self
            .send_request("textDocument/diagnostic", Some(params))
            .await?;

        // Extract diagnostics from the response.
        if let Some(items) = response.get("items") {
            Ok(items.clone())
        } else {
            Ok(json!([]))
        }
    }

    pub async fn workspace_diagnostics(&mut self) -> Result<Value> {
        if self.workspace_diagnostics_supported {
            let params = json!({
                "identifier": "rust-analyzer",
                "previousResultId": null
            });

            match self.send_request("workspace/diagnostic", Some(params)).await {
                Ok(response) => {
                    if let Some(normalized) = normalize_workspace_diagnostic_report(&response) {
                        return Ok(normalized);
                    }

                    info!(
                        "workspace/diagnostic returned unsupported response; falling back. Response: {:?}",
                        response
                    );
                }
                Err(err) => {
                    info!("workspace/diagnostic request failed; falling back: {}", err);
                }
            }
        } else {
            info!("workspace/diagnostic not supported by server; using fallback");
        }

        self.workspace_diagnostics_fallback().await
    }

    async fn workspace_diagnostics_fallback(&mut self) -> Result<Value> {
        let stored = self.diagnostics.lock().await.clone();
        let mut all_diagnostics = diagnostics_map_to_value(&stored);

        // If nothing is known yet, open workspace files to trigger publishDiagnostics.
        if all_diagnostics.is_empty() {
            for file_path in collect_workspace_rust_files(&self.workspace_root) {
                let uri = uri_from_path(&file_path);
                if let Ok(content) = tokio::fs::read_to_string(&file_path).await {
                    let _ = self.open_document(&uri, &content).await;
                }
            }

            let stored = self.diagnostics.lock().await.clone();
            all_diagnostics = diagnostics_map_to_value(&stored);
        }

        Ok(Value::Object(all_diagnostics))
    }

    pub async fn code_actions(
        &mut self,
        uri: &str,
        start_line: u32,
        start_char: u32,
        end_line: u32,
        end_char: u32,
    ) -> Result<Value> {
        // First, try to get diagnostics for this range.
        let diagnostics = self.diagnostics(uri).await.unwrap_or(json!([]));

        // Filter diagnostics to only those in the requested range.
        let filtered_diagnostics = filter_diagnostics_in_range(&diagnostics, start_line, end_line);

        let params = json!({
            "textDocument": { "uri": uri },
            "range": {
                "start": { "line": start_line, "character": start_char },
                "end": { "line": end_line, "character": end_char }
            },
            "context": {
                "diagnostics": filtered_diagnostics,
                "only": ["quickfix", "refactor", "refactor.extract", "refactor.inline", "refactor.rewrite", "source"]
            }
        });

        self.send_request("textDocument/codeAction", Some(params))
            .await
    }
}

fn filter_diagnostics_in_range(diagnostics: &Value, start_line: u32, end_line: u32) -> Value {
    let Some(diag_array) = diagnostics.as_array() else {
        return json!([]);
    };

    let filtered: Vec<Value> = diag_array
        .iter()
        .filter(|d| {
            let Some(range) = d.get("range") else {
                return false;
            };
            let Some(start) = range.get("start") else {
                return false;
            };
            let Some(end) = range.get("end") else {
                return false;
            };

            let diag_start_line = start.get("line").and_then(|l| l.as_u64()).unwrap_or(0) as u32;
            let diag_end_line = end.get("line").and_then(|l| l.as_u64()).unwrap_or(0) as u32;

            // Check if diagnostic overlaps with requested range.
            diag_start_line <= end_line && diag_end_line >= start_line
        })
        .cloned()
        .collect();

    json!(filtered)
}

fn normalize_workspace_diagnostic_report(response: &Value) -> Option<Value> {
    if response.is_null() {
        return None;
    }

    if let Some(obj) = response.as_object() {
        // LSP pull-diagnostics shape: { "items": [ { "uri": "...", "items": [...] }, ... ] }
        if let Some(items) = obj.get("items").and_then(|value| value.as_array()) {
            let mut normalized = serde_json::Map::new();
            for item in items {
                let Some(uri) = item.get("uri").and_then(|value| value.as_str()) else {
                    continue;
                };

                let diagnostics = item
                    .get("items")
                    .or_else(|| item.get("diagnostics"))
                    .cloned()
                    .unwrap_or_else(|| json!([]));

                if diagnostics.is_array() {
                    normalized.insert(uri.to_string(), diagnostics);
                }
            }
            return Some(Value::Object(normalized));
        }

        // Already normalized map: { "file://...": [ ... ] }
        if obj.is_empty() || obj.values().all(Value::is_array) {
            return Some(response.clone());
        }
    }

    None
}

fn collect_workspace_rust_files(workspace_root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect_workspace_rust_files_recursive(workspace_root, &mut files);
    files.sort();
    files
}

fn collect_workspace_rust_files_recursive(dir: &Path, files: &mut Vec<PathBuf>) {
    if files.len() >= MAX_WORKSPACE_DIAGNOSTIC_FILES {
        return;
    }

    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();

        if path.is_dir() {
            if should_skip_workspace_dir(&path) {
                continue;
            }
            collect_workspace_rust_files_recursive(&path, files);
            if files.len() >= MAX_WORKSPACE_DIAGNOSTIC_FILES {
                return;
            }
            continue;
        }

        let is_rust_file = path.extension().and_then(|ext| ext.to_str()) == Some("rs");
        if is_rust_file {
            files.push(path);
            if files.len() >= MAX_WORKSPACE_DIAGNOSTIC_FILES {
                return;
            }
        }
    }
}

fn should_skip_workspace_dir(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };

    SKIPPED_WORKSPACE_DIRS.contains(&name)
}

fn uri_from_path(path: &Path) -> String {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    format!("file://{}", canonical.display())
}

fn diagnostics_map_to_value(
    diagnostics: &std::collections::HashMap<String, Vec<Value>>,
) -> serde_json::Map<String, Value> {
    diagnostics
        .iter()
        .map(|(uri, items)| (uri.clone(), json!(items)))
        .collect()
}
