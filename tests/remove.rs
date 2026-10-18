//! Tests of removing sessions (SPEC.md `SES-6`, SPEC_impl.md "Tools" and "Storage"), against the real devbox-mcp
//! binary over stdio with a fake Codex whose `delete` subcommand records how it was called and can be made to fail or
//! hang (see `support::FAKE_CODEX`).

mod support;

use std::time::{Duration, Instant};

use rmcp::service::{RoleClient, RunningService};
use serde_json::{Value, json};
use support::{
    Harness, call, connect, error_text, eventually, process_running, read, read_until_ended,
    structured,
};

type Client = RunningService<RoleClient, ()>;

/// Creates a session with the default working directory, failing the test if that does not work.
async fn create(client: &Client, name: &str) {
    call(client, "session_create", json!({ "name": name }))
        .await
        .unwrap();
}

/// Sends a message and follows the turn to its end, so the session has a native ID and is idle afterwards.
async fn run_turn(client: &Client, name: &str, message: &str) {
    let sent = structured(
        &call(
            client,
            "session_send",
            json!({ "name": name, "message": message }),
        )
        .await
        .unwrap(),
    );
    read_until_ended(client, name, sent["cursor"].as_str().map(str::to_owned)).await;
}

/// Calls `session_remove` and returns the structured result, failing the test on an error.
async fn remove(client: &Client, name: &str, force: bool) -> Value {
    structured(
        &call(
            client,
            "session_remove",
            json!({ "name": name, "force": force }),
        )
        .await
        .unwrap(),
    )
}

/// The `codex delete` calls the fake recorded, one argument list per call.
fn delete_calls(h: &Harness) -> Vec<Vec<String>> {
    let Ok(text) = std::fs::read_to_string(h.fake_dir.join("delete-argv")) else {
        return Vec::new();
    };
    text.split("\n\n")
        .filter(|call| !call.trim().is_empty())
        .map(|call| call.lines().map(str::to_owned).collect())
        .collect()
}

/// Writes a session record by hand whose first turn was lost before Codex reported a native ID, as happens when
/// devbox-mcp dies in the first moments of a turn.
fn write_record_without_native_id(h: &Harness) {
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
}

/// Spec (`SES-6`): removing an idle session deletes the agent's conversation by its native ID, forgets the session
/// (also across a restart) so its name can be reused, and leaves the working directory and its files alone. Removing a
/// session that does not exist is an error.
#[tokio::test]
async fn removes_a_session_and_its_agent_data() {
    let h = Harness::new();
    let client = connect(&h).await;
    create(&client, "alpha").await;
    run_turn(&client, "alpha", "hello").await;
    let workdir = h.workdir_root.join("alpha");
    std::fs::write(workdir.join("notes.txt"), "keep me").unwrap();

    let removed = remove(&client, "alpha", false).await;
    assert_eq!(removed["agent_data_deleted"], true);
    assert_eq!(removed["session"]["name"], "alpha");
    assert_eq!(removed["session"]["turn"]["state"], "completed");
    assert_eq!(delete_calls(&h), [["delete", "--force", "fake-thread-1"]]);
    assert_eq!(
        std::fs::read_to_string(workdir.join("notes.txt")).unwrap(),
        "keep me"
    );
    let listed = structured(&call(&client, "session_list", json!({})).await.unwrap());
    assert_eq!(listed["sessions"], json!([]));
    let error = error_text(call(&client, "session_remove", json!({ "name": "alpha" })).await);
    assert!(error.contains("no session named"), "{error}");
    client.cancel().await.unwrap();

    let client = connect(&h).await;
    let listed = structured(&call(&client, "session_list", json!({})).await.unwrap());
    assert_eq!(listed["sessions"], json!([]));
    create(&client, "alpha").await;
    client.cancel().await.unwrap();
}

/// Spec (`SES-6`): a session that never had a turn has no agent data, so removing it does not call the agent at all
/// and still reports the data as deleted.
#[tokio::test]
async fn a_session_without_turns_is_removed_without_the_agent() {
    let h = Harness::new();
    let client = connect(&h).await;
    create(&client, "alpha").await;
    let removed = remove(&client, "alpha", false).await;
    assert_eq!(removed["agent_data_deleted"], true);
    assert!(delete_calls(&h).is_empty());
    client.cancel().await.unwrap();
}

/// Spec (`SES-6`): a session with a turn in progress cannot be removed; once the turn has been interrupted and has
/// ended, it can.
#[tokio::test]
async fn refuses_to_remove_a_busy_session() {
    let h = Harness::new();
    let client = connect(&h).await;
    create(&client, "alpha").await;
    call(
        &client,
        "session_send",
        json!({ "name": "alpha", "message": "hang" }),
    )
    .await
    .unwrap();
    let error = error_text(
        call(
            &client,
            "session_remove",
            json!({ "name": "alpha", "force": true }),
        )
        .await,
    );
    assert!(error.contains("busy"), "{error}");
    assert!(delete_calls(&h).is_empty());

    // Interrupting before devbox-mcp has saved the agent's conversation ID would leave a session whose deletion
    // cannot be confirmed, which is a different test.
    let record = h.state_dir.join("sessions/alpha.json");
    assert!(
        eventually(Duration::from_secs(10), || std::fs::read_to_string(&record)
            .is_ok_and(|r| r.contains("fake-thread-1")))
        .await,
        "native ID never saved"
    );
    call(&client, "session_interrupt", json!({ "name": "alpha" }))
        .await
        .unwrap();
    read_until_ended(&client, "alpha", None).await;
    assert_eq!(
        remove(&client, "alpha", false).await["agent_data_deleted"],
        true
    );
    client.cancel().await.unwrap();
}

/// Spec (`SES-6`): when the agent's delete command fails, the session is kept and the error says so, with the
/// agent's own message and the way out; the session stays usable, and `force` then forgets it, reporting that the
/// agent's data may remain.
#[tokio::test]
async fn a_failed_delete_keeps_the_session_unless_forced() {
    let h = Harness::new();
    let client = connect(&h).await;
    create(&client, "alpha").await;
    run_turn(&client, "alpha", "hello").await;
    std::fs::write(h.fake_dir.join("delete-mode"), "fail").unwrap();

    let error = error_text(call(&client, "session_remove", json!({ "name": "alpha" })).await);
    assert!(error.contains("session is kept"), "{error}");
    assert!(error.contains("failed to delete session"), "{error}");
    assert!(error.contains("force"), "{error}");
    let listed = structured(&call(&client, "session_list", json!({})).await.unwrap());
    assert_eq!(listed["sessions"][0]["name"], "alpha");
    run_turn(&client, "alpha", "still here").await;

    let removed = remove(&client, "alpha", true).await;
    assert_eq!(removed["agent_data_deleted"], false);
    assert_eq!(delete_calls(&h).len(), 2);
    let listed = structured(&call(&client, "session_list", json!({})).await.unwrap());
    assert_eq!(listed["sessions"], json!([]));
    client.cancel().await.unwrap();
}

/// Spec (`SES-6`, SPEC_impl.md "Storage"): when devbox-mcp cannot confirm that removal would delete all of the
/// agent's data, it refuses without `force` before deleting anything. That is the case for a session whose turn was
/// lost before the agent reported a native ID, and, after the next send starts a fresh conversation, for the earlier
/// conversation that send left behind, which the record remembers. With `force`, the known conversation is still
/// deleted, but the result says data may remain.
#[tokio::test]
async fn unconfirmable_deletion_needs_force() {
    let h = Harness::new();
    write_record_without_native_id(&h);
    let client = connect(&h).await;

    let error = error_text(call(&client, "session_remove", json!({ "name": "alpha" })).await);
    assert!(error.contains("never reported an ID"), "{error}");
    assert!(delete_calls(&h).is_empty());

    // The next send cannot resume and starts a fresh conversation; the lost one is now untracked.
    let sent = structured(
        &call(
            &client,
            "session_send",
            json!({ "name": "alpha", "message": "hello" }),
        )
        .await
        .unwrap(),
    );
    assert_eq!(sent["context_reset"], true);
    read_until_ended(&client, "alpha", sent["cursor"].as_str().map(str::to_owned)).await;
    client.cancel().await.unwrap();

    // The flag is on disk, so it survives a restart.
    let client = connect(&h).await;
    let error = error_text(call(&client, "session_remove", json!({ "name": "alpha" })).await);
    assert!(error.contains("earlier conversation"), "{error}");
    assert!(delete_calls(&h).is_empty());

    let removed = remove(&client, "alpha", true).await;
    assert_eq!(removed["agent_data_deleted"], false);
    assert_eq!(delete_calls(&h), [["delete", "--force", "fake-thread-1"]]);
    client.cancel().await.unwrap();
}

/// Spec (SPEC_impl.md "Storage"): a record written before devbox-mcp tracked lost conversations cannot say whether a
/// context reset ever left one behind. Past its first turn it is treated as possibly having one, so removal needs
/// `force`; a record still at its first turn cannot have had a reset and is removed normally.
#[tokio::test]
async fn records_from_before_tracking_are_treated_with_caution() {
    let h = Harness::new();
    let sessions = h.state_dir.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    for (name, turn_id) in [("first", 1), ("later", 2)] {
        let workdir = h.tmp.path().join(name);
        std::fs::create_dir_all(&workdir).unwrap();
        let record = json!({
            "name": name, "agent": "codex", "workdir": workdir, "model": "m", "effort": "high",
            "native_id": "fake-thread-1",
            "turn": { "id": turn_id, "state": "completed", "started_at": "2026-10-11T10:00:00Z",
                      "ended_at": "2026-10-11T10:01:00Z", "final_message": "ok", "error": null }
        });
        std::fs::write(sessions.join(format!("{name}.json")), record.to_string()).unwrap();
    }
    let client = connect(&h).await;

    assert_eq!(
        remove(&client, "first", false).await["agent_data_deleted"],
        true
    );
    let error = error_text(call(&client, "session_remove", json!({ "name": "later" })).await);
    assert!(error.contains("predates"), "{error}");
    assert_eq!(delete_calls(&h).len(), 1);
    assert_eq!(
        remove(&client, "later", true).await["agent_data_deleted"],
        false
    );
    client.cancel().await.unwrap();
}

/// Spec (SPEC_impl.md cursors): a cursor from a removed session, used on a new session created under the same name in
/// the same process, is treated like a cursor from another process: a gap and the new session's events from the start
/// of its turn, never a silent skip of events whose sequence numbers happen to match.
#[tokio::test]
async fn an_old_cursor_does_not_read_a_recreated_session() {
    let h = Harness::new();
    let client = connect(&h).await;
    create(&client, "alpha").await;
    let sent = structured(
        &call(
            &client,
            "session_send",
            json!({ "name": "alpha", "message": "first" }),
        )
        .await
        .unwrap(),
    );
    let (_, last) =
        read_until_ended(&client, "alpha", sent["cursor"].as_str().map(str::to_owned)).await;
    let old_cursor = last["next_cursor"].as_str().unwrap().to_owned();
    remove(&client, "alpha", false).await;

    create(&client, "alpha").await;
    run_turn(&client, "alpha", "second").await;
    let read = read(&client, "alpha", Some(&old_cursor)).await;
    assert_eq!(read["gap"], true);
    assert_eq!(read["events"][0]["type"], "turn_started");
    assert_eq!(read["events"][0]["message"], "second");
    client.cancel().await.unwrap();
}

/// Spec (`SES-6`, SPEC_impl.md "Tools"): while removal waits for the agent's delete command, new sends and a second
/// removal fail as busy, but a send repeating a known request ID is still replayed. When the delete command does not
/// finish within 10 seconds it is killed and reaped, the removal fails, and the session is kept and usable again.
///
/// This is the only test that waits out the real timeout; the kill and reap themselves are also unit-tested in
/// `codex.rs` with a short one.
/// Spec (SPEC_impl.md "Codex"): nothing `codex delete` started may keep running once the removal returns, or it could
/// delete data after the session is back in use. Here the command exits successfully at once while a worker it started
/// in its own process group is still running, the shape of an npm launcher whose deleting child outlives it. A
/// regression: deletion used to wait for the launcher alone, report success, and leave the worker running.
#[tokio::test]
async fn removal_stops_what_codex_delete_left_running() {
    let h = Harness::new();
    let client = connect(&h).await;
    create(&client, "alpha").await;
    run_turn(&client, "alpha", "hello").await;
    std::fs::write(h.fake_dir.join("delete-mode"), "worker").unwrap();
    let removed = remove(&client, "alpha", false).await;
    assert_eq!(removed["agent_data_deleted"], true);
    let worker: i32 = std::fs::read_to_string(h.fake_dir.join("delete-worker"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(
        eventually(Duration::from_secs(5), || !process_running(worker)).await,
        "the worker codex delete started is still running"
    );
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn a_removal_in_progress_blocks_new_work_until_it_ends() {
    let h = Harness::new();
    let client = connect(&h).await;
    create(&client, "alpha").await;
    let replay = json!({ "name": "alpha", "message": "hello", "request_id": "r-1" });
    let sent = structured(&call(&client, "session_send", replay.clone()).await.unwrap());
    read_until_ended(&client, "alpha", sent["cursor"].as_str().map(str::to_owned)).await;
    std::fs::write(h.fake_dir.join("delete-mode"), "hang").unwrap();

    let started = Instant::now();
    let removal = call(&client, "session_remove", json!({ "name": "alpha" }));
    let during = async {
        let pid_file = h.fake_dir.join("delete-pid");
        assert!(
            eventually(Duration::from_secs(5), || pid_file.exists()).await,
            "delete never started"
        );
        let replayed = structured(&call(&client, "session_send", replay.clone()).await.unwrap());
        assert_eq!(replayed["replayed"], true);
        let error = error_text(
            call(
                &client,
                "session_send",
                json!({ "name": "alpha", "message": "new work" }),
            )
            .await,
        );
        assert!(error.contains("being removed"), "{error}");
        let error = error_text(call(&client, "session_remove", json!({ "name": "alpha" })).await);
        assert!(error.contains("being removed"), "{error}");
        std::fs::read_to_string(pid_file)
            .unwrap()
            .trim()
            .parse::<i32>()
            .unwrap()
    };
    let (removal, delete_pid) = tokio::join!(removal, during);
    let error = error_text(removal);
    assert!(error.contains("did not finish"), "{error}");
    assert!(error.contains("session is kept"), "{error}");
    assert!(started.elapsed() < Duration::from_secs(20));
    assert!(!process_running(delete_pid), "delete command left running");

    run_turn(&client, "alpha", "usable again").await;
    client.cancel().await.unwrap();
}
