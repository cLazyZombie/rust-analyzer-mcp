use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::{path::Path, time::Duration};
use test_support::{IsolatedProject, MCPTestClient};
use tokio::time::{sleep, Instant};

async fn diagnostics_for_file(client: &MCPTestClient, file_path: &Path) -> Result<Value> {
    let response = client
        .call_tool(
            "rust_analyzer_diagnostics",
            json!({
                "file_path": file_path.to_str().ok_or_else(|| anyhow!("Invalid file path"))?
            }),
        )
        .await?;

    let text = response
        .get("content")
        .and_then(|c| c.get(0))
        .and_then(|item| item.get("text"))
        .and_then(|t| t.as_str())
        .ok_or_else(|| anyhow!("Missing diagnostics text in tool response"))?;

    serde_json::from_str(text).map_err(Into::into)
}

fn has_error_code(diagnostics: &Value, code: &str) -> bool {
    diagnostics
        .get("diagnostics")
        .and_then(|d| d.as_array())
        .map(|items| {
            items.iter().any(|item| {
                item.get("code")
                    .and_then(|c| c.as_str())
                    .map(|c| c == code)
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

async fn wait_for_error_code(
    client: &MCPTestClient,
    file_path: &Path,
    code: &str,
    should_exist: bool,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        let diagnostics = diagnostics_for_file(client, file_path).await?;
        let has_code = has_error_code(&diagnostics, code);
        if has_code == should_exist {
            return Ok(());
        }
        sleep(Duration::from_millis(400)).await;
    }

    Err(anyhow!(
        "Timeout waiting for diagnostics code {} to {}",
        code,
        if should_exist { "appear" } else { "disappear" }
    ))
}

#[tokio::test]
async fn test_diagnostics_refresh_without_workspace_restart() -> Result<()> {
    let isolated = IsolatedProject::new()?;
    let workspace = isolated.path().to_path_buf();
    let target_file = workspace.join("src/main.rs");
    let original = tokio::fs::read_to_string(&target_file).await?;

    let client = MCPTestClient::start(&workspace).await?;
    client.initialize_and_wait().await?;

    // Prime rust-analyzer state for this file.
    let _ = diagnostics_for_file(&client, &target_file).await?;

    // Introduce a deterministic type mismatch without touching workspace settings.
    let broken = format!(
        "{}\nconst _DIAGNOSTIC_REFRESH_PROBE: u64 = \"probe\";\n",
        original
    );
    tokio::fs::write(&target_file, broken).await?;

    // The new error should be observed without calling rust_analyzer_set_workspace.
    wait_for_error_code(&client, &target_file, "E0308", true).await?;

    // Revert and verify diagnostics clear without workspace restart as well.
    tokio::fs::write(&target_file, &original).await?;
    wait_for_error_code(&client, &target_file, "E0308", false).await?;

    client.shutdown().await?;
    Ok(())
}
