//! Tests of turns: sending messages, following them with `session_read`, interrupting them, retries, and what survives
//! a restart (SPEC.md "Turns", SPEC_impl.md "Turn lifecycle", "Tools"), against the real devbox-mcp binary over stdio
//! with a fake Codex.

mod support;

use std::time::{Duration, Instant};

use rmcp::service::{RoleClient, RunningService};
use serde_json::{Value, json};
use support::{
    Harness, assert_all_gone, call, connect, error_text, eventually, read, read_until_ended,
    structured,
};

type Client = RunningService<RoleClient, ()>;

/// Creates a session with the default working directory, failing the test if that does not work.
async fn create(client: &Client, name: &str) {
    call(client, "session_create", json!({ "name": name }))
        .await
        .unwrap();
}

/// Sends a message without a request ID and returns the structured result (`turn_id`, `cursor`, and so on).
async fn send(client: &Client, name: &str, message: &str) -> Value {
    structured(
        &call(
            client,
            "session_send",
            json!({ "name": name, "message": message }),
        )
        .await
        .unwrap(),
    )
}

/// The `type` of each event, in order: what most assertions about an event stream compare.
fn types(events: &[Value]) -> Vec<&str> {
    events.iter().map(|e| e["type"].as_str().unwrap()).collect()
}

/// Spec (`TRN-1`, `TRN-3`, `SES-8`): a send returns at once with a turn ID and a cursor; reading from that cursor
/// yields the turn's events in order, ending with `turn_ended`; the final state and reply are in `turn`.
#[tokio::test]
async fn a_turn_runs_in_the_background_and_is_read_incrementally() {
    let h = Harness::new();
    let client = connect(&h).await;
    create(&client, "alpha").await;

    let sent = send(&client, "alpha", "say hello").await;
    assert_eq!(sent["turn_id"], 1);
    assert_eq!(sent["replayed"], false);
    assert_eq!(sent["context_reset"], false);
    let (events, last) =
        read_until_ended(&client, "alpha", sent["cursor"].as_str().map(str::to_owned)).await;
    assert_eq!(
        types(&events),
        ["turn_started", "agent_message", "turn_ended"]
    );
    assert_eq!(events[0]["message"], "say hello");
    assert_eq!(events[2]["state"], "completed");
    assert!(events.iter().all(|e| e["turn_id"] == 1));
    assert_eq!(last["turn"]["state"], "completed");
    assert_eq!(last["turn"]["final_message"], "fake reply: say hello");
    assert_eq!(last["liveness"]["agent_alive"], Value::Null);

    // Nothing new after the end: an empty batch, and the cursor stays put.
    let again = read(&client, "alpha", last["next_cursor"].as_str()).await;
    assert_eq!(again["events"], json!([]));
    assert_eq!(again["next_cursor"], last["next_cursor"]);
    client.cancel().await.unwrap();
}

/// Spec (`SES-5`, `TRA-1`): the second turn continues the agent's own conversation by its native ID, which is what
/// makes a session more than a string of one-off invocations.
#[tokio::test]
async fn later_turns_resume_the_agent_conversation() {
    let h = Harness::new();
    let client = connect(&h).await;
    create(&client, "alpha").await;
    let sent = send(&client, "alpha", "first").await;
    read_until_ended(&client, "alpha", sent["cursor"].as_str().map(str::to_owned)).await;

    let sent = send(&client, "alpha", "second").await;
    assert_eq!(sent["turn_id"], 2);
    let (events, last) =
        read_until_ended(&client, "alpha", sent["cursor"].as_str().map(str::to_owned)).await;
    assert!(
        events.iter().all(|e| e["turn_id"] == 2),
        "previous turn's events leaked: {events:?}"
    );
    assert_eq!(last["turn"]["state"], "completed");
    let argv = h.recorded("argv");
    let args: Vec<&str> = argv.lines().collect();
    assert_eq!(args[1], "resume", "{argv}");
    assert!(args.contains(&"fake-thread-1"), "{argv}");
    client.cancel().await.unwrap();
}

/// Spec (`TRN-2`, `TRN-4`, `LIF-1`): a send to a busy session is rejected with an error that says what to do; an
/// interrupt returns at once with `interrupting`, the turn then ends `interrupted`, and the agent's whole process tree is
/// gone.
#[tokio::test]
async fn busy_sessions_reject_sends_and_interrupts_stop_the_agent() {
    let h = Harness::new();
    let client = connect(&h).await;
    create(&client, "alpha").await;
    send(&client, "alpha", "hang").await;
    let pids = h.hanging_agent_pids().await;

    let err = error_text(
        call(
            &client,
            "session_send",
            json!({ "name": "alpha", "message": "x" }),
        )
        .await,
    );
    assert!(
        err.contains("busy") && err.contains("session_interrupt"),
        "{err}"
    );

    let read_live = read(&client, "alpha", None).await;
    assert_eq!(read_live["liveness"]["agent_alive"], true);
    assert_eq!(read_live["turn"]["state"], "running");

    let started = Instant::now();
    let turn = structured(
        &call(&client, "session_interrupt", json!({ "name": "alpha" }))
            .await
            .unwrap(),
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "interrupt waited for the agent"
    );
    assert_eq!(turn["state"], "interrupting");

    let (_, last) = read_until_ended(&client, "alpha", None).await;
    assert_eq!(last["turn"]["state"], "interrupted");
    assert_eq!(last["turn"]["cleanup_uncertain"], false);
    assert_all_gone(&pids, "agent survived an interrupt").await;

    // Interrupting again changes nothing and reports the turn as it is.
    let turn = structured(
        &call(&client, "session_interrupt", json!({ "name": "alpha" }))
            .await
            .unwrap(),
    );
    assert_eq!(turn["state"], "interrupted");
    client.cancel().await.unwrap();
}

/// Spec (`TRN-6`): a retried send with the same request ID returns the original turn, even while it is still running
/// (the retry check comes before the busy check), and starts nothing; the same ID with a different message is
/// rejected.
#[tokio::test]
async fn retried_sends_return_the_original_turn() {
    let h = Harness::new();
    let client = connect(&h).await;
    create(&client, "alpha").await;
    let args = json!({ "name": "alpha", "message": "hang", "request_id": "r-1" });
    let first = structured(&call(&client, "session_send", args.clone()).await.unwrap());
    h.hanging_agent_pids().await;
    let retry = structured(&call(&client, "session_send", args).await.unwrap());
    assert_eq!(retry["turn_id"], first["turn_id"]);
    assert_eq!(retry["replayed"], true);
    assert_eq!(retry["cursor"], first["cursor"]);

    let err = error_text(
        call(
            &client,
            "session_send",
            json!({ "name": "alpha", "message": "other", "request_id": "r-1" }),
        )
        .await,
    );
    assert!(err.contains("different message"), "{err}");
    call(&client, "session_interrupt", json!({ "name": "alpha" }))
        .await
        .unwrap();
    read_until_ended(&client, "alpha", None).await;
    client.cancel().await.unwrap();
}

/// Spec (`TRN-6`, `SES-5`, `SES-8`, `TRN-5`): after a restart, request IDs are still recognized, the turn that was
/// running is reported as lost, a read reports a gap rather than pretending nothing happened, and the next turn resumes
/// the agent's conversation.
#[tokio::test]
async fn restarts_report_lost_turns_and_keep_request_ids() {
    let h = Harness::new();
    let client = connect(&h).await;
    create(&client, "alpha").await;
    let sent = structured(
        &call(
            &client,
            "session_send",
            json!({ "name": "alpha", "message": "hang", "request_id": "r-1" }),
        )
        .await
        .unwrap(),
    );
    let pids = h.hanging_agent_pids().await;
    client.cancel().await.unwrap();
    assert_all_gone(&pids, "agent survived devbox-mcp stopping").await;

    let client = connect(&h).await;
    let after = read(&client, "alpha", sent["cursor"].as_str()).await;
    assert_eq!(after["turn"]["state"], "lost");
    assert_eq!(after["gap"], true);
    assert_eq!(after["events"], json!([]));
    assert_eq!(after["liveness"]["turn_duration_seconds"], Value::Null);

    let retry = structured(
        &call(
            &client,
            "session_send",
            json!({ "name": "alpha", "message": "hang", "request_id": "r-1" }),
        )
        .await
        .unwrap(),
    );
    assert_eq!(retry["replayed"], true);
    assert_eq!(retry["cursor"], Value::Null);

    let sent = send(&client, "alpha", "after restart").await;
    assert_eq!(sent["turn_id"], 2);
    assert_eq!(sent["context_reset"], false);
    read_until_ended(&client, "alpha", sent["cursor"].as_str().map(str::to_owned)).await;
    assert!(h.recorded("argv").lines().any(|a| a == "resume"));
    client.cancel().await.unwrap();
}

/// Spec (`SES-5`): when the first turn was lost before the agent reported its session ID, the conversation cannot be
/// resumed; the next turn starts fresh and says so instead of silently pretending to continue.
#[tokio::test]
async fn a_lost_first_turn_without_native_id_resets_context() {
    let h = Harness::new();
    let sessions = h.state_dir.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let workdir = h.tmp.path().join("w");
    std::fs::create_dir_all(&workdir).unwrap();
    let record = json!({
        "name": "alpha", "agent": "codex", "workdir": workdir, "model": "m", "effort": "high", "native_id": null,
        "turn": { "id": 1, "state": "running", "started_at": "2026-10-11T10:00:00Z", "ended_at": null,
                  "final_message": null, "error": null }
    });
    std::fs::write(sessions.join("alpha.json"), record.to_string()).unwrap();

    let client = connect(&h).await;
    let args = json!({ "name": "alpha", "message": "hello", "request_id": "r-1" });
    let sent = structured(&call(&client, "session_send", args.clone()).await.unwrap());
    assert_eq!(sent["context_reset"], true);
    read_until_ended(&client, "alpha", sent["cursor"].as_str().map(str::to_owned)).await;
    assert!(!h.recorded("argv").lines().any(|a| a == "resume"));

    // A retry whose first response was lost must still warn about the reset, although the fresh conversation now has a
    // native ID.
    let retried = structured(&call(&client, "session_send", args).await.unwrap());
    assert_eq!(retried["replayed"], true);
    assert_eq!(retried["context_reset"], true);
    client.cancel().await.unwrap();
}

/// Spec (`TRN-3`): cursors are tied to their session; a malformed one or another session's is an error, and a cursor
/// from an earlier devbox-mcp gives a gap.
#[tokio::test]
async fn cursors_are_checked() {
    let h = Harness::new();
    let client = connect(&h).await;
    create(&client, "alpha").await;
    create(&client, "beta").await;
    let sent = send(&client, "alpha", "hi").await;
    read_until_ended(&client, "alpha", None).await;

    let err = error_text(
        call(
            &client,
            "session_read",
            json!({ "name": "alpha", "cursor": "garbage" }),
        )
        .await,
    );
    assert!(err.contains("malformed cursor"), "{err}");
    let err = error_text(
        call(
            &client,
            "session_read",
            json!({ "name": "beta", "cursor": sent["cursor"] }),
        )
        .await,
    );
    assert!(err.contains("belongs to session"), "{err}");
    let stale = read(&client, "alpha", Some("alpha:earlier-process:3")).await;
    assert_eq!(stale["gap"], true);
    assert_eq!(
        types(stale["events"].as_array().unwrap()),
        ["turn_started", "agent_message", "turn_ended"]
    );
    client.cancel().await.unwrap();
}

/// Spec (`BND-1`): a read returns at most 50 events and says when more are waiting; paging with `next_cursor` gets the
/// rest in order without gaps.
#[tokio::test]
async fn reads_are_paged() {
    let h = Harness::new();
    let client = connect(&h).await;
    create(&client, "alpha").await;
    let sent = send(&client, "alpha", "many").await;
    let (events, _) =
        read_until_ended(&client, "alpha", sent["cursor"].as_str().map(str::to_owned)).await;
    // turn_started, 60 messages, the reply, turn_ended.
    assert_eq!(events.len(), 63);
    let seqs: Vec<u64> = events.iter().map(|e| e["seq"].as_u64().unwrap()).collect();
    assert!(seqs.windows(2).all(|w| w[1] == w[0] + 1), "{seqs:?}");

    let first_page = read(&client, "alpha", sent["cursor"].as_str()).await;
    assert_eq!(first_page["events"].as_array().unwrap().len(), 50);
    assert_eq!(first_page["more"], true);
    client.cancel().await.unwrap();
}

/// Spec (`BND-1`): a failure reason longer than devbox-mcp keeps is cut, and says so both in its own text (which is all
/// that survives a restart) and in the `turn_ended` event's `truncated` flag.
#[tokio::test]
async fn long_failure_reasons_are_marked_as_cut() {
    let h = Harness::new();
    let client = connect(&h).await;
    create(&client, "alpha").await;
    send(&client, "alpha", "fail-long").await;
    let (events, last) = read_until_ended(&client, "alpha", None).await;
    let error = last["turn"]["error"].as_str().unwrap();
    assert!(error.len() <= 4096, "{} bytes", error.len());
    assert!(error.ends_with(" [cut]"), "{error}");
    let ended = events.last().unwrap();
    assert_eq!(ended["type"], "turn_ended");
    assert_eq!(ended["truncated"], true);
    assert_eq!(ended["error"], error);
    client.cancel().await.unwrap();
}

/// Spec (SPEC_impl.md "Storage"): a record write that fails after the turn started is reported to the caller as a
/// non-fatal `error` event before `turn_ended`, instead of only on devbox-mcp's stderr, because it changes what a restart
/// would show. The turn keeps its real outcome.
#[tokio::test]
async fn failed_record_writes_during_a_turn_are_reported() {
    let h = Harness::new();
    let client = connect(&h).await;
    create(&client, "alpha").await;
    send(&client, "alpha", "hang").await;
    let record = h.state_dir.join("sessions/alpha.json");
    assert!(
        eventually(Duration::from_secs(10), || std::fs::read_to_string(&record)
            .is_ok_and(|r| r.contains("fake-thread-1")))
        .await,
        "native ID never saved"
    );
    // The store writes through a fixed temporary name; a directory there makes every later write fail.
    std::fs::create_dir(h.state_dir.join("sessions/alpha.json.tmp")).unwrap();

    call(&client, "session_interrupt", json!({ "name": "alpha" }))
        .await
        .unwrap();
    let (events, last) = read_until_ended(&client, "alpha", None).await;
    assert_eq!(last["turn"]["state"], "interrupted");
    let n = events.len();
    assert_eq!(events[n - 1]["type"], "turn_ended");
    assert_eq!(events[n - 2]["type"], "error");
    assert_eq!(events[n - 2]["fatal"], false);
    let message = events[n - 2]["message"].as_str().unwrap();
    assert!(message.contains("could not save"), "{message}");
    client.cancel().await.unwrap();
}

/// Spec (`BND-1`, `DOC-3`): errors about invalid arguments quote only a bounded preview of the offending value, so an
/// oversized argument cannot produce an oversized response, and the advice after it survives.
#[tokio::test]
async fn errors_do_not_echo_large_arguments() {
    let h = Harness::new();
    let client = connect(&h).await;
    create(&client, "alpha").await;
    let huge = "x".repeat(1 << 20);
    for (tool, args) in [
        (
            "session_send",
            json!({ "name": "alpha", "message": "hi", "request_id": huge }),
        ),
        ("session_read", json!({ "name": huge })),
        ("session_read", json!({ "name": "alpha", "cursor": huge })),
        ("session_create", json!({ "name": huge })),
        ("session_create", json!({ "name": "beta", "workdir": huge })),
    ] {
        let error = error_text(call(&client, tool, args).await);
        assert!(error.len() < 1024, "{tool}: {} bytes", error.len());
    }
    client.cancel().await.unwrap();
}

/// Spec (`SES-8`, `LIF-2`): a failed turn keeps its reason, also after a restart.
#[tokio::test]
async fn failure_reasons_survive_a_restart() {
    let h = Harness::new();
    let client = connect(&h).await;
    create(&client, "alpha").await;
    send(&client, "alpha", "fail now").await;
    let (_, last) = read_until_ended(&client, "alpha", None).await;
    assert_eq!(last["turn"]["state"], "failed");
    assert_eq!(last["turn"]["error"], "fake failure");
    client.cancel().await.unwrap();

    let client = connect(&h).await;
    let listed = structured(&call(&client, "session_list", json!({})).await.unwrap());
    assert_eq!(listed["sessions"][0]["turn"]["state"], "failed");
    assert_eq!(listed["sessions"][0]["turn"]["error"], "fake failure");
    client.cancel().await.unwrap();
}
