# AGENTS.md

This file is for coding agents working in `rust-analyzer-mcp`.

## Project Summary

- Crate: `rust-analyzer-mcp` (`src/main.rs` binary + `src/lib.rs` library).
- Purpose: expose rust-analyzer capabilities as MCP tools over stdio.
- Runtime: Tokio async, rust-analyzer subprocess, JSON-RPC/MCP + LSP framing.

## Architecture Map

- Entry point:
  - `src/main.rs`: parses optional workspace arg, starts `RustAnalyzerMCPServer`.
- MCP server layer:
  - `src/mcp/server.rs`: request loop, MCP routing (`initialize`, `ping`, `tools/list`, `tools/call`), client lifecycle.
  - `src/mcp/tools.rs`: MCP tool definitions + JSON schemas.
  - `src/mcp/handlers.rs`: maps tool calls to rust-analyzer client methods.
  - `src/mcp/transport.rs`: stdio framing parser/writer. Supports both NDJSON and `Content-Length`.
- LSP client layer:
  - `src/lsp/client.rs`: spawn rust-analyzer process, initialize LSP session, send requests/notifications, manage open docs.
  - `src/lsp/connection.rs`: background stdout/stderr handlers; routes responses; stores `publishDiagnostics`.
  - `src/lsp/handlers.rs`: high-level methods (`hover`, `definition`, `references`, `completion`, `symbols`, `format`, diagnostics, code actions).
- Protocol + formatting:
  - `src/protocol/mcp.rs`: MCP request/response/tool types.
  - `src/protocol/lsp.rs`: LSP request/response envelope types.
  - `src/diagnostics/mod.rs`: normalized diagnostic output format.
  - `src/config.rs`: request timeout + document-open delay constants.

## Request Flow (Critical Path)

1. MCP message is read in `StdioTransport`.
2. `RustAnalyzerMCPServer::run_with_streams` parses into `MCPRequest`.
3. If `id` is absent, message is treated as notification (no response).
4. For `tools/call`, `mcp::handlers::handle_tool_call` dispatches.
5. Handler ensures rust-analyzer client is started and document is opened as needed.
6. `RustAnalyzerClient` sends LSP request; `lsp::connection` matches response by request id.
7. Tool result is wrapped as MCP `content: [{type: "text", text: "...json..."}]`.

## MCP Tools Currently Exposed

- `rust_analyzer_hover`
- `rust_analyzer_definition`
- `rust_analyzer_references`
- `rust_analyzer_completion`
- `rust_analyzer_symbols`
- `rust_analyzer_format`
- `rust_analyzer_code_actions`
- `rust_analyzer_set_workspace`
- `rust_analyzer_diagnostics`
- `rust_analyzer_workspace_diagnostics`

When changing tools, keep these in sync:

- `src/mcp/tools.rs` (schema surface)
- `src/mcp/handlers.rs` (execution surface)
- `tests/unit/protocol/tool_tests.rs` (schema/format expectations)

## Testing Layout

- Integration: `tests/integration/`
  - End-to-end MCP + rust-analyzer behavior.
  - Includes diagnostics behavior tests (`tests/integration/diagnostics.rs`).
- Stress: `tests/stress/concurrent_requests.rs`
  - Ping throughput, mixed workloads, concurrent tool calls.
- Unit: `tests/unit/protocol/`
  - MCP request/response/tool serialization and invariants.
- Property: `tests/property/protocol_fuzzing.rs`
  - Fuzz-like protocol/property tests.
- Test support crate: `test-support/`
  - `IpcClient`, isolated project setup, CI-sensitive timeouts/readiness checks.

Test workspaces used by tests:

- `test-project/`
- `test-project-diagnostics/`

## Build / Test Commands

- Build: `cargo build`
- Run server: `cargo run -- /path/to/workspace`
- Test all: `cargo test`
- Format: `cargo +nightly fmt --all`
- Lint: `cargo clippy -- -D warnings`

## Rust-Analyzer MCP Notes

- Workspace roots are canonicalized in both server and client setup.
- Documents must be opened (`didOpen` + `didSave`) before many LSP features are reliable.
- Diagnostics are asynchronous; polling/waiting is required for stable results.
- `workspace/diagnostic` response shape can vary; fallback formatting is implemented in `format_workspace_diagnostics`.
- Transport intentionally supports both newline-delimited JSON and `Content-Length` frames.

## Change Guidance

- For new MCP method/tool:
  - Add schema in `src/mcp/tools.rs`.
  - Add dispatcher branch + handler in `src/mcp/handlers.rs`.
  - Add client API in `src/lsp/handlers.rs` or `src/lsp/client.rs` as needed.
  - Add integration + unit tests.
- For protocol/framing changes:
  - Update `src/mcp/transport.rs` and extend its unit tests.
- For diagnostics behavior:
  - Update both `src/lsp/connection.rs` storage logic and `src/diagnostics/mod.rs` formatting logic.

## Practical Guardrails

- Preserve notification semantics: requests without `id` must not emit responses.
- Keep line/character indexing 0-based in tool inputs.
- Avoid breaking tool output shape (`content[].text` currently contains JSON string).
- Be careful with timeouts and sleeps: they are part of reliability contracts in CI.
