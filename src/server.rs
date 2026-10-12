//! The MCP tool surface devbox-mcp exposes over stdio.
//!
//! The session tools of SPEC.md "MCP tools" are being added here one by one (`session_create`, `session_list` so far),
//! next to the prototype's `run_turn`, which runs one Codex turn to completion and goes away once sessions can run
//! turns. Each tool handler only translates between MCP and [`Sessions`]; the rules live there. SPEC_impl.md's
//! implementation status lists what is still missing.

use std::path::PathBuf;
use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ServerCapabilities, ServerConfig};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{ErrorData as McpError, Json, ServerHandler, schemars, tool, tool_handler, tool_router};
use serde::{Deserialize, Serialize};

use crate::codex::{self, Codex, Progress, TurnRequest, TurnResult};
use crate::sessions::{SessionError, SessionPage, SessionSummary, Sessions};
use crate::store::Agent;

/// Server-wide settings and state, fixed at startup from the command line.
#[derive(Clone)]
pub struct Config {
    pub sessions: Arc<Sessions>,
    /// Where the prototype's `run_turn` runs when the caller names no working directory. Must be absolute.
    pub workdir_root: PathBuf,
    pub codex: Codex,
}

impl From<SessionError> for McpError {
    fn from(err: SessionError) -> Self {
        match err {
            SessionError::Invalid(message) => McpError::invalid_params(message, None),
            SessionError::Internal(message) => McpError::internal_error(message, None),
        }
    }
}

// Arguments of `session_create`. Doc comments on schema types and their fields become JSON Schema descriptions, which
// is text the calling model reads, so they are written for that model.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SessionCreateParams {
    /// The new session's name: a lowercase letter, then lowercase letters, digits, `_`, or `-`; at most 64 characters.
    /// You refer to the session by this name from then on.
    pub name: String,
    /// Absolute path of the directory the agent works in. It is created if missing; an existing directory is used as it
    /// is, with whatever it already contains, and may be shared with other sessions. Omit it to use a directory named
    /// after the session under devbox-mcp's default root (also reused if it already exists).
    pub workdir: Option<String>,
    /// The agent to run in this session. Only `codex` is available, and it is the default.
    pub agent: Option<Agent>,
}

// Arguments of `session_list`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SessionListParams {
    /// To get the next page, pass the `next_after` value of the previous page. Omit it for the first page.
    pub after: Option<String>,
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
#[derive(Clone)]
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

    /// Creates a session (SPEC.md `SES-1` to `SES-3`, `SES-7`).
    #[tool(
        description = "Create a new session: a long-lived conversation with a coding agent (Codex) that runs on this \
                       Linux box with full access to it, working in one directory. Use it once per piece of work you \
                       want to come back to; later messages to the same session keep the agent's context. Returns the \
                       session. Fails if the name is invalid or already taken: an existing session with that name is \
                       shown in the error, so if it is yours, keep using it instead of creating another."
    )]
    async fn session_create(
        &self,
        Parameters(params): Parameters<SessionCreateParams>,
    ) -> Result<Json<SessionSummary>, McpError> {
        Ok(Json(self.config.sessions.create(
            &params.name,
            params.workdir.as_deref(),
            params.agent,
        )?))
    }

    /// Lists sessions a page at a time (SPEC.md `SES-4`).
    #[tool(
        description = "List the sessions on this box, in name order, up to 50 per call, with each session's agent, \
                       working directory, and its most recent turn (its state and, once it has ended, the start of the \
                       agent's final message). Use it to find sessions you created earlier, for example after \
                       reconnecting. When `next_after` is set, call again with `after` set to it for the next page."
    )]
    async fn session_list(
        &self,
        Parameters(params): Parameters<SessionListParams>,
    ) -> Result<Json<SessionPage>, McpError> {
        Ok(Json(self.config.sessions.list(params.after.as_deref())))
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
