//! devbox-mcp: an MCP server that lets remote agents drive coding agents on a Linux box.
//!
//! It speaks MCP over stdio only, because it is meant to be started by the OpenAI tunnel client and must never open a
//! way into the box of its own (SPEC.md, `SEC-2`). See SPEC.md for what it promises and SPEC_impl.md for how it is
//! built and how far along it is.

mod codex;
mod server;

use std::path::PathBuf;

use anyhow::{Context as _, bail};
use clap::Parser;
use rmcp::ServiceExt as _;
use tokio::signal::unix::{SignalKind, signal};

/// Command-line flags. All configuration comes from here rather than from environment variables, so tests can inject
/// every path and executable (SPEC_impl.md, "Configuration").
#[derive(Debug, Parser)]
#[command(version, about)]
struct Args {
    /// Directory turns run in when the caller names none. Defaults to ~/devbox-mcp/work. Must be absolute.
    #[arg(long)]
    workdir_root: Option<PathBuf>,

    /// The Codex executable to run.
    #[arg(long, default_value = "codex")]
    codex_bin: PathBuf,
}

/// The default working directory root, `~/devbox-mcp/work`.
fn default_workdir_root() -> anyhow::Result<PathBuf> {
    let home =
        std::env::home_dir().context("cannot determine home directory; pass --workdir-root")?;
    Ok(home.join("devbox-mcp").join("work"))
}

// A current-thread runtime is load-bearing: agent children get a parent-death signal, which Linux ties to the thread
// that spawned them. With one thread, that is the main thread, which lives exactly as long as the process. See
// `codex::set_parent_death_signal`.
#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let workdir_root = match args.workdir_root {
        Some(dir) => dir,
        None => default_workdir_root()?,
    };
    if !workdir_root.is_absolute() {
        bail!(
            "--workdir-root must be absolute, got {}",
            workdir_root.display()
        );
    }

    // Installed before serving so a signal that arrives during MCP initialization is not lost to the default
    // disposition, which would kill the process without cleanup.
    let mut sigterm = signal(SignalKind::terminate()).context("cannot install SIGTERM handler")?;
    let mut sigint = signal(SignalKind::interrupt()).context("cannot install SIGINT handler")?;

    let server = server::DevboxServer::new(server::Config {
        workdir_root,
        codex: codex::Codex {
            bin: args.codex_bin,
        },
    });
    let service = server
        .serve(rmcp::transport::stdio())
        .await
        .context("MCP initialization failed")?;

    // Every way out of here is an orderly shutdown: returning from main drops the tokio runtime, which drops in-flight
    // tool calls, whose guards kill the agents' process groups (`LIF-1`). That is why SIGTERM and SIGINT are caught
    // instead of left to kill the process outright: tunnel-client stops its child by closing stdin *and* sending
    // SIGTERM, and with the default disposition the agents' own children would outlive devbox-mcp.
    tokio::select! {
        result = service.waiting() => {
            result.context("MCP service failed")?;
        }
        _ = sigterm.recv() => {}
        _ = sigint.recv() => {}
    }
    Ok(())
}
