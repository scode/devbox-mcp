//! End-to-end tests with the real tunnel-client between the MCP client and devbox-mcp.
//!
//! tunnel-client runs in its local `dev proxy` mode (see `support::dev_proxy`), so these tests need its binary but no
//! OpenAI account, credentials, or network. Install the pinned binary with `scripts/install-tunnel-client.sh`; CI does
//! the same. They exist because tunnel-client is not a transparent pipe: it serializes requests, applies response
//! deadlines, and owns devbox-mcp's process lifecycle, and the stdio tests only imitate that.

mod support;

use std::path::Path;
use std::time::Duration;

use serde_json::json;
use support::dev_proxy::{DevProxy, default_tunnel_client};
use support::{DEVBOX_MCP, Harness, assert_all_gone, run_turn, send_signal, structured};

/// Starts devbox-mcp on this harness behind the pinned tunnel-client.
async fn start_proxy(h: &Harness) -> DevProxy {
    DevProxy::start(
        &default_tunnel_client(),
        Path::new(DEVBOX_MCP),
        &h.devbox_args(),
        h.tmp.path(),
        Duration::from_secs(30),
    )
    .await
    .unwrap()
}

/// Spec (`TSP-1`): devbox-mcp works as a stdio server started by tunnel-client. A turn goes through tunnel-client's
/// real request path and comes back with the agent's reply.
#[tokio::test]
async fn runs_a_turn_through_tunnel_client() {
    let h = Harness::new();
    let proxy = start_proxy(&h).await;
    let client = proxy.connect().await.unwrap();

    let tools = client.list_all_tools().await.unwrap();
    assert!(tools.iter().any(|t| t.name == "run_turn"));
    let result = run_turn(&client, json!({ "prompt": "say hello world" })).await;
    let out = structured(&result);
    assert_eq!(out["completed"], true);
    assert_eq!(out["reply"], "fake reply: say hello world");
    client.cancel().await.unwrap();
}

/// Spec (`LIF-1`): stopping tunnel-client (SIGTERM, as systemd or a user would) shuts devbox-mcp down in order and takes
/// the agent and the commands it started with it. Unlike the stdio shutdown tests, which imitate tunnel-client's stop
/// sequence, this uses tunnel-client's own.
#[tokio::test]
async fn stopping_tunnel_client_stops_devbox_mcp_and_the_agent() {
    let h = Harness::new();
    let mut proxy = start_proxy(&h).await;
    let client = proxy.connect().await.unwrap();

    let peer = client.peer().clone();
    let params = rmcp::model::CallToolRequestParams::new("run_turn")
        .with_arguments(json!({ "prompt": "hang" }).as_object().unwrap().clone());
    let _call = tokio::spawn(async move { peer.call_tool(params).await });
    let mut pids = h.hanging_agent_pids().await;
    // The fake agent's parent is devbox-mcp itself.
    pids.push(h.recorded("ppid").trim().parse().unwrap());

    send_signal(proxy.pid(), libc::SIGTERM);
    proxy.wait(Duration::from_secs(20)).await.unwrap();
    assert_all_gone(
        &pids,
        "devbox-mcp or the agent survived tunnel-client stopping",
    )
    .await;
}
