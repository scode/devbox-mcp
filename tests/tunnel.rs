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
use support::{
    DEVBOX_MCP, Harness, assert_all_gone, call, hermetic_env, run_session_turn, send_signal,
    structured,
};

/// Starts devbox-mcp on this harness behind the pinned tunnel-client.
async fn start_proxy(h: &Harness) -> DevProxy {
    DevProxy::start(
        &default_tunnel_client(),
        Path::new(DEVBOX_MCP),
        &h.devbox_args(),
        h.tmp.path(),
        Duration::from_secs(30),
        &hermetic_env(h.tmp.path()),
    )
    .await
    .unwrap()
}

/// Spec (`TSP-1`): devbox-mcp works as a stdio server started by tunnel-client. A whole session flow (create, send,
/// read until the end) goes through tunnel-client's real request path and comes back with the agent's reply.
#[tokio::test]
async fn runs_a_turn_through_tunnel_client() {
    let h = Harness::new();
    let proxy = start_proxy(&h).await;
    let client = proxy.connect().await.unwrap();

    let (_, last) = run_session_turn(&client, "alpha", "say hello world").await;
    assert_eq!(last["turn"]["state"], "completed");
    assert_eq!(last["turn"]["final_message"], "fake reply: say hello world");
    client.cancel().await.unwrap();
}

/// Spec (`BND-2`, `TRN-1`): tunnel-client forwards calls to devbox-mcp one at a time, so a call that waited for an
/// agent would hold up every other call. With a turn running indefinitely, other calls (here a listing, a read, and
/// an interrupt) still come back promptly through the real tunnel-client.
#[tokio::test]
async fn a_running_turn_does_not_hold_up_other_calls() {
    let h = Harness::new();
    let proxy = start_proxy(&h).await;
    let client = proxy.connect().await.unwrap();
    call(&client, "session_create", json!({ "name": "alpha" }))
        .await
        .unwrap();
    call(
        &client,
        "session_send",
        json!({ "name": "alpha", "message": "hang" }),
    )
    .await
    .unwrap();
    let pids = h.hanging_agent_pids().await;

    for (tool, args) in [
        ("session_list", json!({})),
        ("session_read", json!({ "name": "alpha" })),
        ("session_interrupt", json!({ "name": "alpha" })),
    ] {
        let started = std::time::Instant::now();
        let result = call(&client, tool, args).await.unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{tool} took {:?} while a turn was running",
            started.elapsed()
        );
        assert_ne!(structured(&result), serde_json::Value::Null);
    }
    assert_all_gone(&pids, "agent survived an interrupt through tunnel-client").await;
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

    call(&client, "session_create", json!({ "name": "alpha" }))
        .await
        .unwrap();
    call(
        &client,
        "session_send",
        json!({ "name": "alpha", "message": "hang" }),
    )
    .await
    .unwrap();
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
