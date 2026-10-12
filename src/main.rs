//! devbox-mcp: an MCP server that lets remote agents drive coding agents on a Linux box.
//!
//! It speaks MCP over stdio only, because it is meant to be started by the OpenAI tunnel client and must never open a
//! way into the box of its own (SPEC.md, `SEC-2`). See SPEC.md for what it promises and SPEC_impl.md for how it is
//! built and how far along it is.

mod codex;
mod process;
mod server;

use std::path::PathBuf;
use std::time::Duration;

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

/// How long shutdown waits for the runtime's blocking threads after in-flight work has been dropped.
///
/// The one that matters is rmcp's stdin reader: it sits in a blocking `read` that only returns when stdin closes, so
/// shutting down on a signal while stdin is still open (systemd stopping a whole service, say) would otherwise wait
/// forever. Agents are already dead by the time this timeout starts, so cutting it short loses nothing.
const BLOCKING_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);

fn main() -> anyhow::Result<()> {
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
    let config = server::Config {
        workdir_root,
        codex: codex::Codex {
            bin: args.codex_bin,
        },
    };

    // A current-thread runtime is load-bearing: agent children get a parent-death signal, which Linux ties to the
    // thread that spawned them. With one thread, that is the main thread, which lives exactly as long as the process.
    // See `process::set_parent_death_signal`.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("cannot start tokio runtime")?;
    let result = runtime.block_on(serve(config));
    // Shutting the runtime down drops every in-flight tool call, and dropping a call kills its agent and everything the
    // agent started (`LIF-1`).
    runtime.shutdown_timeout(BLOCKING_SHUTDOWN_TIMEOUT);
    result
}

/// Serves MCP over stdio until the client goes away or devbox-mcp is told to stop.
///
/// Every way out of here leads to an orderly shutdown in `main`. That is why SIGTERM and SIGINT are caught instead of
/// left to kill the process outright: tunnel-client stops its child by closing stdin *and* sending SIGTERM, and with the
/// default disposition the agents' own children would outlive devbox-mcp. The signals are watched during MCP
/// initialization too, since a client that never finishes initializing must not make devbox-mcp unstoppable.
async fn serve(config: server::Config) -> anyhow::Result<()> {
    let mut sigterm = signal(SignalKind::terminate()).context("cannot install SIGTERM handler")?;
    let mut sigint = signal(SignalKind::interrupt()).context("cannot install SIGINT handler")?;

    let server = server::DevboxServer::new(config);
    let service = tokio::select! {
        service = server.serve(rmcp::transport::stdio()) => service.context("MCP initialization failed")?,
        _ = sigterm.recv() => return Ok(()),
        _ = sigint.recv() => return Ok(()),
    };
    tokio::select! {
        result = service.waiting() => {
            result.context("MCP service failed")?;
        }
        _ = sigterm.recv() => {}
        _ = sigint.recv() => {}
    }
    Ok(())
}
