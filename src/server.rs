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
    ReadResult, RemoveResult, SendResult, SessionError, SessionPage, SessionSummary, Sessions,
    TurnSummary,
};
use crate::store::Agent;
use crate::transcript::Page;

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
    /// The agent to run in this session: `codex` (OpenAI's Codex CLI, the default) or `claude` (Anthropic's Claude
    /// Code). Fixed for the session's life.
    pub agent: Option<Agent>,
    /// The model the agent runs: 1 to 128 characters from A-Z, a-z, 0-9, `.`, `_`, `:`, `/`, and `-`. Defaults to
    /// `gpt-6.1-sol` for codex and `opus` for claude (which also takes `sonnet`, `haiku`, or a full model ID). Fixed for
    /// the session's life; use a new session to switch. That check is only about characters: devbox-mcp does not check
    /// that the agent knows the model, and if it does not, the session's turns fail with the agent's own error, which
    /// names the problem.
    pub model: Option<String>,
    /// The reasoning effort, for example `low`, `medium`, `high` (the default for both agents), `xhigh`, or `max`: 1 to
    /// 32 characters from a-z, 0-9, `_`, and `-`. Fixed for the session's life. For claude it must be one of those five
    /// and anything else is rejected when the session is created. For codex the check is only about characters: which
    /// efforts are available depends on the model, and an unsupported one makes the session's turns fail with an error
    /// listing the valid values.
    pub effort: Option<String>,
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

// Arguments of `session_transcript`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SessionTranscriptParams {
    /// The session whose transcript to read.
    pub name: String,
    /// The index of the first entry to read, counting from 0. Omit it to read the last entries, which is usually what
    /// you want (what happened most recently, or how the session ended).
    pub from: Option<u64>,
    /// How many entries to read, 1 to 50; 20 when omitted. A read may return fewer when the entries are large.
    pub limit: Option<usize>,
}

// Arguments of `session_remove`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SessionRemoveParams {
    /// The session to remove.
    pub name: String,
    /// Remove the session even when devbox-mcp cannot confirm that the agent's stored data for it was deleted, which
    /// may then remain in the agent's storage. Defaults to false. Use it only when the user wants the session gone
    /// anyway.
    pub force: Option<bool>,
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
    /// The server instructions, built once at startup because they name the Unix user the agents run as.
    instructions: Arc<str>,
}

#[tool_router]
impl DevboxServer {
    /// A server for `sessions`, whose agents run as `unix_user` (named in the instructions, so a caller knows whose
    /// files and credentials the agents work with).
    pub fn new(sessions: Arc<Sessions>, unix_user: &str) -> Self {
        Self {
            sessions,
            tool_router: Self::tool_router(),
            instructions: instructions(unix_user).into(),
        }
    }

    /// Creates a session (SPEC.md `SES-1` to `SES-3`, `SES-7`).
    #[tool(
        description = "Create a new session: a long-lived conversation with a coding agent (Codex by default, or \
                       Claude Code) that runs on this Linux box with full access to it, working in one directory. \
                       Create one per piece of work you want to come back to; later messages to the same session keep \
                       the agent's context. You may choose the agent, model, and reasoning effort; all are fixed for \
                       the session's life and shown in the returned session. A choice the agent does not support is \
                       never silently ignored, but whether it is rejected here depends on the agent and the value: an \
                       unknown Claude effort is rejected now, while an unknown model, or an effort Codex's model does \
                       not support, is accepted here and makes the session's turns fail with the agent's own error. \
                       So a successful creation does not mean the agent supports your choice, and what one agent \
                       rejects here says nothing about another. Returns the session. Fails if the name is invalid or \
                       already taken: an existing session with that name is shown in the error, so if it is yours, \
                       keep using it instead of creating another. Next, give the agent work with session_send."
    )]
    async fn session_create(
        &self,
        Parameters(params): Parameters<SessionCreateParams>,
    ) -> Result<Json<SessionSummary>, McpError> {
        Ok(Json(self.sessions.create(
            &params.name,
            params.workdir.as_deref(),
            params.agent,
            params.model.as_deref(),
            params.effort.as_deref(),
        )?))
    }

    /// Lists sessions a page at a time (SPEC.md `SES-4`).
    #[tool(
        description = "List the sessions on this box, in name order, up to 50 per call, with each session's agent, \
                       model and effort, working directory, and its most recent turn (its state and, once it has ended, \
                       the start of the agent's final message). Use it to find sessions you created earlier, for example after \
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
                       turn with session_read, passing the returned cursor and then each read's `next_cursor`, until \
                       the turn's state is neither `running` nor `interrupting` and `more` is false; the agent's reply \
                       is then in the turn's `final_message`. A session runs one turn at a time: sending while a turn \
                       is still running fails with a 'busy' error (wait for the turn to end, or stop it with \
                       session_interrupt). The agent remembers earlier turns of the same session, so follow-ups can \
                       refer to earlier work, unless the result has `context_reset` true: then the agent could not \
                       continue its earlier conversation and starts without it, so put any context it needs into the \
                       message. If you are not sure a send went through, send again with the same `request_id`: you \
                       get the original turn, with `replayed` true. session_read always shows the session's most \
                       recent turn, so if a replayed `turn_id` is older than the turn a read shows, later turns have \
                       replaced it and its outcome is no longer available."
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
        description = "Stop a session's running turn. Returns at once with the turn in state `interrupting`; \
                       devbox-mcp then kills the agent and the processes it started, and the turn becomes \
                       `interrupted` once the agent is gone (follow it with session_read). If the ended turn has \
                       `cleanup_uncertain` true, devbox-mcp could not confirm that every process the agent started is \
                       gone, so do not report a clean stop. If the agent happened to finish just before, the turn \
                       keeps its own outcome. If no turn is running, nothing changes and the most recent turn is \
                       returned; a session that was never sent a message has no turn, and that is an error. Work the \
                       agent already did (file changes, for example) is not undone."
    )]
    async fn session_interrupt(
        &self,
        Parameters(params): Parameters<SessionInterruptParams>,
    ) -> Result<Json<TurnSummary>, McpError> {
        Ok(Json(self.sessions.interrupt(&params.name)?))
    }

    /// Reads the agent's own transcript of a session (SPEC.md `TRA-2`).
    #[tool(
        description = "Read the agent's own transcript of a session: its full record of the conversation (your \
                       messages, its replies, every command and tool call with its output, and the agent's internal \
                       bookkeeping), as the agent's own JSON entries, one per line of its transcript file. Use it to \
                       see exactly what the agent did, beyond the events session_read gives. Without `from` it \
                       returns the last entries, so what happened most recently comes first to hand; to look further \
                       back, read again with `from` set to the first returned index minus `limit`, and use \
                       `next_from` to page forwards. Strings longer than 4 KiB are cut and end in ' [cut]', and an \
                       entry too large even then comes back as the start of its text with `truncated` set. The format \
                       of entries is the agent's own and differs between agents. Fails, saying why, when the session \
                       has no transcript yet or the agent's transcript file cannot be found or read; then ask the \
                       agent itself with session_send. Reading a very large transcript takes a moment."
    )]
    async fn session_transcript(
        &self,
        Parameters(params): Parameters<SessionTranscriptParams>,
    ) -> Result<Json<Page>, McpError> {
        Ok(Json(
            self.sessions
                .transcript(&params.name, params.from, params.limit)
                .await?,
        ))
    }

    /// Removes a session (SPEC.md `SES-6`).
    #[tool(
        description = "Remove a session you no longer need: deletes the agent's own stored conversation for it and \
                       forgets the session, so its name can be used again. The session's working directory and the \
                       files in it are left alone. Returns the session as it was, and whether the agent's data was \
                       deleted. Fails if the session has a turn running or being interrupted (wait for it to end, or \
                       stop it with session_interrupt first). Also fails, keeping the session, when devbox-mcp cannot \
                       confirm that the agent's data was deleted: deleting it failed or left data devbox-mcp does not \
                       know how to remove, or the session had a conversation whose ID devbox-mcp never learned (it \
                       then cannot delete it). Such a failure \
                       says why; retrying may help if the cause was temporary. Only pass `force: true` when the user \
                       wants the session gone even if the agent's data may remain; do not use it by default. This \
                       call can take several seconds: up to 10 for Codex's delete command, and for Claude the time to \
                       delete its files and check that none are left."
    )]
    async fn session_remove(
        &self,
        Parameters(params): Parameters<SessionRemoveParams>,
    ) -> Result<Json<RemoveResult>, McpError> {
        Ok(Json(
            self.sessions
                .remove(&params.name, params.force.unwrap_or(false))
                .await?,
        ))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for DevboxServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            // Not `Implementation::from_build_env()`: that macro expands inside rmcp and reports rmcp's own name and version.
            .with_server_info(Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(self.instructions.to_string())
    }
}

/// The server instructions (SPEC.md `DOC-1`): what devbox-mcp is, the workflow, and the execution environment a caller
/// needs to know before handing an agent real work.
///
/// The environment part was checked on a deployed service: commands run without a terminal and with stdin at end of
/// file, so `read`, `getpass`, and ssh fail at once instead of prompting, and network access works. That holds for
/// programs that give up on end of input, not for all of them, which is why the text promises no more than that.
fn instructions(unix_user: &str) -> String {
    format!(
        "devbox-mcp runs coding agents (Codex or Claude Code) on a remote Linux box, with full access to it, on your behalf. \
         Work happens in sessions: a session is a long-lived conversation with one agent, working in one \
         directory, that you can come back to later with its context intact. Typical use: session_create once, \
         then session_send a message (it returns at once; the agent works in the background), then \
         session_read with the returned cursor, repeatedly, until the turn's state is neither `running` nor \
         `interrupting` and `more` is false; the agent's reply is the turn's `final_message`. Send follow-ups \
         to the same session. No call ever waits for the agent, so pace your reads (every few seconds at most, \
         or after doing other work), and expect long turns to take minutes. Use session_list to find earlier \
         sessions, session_transcript to read the agent's own detailed record of a session, session_interrupt to \
         stop a turn, and session_remove to delete a session you no longer need.\n\n\
         How agents run: as the Unix user {unix_user}, with no sandbox, no approval prompts, and unrestricted \
         network access, so an agent can do whatever that user can do on this box, with that user's files and \
         credentials and the programs on the PATH devbox-mcp was set up with. It works in its session's working \
         directory. Commands get no terminal and empty input, so most programs that would ask for input (a \
         password prompt, an ssh login, a confirmation) fail at once and the failure shows up in the turn's events. \
         A program may handle missing input differently, though, for example by asking again forever, and nothing \
         times commands out, so ask the agent to run things non-interactively; if a turn stops producing events \
         while its agent stays alive, check on it and interrupt it if it is stuck. A session's model \
         and reasoning effort are fixed when it is created and shown in its summary. devbox-mcp keeps no list of \
         models, so an unsupported choice is not ignored: depending on the agent and the value, either creating \
         the session fails or the session's turns fail with the agent's own error saying what is wrong."
    )
}
