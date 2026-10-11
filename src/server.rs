//! The MCP tool surface devbox-mcp exposes over stdio: the session tools of SPEC.md "MCP tools".
//!
//! Each tool handler only translates between MCP and [`Sessions`]; the rules live there. None of them waits for an agent
//! (SPEC.md `BND-2`): tunnel-client forwards calls one at a time, so a waiting call would hold up every other call.
//!
//! Doc comments on the argument types below, and the `description` of each tool, are what a calling model reads to learn
//! how to use devbox-mcp (SPEC.md `DOC-*`), so they are written for that model. Notes meant for developers on those
//! types are plain `//` comments, which stay out of the schemas.

use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ServerCapabilities, ServerConfig};
use rmcp::{ErrorData as McpError, Json, ServerHandler, schemars, tool, tool_handler, tool_router};
use serde::Deserialize;

use crate::sessions::{
    ReadResult, SendResult, SessionError, SessionPage, SessionSummary, Sessions, TurnSummary,
};
use crate::store::Agent;

impl From<SessionError> for McpError {
    fn from(err: SessionError) -> Self {
        match err {
            SessionError::Invalid(message) => McpError::invalid_params(message, None),
            SessionError::Internal(message) => McpError::internal_error(message, None),
        }
    }
}

// Arguments of `session_create`.
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

// Arguments of `session_send`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SessionSendParams {
    /// The session to send to.
    pub name: String,
    /// What the agent should do, as you would write it to a capable colleague. At most 256 KiB.
    pub message: String,
    /// Optional ID you choose for this send (1 to 128 characters from A-Z, a-z, 0-9, `.`, `_`, `:`, `-`). If you are not
    /// sure an earlier send went through (for example, its response never arrived), send again with the same ID and the
    /// same message: you get the turn that send started instead of a second turn. Use a new ID for every new message.
    pub request_id: Option<String>,
}

// Arguments of `session_read`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SessionReadParams {
    /// The session to read.
    pub name: String,
    /// Where to continue from: the `cursor` from `session_send` or the `next_cursor` from your previous read. Omit it to
    /// read from the start of the most recent turn.
    pub cursor: Option<String>,
}

// Arguments of `session_interrupt`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SessionInterruptParams {
    /// The session whose running turn to stop.
    pub name: String,
}

/// The MCP server handler. Cheap to clone; rmcp clones it per request.
#[derive(Clone)]
pub struct DevboxServer {
    sessions: Arc<Sessions>,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl DevboxServer {
    pub fn new(sessions: Arc<Sessions>) -> Self {
        Self {
            sessions,
            tool_router: Self::tool_router(),
        }
    }

    /// Creates a session (SPEC.md `SES-1` to `SES-3`, `SES-7`).
    #[tool(
        description = "Create a new session: a long-lived conversation with a coding agent (Codex) that runs on this \
                       Linux box with full access to it, working in one directory. Create one per piece of work you \
                       want to come back to; later messages to the same session keep the agent's context. Returns the \
                       session. Fails if the name is invalid or already taken: an existing session with that name is \
                       shown in the error, so if it is yours, keep using it instead of creating another. Next, give \
                       the agent work with session_send."
    )]
    async fn session_create(
        &self,
        Parameters(params): Parameters<SessionCreateParams>,
    ) -> Result<Json<SessionSummary>, McpError> {
        Ok(Json(self.sessions.create(
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
        Ok(Json(self.sessions.list(params.after.as_deref())))
    }

    /// Sends a message, starting a turn in the background (SPEC.md `TRN-1`, `TRN-2`, `TRN-6`).
    #[tool(
        description = "Send a message to a session's agent, starting a turn: the agent works on it in the background, \
                       for seconds or for many minutes, and this call returns at once without waiting. Then follow the \
                       turn with session_read, passing the returned cursor, until the turn's state is no longer \
                       `running` or `interrupting`; the agent's reply is then in the turn's `final_message`. A session \
                       runs one turn at a time: sending while a turn is still running fails with a 'busy' error \
                       (wait for the turn to end, or stop it with session_interrupt). The agent remembers earlier turns \
                       of the same session, so follow-ups can refer to earlier work."
    )]
    async fn session_send(
        &self,
        Parameters(params): Parameters<SessionSendParams>,
    ) -> Result<Json<SendResult>, McpError> {
        Ok(Json(self.sessions.send(
            &params.name,
            &params.message,
            params.request_id.as_deref(),
        )?))
    }

    /// Reads a session's events after a cursor (SPEC.md `TRN-3`, `TRN-7`).
    #[tool(
        description = "Read what has happened in a session since your cursor: the agent's messages, the commands it \
                       ran with their output, the files it changed, errors, and the end of the turn, plus the current \
                       state of the most recent turn and whether the agent is still alive. Returns immediately with \
                       what is there; it never waits. Pass `next_cursor` from one read as `cursor` to the next to get \
                       only new events. If `more` is true, read again right away. While the turn is still running, \
                       read again later (every few seconds at most, or after doing other work); turns often take \
                       minutes. If `gap` is true, some events are missing (for example after devbox-mcp restarted), \
                       but the turn's state is still current."
    )]
    async fn session_read(
        &self,
        Parameters(params): Parameters<SessionReadParams>,
    ) -> Result<Json<ReadResult>, McpError> {
        Ok(Json(
            self.sessions.read(&params.name, params.cursor.as_deref())?,
        ))
    }

    /// Starts interrupting a running turn (SPEC.md `TRN-4`).
    #[tool(
        description = "Stop a session's running turn. Returns at once with the turn in state `interrupting`; the agent \
                       and everything it started are then killed, and the turn becomes `interrupted` once they are \
                       gone (follow it with session_read). If the agent happened to finish just before, the turn keeps \
                       its own outcome. If no turn is running, nothing changes and the most recent turn is returned. \
                       Work the agent already did (file changes, for example) is not undone."
    )]
    async fn session_interrupt(
        &self,
        Parameters(params): Parameters<SessionInterruptParams>,
    ) -> Result<Json<TurnSummary>, McpError> {
        Ok(Json(self.sessions.interrupt(&params.name)?))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for DevboxServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            // Not `Implementation::from_build_env()`: that macro expands inside rmcp and reports rmcp's own name and version.
            .with_server_info(Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "devbox-mcp runs coding agents (Codex) on a remote Linux box, with full access to it, on your behalf. \
                 Work happens in sessions: a session is a long-lived conversation with one agent, working in one \
                 directory, that you can come back to later with its context intact. Typical use: session_create once, \
                 then session_send a message (it returns at once; the agent works in the background), then \
                 session_read with the returned cursor, repeatedly, until the turn's state is neither `running` nor \
                 `interrupting` and `more` is false; the agent's reply is the turn's `final_message`. Send follow-ups to the same session. No call ever waits \
                 for the agent, so pace your reads (every few seconds at most, or after doing other work), and expect \
                 long turns to take minutes. Use session_list to find earlier sessions, and session_interrupt to stop \
                 a turn.",
            )
    }
}
