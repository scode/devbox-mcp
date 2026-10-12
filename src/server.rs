//! The MCP tool surface devbox-mcp exposes over stdio.
//!
//! This is the primordial prototype surface: a single `run_turn` tool that runs one Codex turn to completion and
//! returns its reply. It exists to prove the end-to-end path (MCP client -> devbox-mcp -> agent -> reply) and the
//! testing flow around it. It deliberately does not yet follow SPEC.md's session and turn model; SPEC_impl.md's
//! implementation status lists the gaps.

use std::path::PathBuf;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ServerCapabilities, ServerConfig};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{ErrorData as McpError, Json, ServerHandler, schemars, tool, tool_handler, tool_router};
use serde::{Deserialize, Serialize};

use crate::codex::{self, Codex, Progress, TurnRequest, TurnResult};

/// Server-wide settings, fixed at startup from the command line.
#[derive(Debug, Clone)]
pub struct Config {
    /// Where turns run when the caller names no working directory. Must be absolute.
    pub workdir_root: PathBuf,
    pub codex: Codex,
}

// Arguments of the `run_turn` tool. Doc comments on schema types and their fields become JSON Schema descriptions,
// which is text the calling model reads, so they are written for that model. Notes meant for developers, like this one,
// are plain comments to keep them out of the schema.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RunTurnParams {
    /// The message to send to the agent.
    pub prompt: String,
    /// Absolute path of the directory the agent works in. Defaults to the server's configured working directory root.
    /// Created if it does not exist.
    pub workdir: Option<String>,
}

// Result of the `run_turn` tool, returned as structured content (and, by rmcp, also as JSON text for clients that
// ignore structured content). A plain comment rather than a doc comment, for the same reason as on `RunTurnParams`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct RunTurnResult {
    /// True if the agent finished its turn normally.
    pub completed: bool,
    /// The agent's final message, if it produced one.
    pub reply: Option<String>,
    /// Why the turn did not complete, when it did not.
    pub error: Option<String>,
    /// The agent's native session ID, if it reported one.
    pub agent_session_id: Option<String>,
    /// The directory the agent ran in.
    pub workdir: String,
}

/// The MCP server handler. Cheap to clone; rmcp clones it per request.
#[derive(Debug, Clone)]
pub struct DevboxServer {
    config: Config,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl DevboxServer {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            tool_router: Self::tool_router(),
        }
    }

    /// Runs a single Codex turn in a fresh session and waits for it to finish.
    ///
    /// Caller mistakes (a relative `workdir`) are MCP errors. A turn that ran but failed is a successful call whose
    /// result says `completed: false`, so the caller sees the agent's own error text rather than a generic tool failure.
    #[tool(
        description = "Run one turn of the Codex agent on the remote box in a fresh session and return its final reply. \
                       Blocks until the turn finishes."
    )]
    async fn run_turn(
        &self,
        Parameters(params): Parameters<RunTurnParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<RunTurnResult>, McpError> {
        let workdir = match params.workdir {
            Some(dir) => PathBuf::from(dir),
            None => self.config.workdir_root.clone(),
        };
        if !workdir.is_absolute() {
            return Err(McpError::invalid_params(
                format!(
                    "workdir must be an absolute path, got {}",
                    workdir.display()
                ),
                None,
            ));
        }
        std::fs::create_dir_all(&workdir).map_err(|e| {
            McpError::internal_error(
                format!("cannot create workdir {}: {e}", workdir.display()),
                None,
            )
        })?;

        let req = TurnRequest {
            prompt: &params.prompt,
            workdir: &workdir,
            model: codex::DEFAULT_MODEL,
            effort: codex::DEFAULT_EFFORT,
            resume: None,
        };
        let mut native_id = None;
        // rmcp does not drop a handler's future when the client cancels the call; it only cancels this token. Dropping
        // the turn is what kills the agent, so racing the two is what makes a cancelled call stop its agent (`LIF-1`).
        let outcome = tokio::select! {
            outcome = self.config.codex.run_turn(
                &req,
                |progress| {
                    if let Progress::NativeId(id) = progress {
                        native_id = Some(id);
                    }
                },
                std::future::pending(),
            ) => outcome,
            _ = ctx.ct.cancelled() => {
                return Err(McpError::internal_error("run_turn was cancelled by the client", None));
            }
        }
        .map_err(|e| McpError::internal_error(format!("{e:#}"), None))?;
        let (completed, error) = match outcome.result {
            TurnResult::Completed => (true, None),
            TurnResult::Failed { error } => (false, Some(error)),
            TurnResult::Interrupted => (false, Some("the turn was interrupted".to_owned())),
        };
        Ok(Json(RunTurnResult {
            completed,
            reply: outcome.final_message,
            error,
            agent_session_id: native_id,
            workdir: workdir.display().to_string(),
        }))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for DevboxServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            // Not `Implementation::from_build_env()`: that macro expands inside rmcp and reports rmcp's own name and version.
            .with_server_info(Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "devbox-mcp runs coding agents on a remote Linux box. This is an early prototype with a single tool, \
                 run_turn, which runs one Codex turn and returns its reply.",
            )
    }
}
