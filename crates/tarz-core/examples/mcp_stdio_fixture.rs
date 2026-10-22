//! Local MCP server used by Tarz's own tests; not a user-facing example.
//!
//! `cargo test` builds it and the tests start it as a stdio MCP server. The
//! role comes from `TARZ_MCP_FIXTURE_ROLE`:
//!
//! - `serve` answers MCP on stdin and stdout with the tools listed below;
//! - `fail` writes a reason to stderr and exits before the handshake;
//! - `slow` never completes the handshake.
//!
//! When `TARZ_MCP_FIXTURE_RELIST` is `hang`, every tool listing after the
//! first never answers.
//!
//! When `TARZ_MCP_FIXTURE_PID_FILE` is set the process writes its id there
//! first. When `TARZ_MCP_FIXTURE_EXIT_MARKER` is set, `serve` writes that file
//! once the client has closed the session, just before exiting.

use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult,
    PaginatedRequestParams, ServerConfig, Tool,
};
use rmcp::service::{RequestContext, RoleServer, ServiceExt};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

const ROLE: &str = "TARZ_MCP_FIXTURE_ROLE";
const PID_FILE: &str = "TARZ_MCP_FIXTURE_PID_FILE";
const EXIT_MARKER: &str = "TARZ_MCP_FIXTURE_EXIT_MARKER";
const RELIST: &str = "TARZ_MCP_FIXTURE_RELIST";
/// Reason `fail` writes to stderr.
const FAILURE: &str = "fixture: missing configuration";
/// How long `hang` waits before answering; the tests' deadlines are shorter.
const HANG: Duration = Duration::from_secs(10);
/// A name Causa namespaces into more than 64 bytes.
const LONG_NAME: &str = "a_tool_name_that_is_far_too_long_to_fit_any_provider_limit_xyz";

fn main() {
    if let Some(path) = std::env::var_os(PID_FILE) {
        std::fs::write(path, std::process::id().to_string()).expect("write pid file");
    }
    match std::env::var(ROLE).as_deref() {
        Ok("serve") => serve(),
        Ok("fail") => {
            eprintln!("{FAILURE}");
            std::process::exit(2);
        }
        Ok("slow") => std::thread::sleep(Duration::from_secs(60)),
        _ => {
            eprintln!("set {ROLE} to serve, fail or slow");
            std::process::exit(2);
        }
    }
}

fn serve() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        if let Ok(running) = Fixture
            .serve((tokio::io::stdin(), tokio::io::stdout()))
            .await
        {
            let _ = running.waiting().await;
        }
    });
    if let Some(path) = std::env::var_os(EXIT_MARKER) {
        std::fs::write(path, "exited").expect("write exit marker");
    }
}

/// Tool listings answered so far.
static LISTINGS: AtomicUsize = AtomicUsize::new(0);

struct Fixture;

fn tool(name: &str, description: &str) -> Tool {
    let schema = serde_json::json!({ "type": "object" });
    Tool::new(
        name.to_owned(),
        description.to_owned(),
        Arc::new(schema.as_object().cloned().unwrap_or_default()),
    )
}

/// Text of about `bytes` bytes with recognizable ends and, in between,
/// multibyte characters and characters JSON has to escape.
fn big_text(bytes: usize) -> String {
    let mut text = String::from("HEAD-MARK ");
    while text.len() < bytes {
        text.push_str("é\"\\\n漢\u{1}abc ");
    }
    text.push_str(" TAIL-MARK");
    text
}

impl ServerHandler for Fixture {
    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, rmcp::ErrorData> {
        if LISTINGS.fetch_add(1, Ordering::SeqCst) > 0
            && std::env::var(RELIST).as_deref() == Ok("hang")
        {
            std::future::pending::<()>().await;
        }
        Ok(ListToolsResult::with_all_items(vec![
            tool("echo", "Echoes its arguments."),
            tool("env", "Lists the server's environment."),
            tool("cwd", "Names the server's working directory."),
            tool("big", "Returns about `bytes` bytes of text."),
            tool("hang", "Answers after a long delay."),
            tool("big_error", "Fails with about `bytes` bytes of error text."),
            tool("bad.name", "A name providers cannot call."),
            tool(LONG_NAME, "A name too long for providers."),
        ]))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, rmcp::ErrorData> {
        let arguments = request
            .arguments
            .map(serde_json::Value::Object)
            .unwrap_or_default();
        let text = match request.name.as_ref() {
            "echo" => format!("echo-result:{arguments}"),
            "env" => {
                let environment: std::collections::BTreeMap<String, String> =
                    std::env::vars().collect();
                serde_json::to_string(&environment).expect("environment as JSON")
            }
            "cwd" => std::env::current_dir()
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
            "big" => big_text(
                arguments
                    .get("bytes")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(100_000) as usize,
            ),
            "big_error" => {
                let bytes = arguments
                    .get("bytes")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(300_000) as usize;
                return Err(rmcp::ErrorData::internal_error(
                    format!("ERR-HEAD {} ERR-TAIL", "E".repeat(bytes)),
                    None,
                ));
            }
            "hang" => {
                tokio::time::sleep(HANG).await;
                "late".to_owned()
            }
            other => {
                return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                    "unknown tool: {other}"
                ))])
                .into());
            }
        };
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]).into())
    }

    fn get_info(&self) -> ServerConfig {
        ServerConfig::default()
    }
}
