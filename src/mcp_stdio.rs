//! MCP over stdio, adapting the protocol to [`crate::tools::ToolSurface`].
//!
//! The whole of `silent-critic`'s dependency on an MCP library lives here
//! (`REPO_INVARIANTS.md` ENG-010), mirroring `kb`'s `mcp_stdio.rs` exactly.
//! Handlers in [`crate::tools`] know nothing about JSON-RPC, content blocks,
//! protocol versions, or capability negotiation; this module translates in
//! both directions and does nothing else, so replacing the protocol library
//! is a rewrite of one file rather than of the tool surface — and the
//! adapter itself knows nothing about supervision: it enumerates
//! [`crate::tools::ToolSurface::specs`] and dispatches through
//! [`crate::tools::ToolSurface::call`].
//!
//! **A tool that failed returns a result, not a protocol error.** MCP
//! distinguishes the two, and the distinction is about whose problem it is: a
//! protocol error says the server could not route the request, and clients
//! render it opaquely, so an agent asking `dispatch` for a task that is not
//! ready would be told only that something went wrong internally. That is a
//! worse answer than the reason [`crate::tools::ToolOutcome::Failed`]
//! carries, which is a fact the agent can act on.

use std::borrow::Cow;
use std::sync::Arc;

use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    ListToolsResult, PaginatedRequestParams, ProtocolVersion, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{ErrorData as McpError, ServiceExt};

use crate::tools::{ToolOutcome, ToolSurface};

/// An MCP server over a tool surface.
pub struct McpServer<S> {
    surface: Arc<S>,
}

impl<S> McpServer<S> {
    /// A server offering `surface`.
    pub fn new(surface: S) -> Self {
        Self {
            surface: Arc::new(surface),
        }
    }
}

/// Translate a domain tool specification into the protocol's shape.
///
/// A schema that is not a JSON object is replaced by the empty object rather
/// than refused: the specifications are compile-time constants in this
/// crate, so a non-object here is a programming error that a running server
/// should survive as an unhelpful tool rather than as a failure to start.
fn as_tool(spec: &crate::tools::ToolSpec) -> Tool {
    let schema = spec
        .schema
        .as_object()
        .cloned()
        .unwrap_or_else(serde_json::Map::new);
    Tool::new(spec.name, spec.description, Arc::new(schema))
}

/// The protocol revisions this server implements.
///
/// rmcp also knows 2026-07-28 and, left to its default, agrees to it whenever
/// a client asks. That revision changes what results must carry (discovery,
/// cache hints), none of which this handler produces. Negotiation is bounded
/// to the revisions the tests exercise; a newer request settles on
/// 2025-11-25, mirroring `kb`'s `mcp_stdio.rs`.
const SUPPORTED_PROTOCOL_VERSIONS: &[ProtocolVersion] = &[
    ProtocolVersion::V_2024_11_05,
    ProtocolVersion::V_2025_03_26,
    ProtocolVersion::V_2025_06_18,
    ProtocolVersion::V_2025_11_25,
];

impl<S: ToolSurface + Send + Sync + 'static> ServerHandler for McpServer<S> {
    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(SUPPORTED_PROTOCOL_VERSIONS)
    }

    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            // Scope-dependent (fix round 2, finding #4): a worker session
            // must never be told the plan carries hidden acceptance
            // criteria at all.
            .with_instructions(self.surface.instructions())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(
            self.surface.specs().iter().map(as_tool).collect(),
        ))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let arguments = request
            .arguments
            .map_or_else(|| serde_json::json!({}), serde_json::Value::Object);
        // Coerced to a trait object so `dispatch` has exactly one
        // monomorphization: as a function generic over `S`, this coverage
        // instrumentation tracks each instantiation separately, and no
        // single caller here ever exercises both `Text`/`Failed` and a
        // panicking tool — a shared instantiation is what lets one test
        // suite prove the whole function.
        let surface = Arc::clone(&self.surface) as Arc<dyn ToolSurface + Send + Sync>;
        let result = dispatch(surface, request.name.to_string(), arguments).await;
        Ok(CallToolResponse::from(result))
    }
}

/// Call `name` on `surface` off the reactor thread and translate the
/// outcome, isolated from [`ServerHandler::call_tool`] so it is testable
/// without a real MCP session (constructing a live [`RequestContext`]
/// requires a connected [`rmcp::service::Peer`], which nothing outside this
/// crate's own transport can produce). Takes a trait object rather than a
/// generic parameter for the reason noted at its one call site.
///
/// Never fails as a `Result`: a tool that panicked mid-call is exactly the
/// kind of failure [`ToolOutcome::Failed`] exists for (the call reached a
/// handler and did not finish), so a `spawn_blocking` `JoinError` is folded
/// into a `Failed` outcome here rather than surfaced as an
/// [`McpError`]/transport-level error, matching the brief's invariant that a
/// tool that could not do its job answers with the reason as content, never
/// a protocol error.
async fn dispatch(
    surface: Arc<dyn ToolSurface + Send + Sync>,
    name: String,
    arguments: serde_json::Value,
) -> CallToolResult {
    // Off the reactor thread, always. The surface is synchronous by design
    // (file I/O, git plumbing), so running it here would block every other
    // request on this thread.
    let called = name.clone();
    let outcome = tokio::task::spawn_blocking(move || surface.call(&called, &arguments))
        .await
        .unwrap_or_else(|e| ToolOutcome::Failed(format!("the {name} tool did not finish: {e}")));
    match outcome {
        ToolOutcome::Text(text) => CallToolResult::success(vec![ContentBlock::text(text)]),
        ToolOutcome::Failed(reason) => CallToolResult::error(vec![ContentBlock::text(reason)]),
    }
}

/// Serve `surface` over stdin and stdout until the client disconnects.
///
/// # Errors
///
/// Returns the reason the transport could not be established or did not
/// shut down cleanly.
pub async fn serve<S: ToolSurface + Send + Sync + 'static>(
    surface: S,
) -> Result<(), Box<dyn std::error::Error>> {
    let service = McpServer::new(surface)
        .serve(rmcp::transport::stdio())
        .await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Arc, ContentBlock, McpServer, ServerHandler, ToolOutcome, ToolSurface, dispatch};
    use crate::tools::ToolSpec;

    /// A surface whose `call` produces every outcome `dispatch` must
    /// translate, keyed by name: `"text"` succeeds, `"failed"` fails
    /// readably, and anything else panics mid-call. One mock covering all
    /// three keeps `dispatch`'s translation logic provable within the same
    /// compiled artifact this module's own tests run in — the coverage
    /// instrumentation tracks this artifact's copy of `dispatch`
    /// separately from the one linked into `tests/mcp_stdio.rs`'s spawned
    /// binary, so a real protocol session exercising `Text`/`Failed` there
    /// does not, by itself, prove this artifact's copy does the same.
    struct ThreeOutcomeSurface;

    impl ToolSurface for ThreeOutcomeSurface {
        fn specs(&self) -> Vec<ToolSpec> {
            vec![ToolSpec {
                name: "text",
                description: "returns Text",
                schema: serde_json::json!({ "type": "object" }),
            }]
        }

        fn call(&self, name: &str, _arguments: &serde_json::Value) -> ToolOutcome {
            match name {
                "text" => ToolOutcome::Text("ok".to_owned()),
                "failed" => ToolOutcome::Failed("nope".to_owned()),
                // `clippy::panic` denies this crate-wide; allowed here
                // because panicking inside the `spawn_blocking` closure is
                // exactly how this test reaches `dispatch`'s `JoinError`
                // branch — the one path a real protocol session cannot
                // exercise (constructing a live `RequestContext` needs a
                // connected `rmcp` transport `Peer`, which nothing outside
                // this crate can produce).
                #[allow(clippy::panic)]
                _ => panic!("a tool that never returns, for the join-error path"),
            }
        }

        fn instructions(&self) -> &'static str {
            "test surface"
        }
    }

    /// `get_info`'s instructions come from `ToolSurface::instructions`
    /// (fix round 2, finding #4), exercised directly rather than only
    /// through a real protocol handshake in `tests/mcp_stdio.rs`'s spawned
    /// binary -- a separately-compiled artifact whose own executions this
    /// module's coverage instance cannot take credit for (see the comment
    /// on `ThreeOutcomeSurface` above).
    #[test]
    fn get_info_carries_the_surfaces_own_instructions() {
        let info = McpServer::new(ThreeOutcomeSurface).get_info();
        assert_eq!(Some("test surface"), info.instructions.as_deref());
    }

    fn text_of(result: &super::CallToolResult) -> Option<&str> {
        result
            .content
            .first()
            .and_then(ContentBlock::as_text)
            .map(|text| text.text.as_str())
    }

    #[test]
    fn dispatch_translates_every_outcome_a_tool_call_can_produce()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(ThreeOutcomeSurface.specs().len(), 1);

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let surface = Arc::new(ThreeOutcomeSurface);

        let text_result = runtime.block_on(dispatch(
            Arc::clone(&surface) as Arc<dyn ToolSurface + Send + Sync>,
            "text".to_owned(),
            serde_json::json!({}),
        ));
        assert!(
            !text_result.is_error.unwrap_or(false),
            "a Text outcome was flagged an error: {text_result:?}"
        );

        let failed_result = runtime.block_on(dispatch(
            Arc::clone(&surface) as Arc<dyn ToolSurface + Send + Sync>,
            "failed".to_owned(),
            serde_json::json!({}),
        ));
        assert!(
            failed_result.is_error.unwrap_or(false),
            "a Failed outcome was not flagged an error: {failed_result:?}"
        );

        let panicked_result = runtime.block_on(dispatch(
            surface as Arc<dyn ToolSurface + Send + Sync>,
            "whatever".to_owned(),
            serde_json::json!({}),
        ));
        assert!(
            panicked_result.is_error.unwrap_or(false),
            "a panicking tool call was not flagged an error: {panicked_result:?}"
        );
        let text = text_of(&panicked_result).ok_or("no text content in the result")?;
        assert!(
            text.contains("whatever") && text.contains("did not finish"),
            "the join failure did not name the tool: {text}"
        );

        Ok(())
    }
}
