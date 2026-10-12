//! Tests of `session_transcript` (SPEC.md `TRA-2`, SPEC_impl.md "Tools"), against the real devbox-mcp binary over
//! stdio. The test's devbox-mcp runs with a temporary `HOME`, so Codex's data directory is `<tmp>/.codex`, and the
//! tests write rollout files there by hand, the way Codex lays them out; the fake Codex itself writes none. The paging
//! and bounds rules are unit-tested in `src/transcript.rs`; these tests cover finding the file and the tool's contract.

mod support;

use std::io::Write as _;
use std::path::PathBuf;

use rmcp::service::{RoleClient, RunningService};
use serde_json::{Value, json};
use support::{Harness, call, call_checked, connect, error_text, read_until_ended, structured};

type Client = RunningService<RoleClient, ()>;

/// Creates a session and runs one turn, so it has the fake Codex's native ID, `fake-thread-1`.
async fn session_with_a_turn(client: &Client, name: &str) {
    call(client, "session_create", json!({ "name": name }))
        .await
        .unwrap();
    let sent = structured(
        &call(
            client,
            "session_send",
            json!({ "name": name, "message": "hello" }),
        )
        .await
        .unwrap(),
    );
    read_until_ended(client, name, sent["cursor"].as_str().map(str::to_owned)).await;
}

/// Writes a rollout for `fake-thread-1` with `n` entries where Codex would keep it, and returns its path.
fn write_rollout(h: &Harness, n: u64, day: &str) -> PathBuf {
    let dir = h.tmp.path().join(".codex/sessions/2026/10").join(day);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("rollout-2026-10-11T10-00-00-fake-thread-1.jsonl");
    let lines: String = (0..n)
        .map(|i| format!("{}\n", json!({ "type": "event_msg", "n": i })))
        .collect();
    std::fs::write(&path, lines).unwrap();
    path
}

/// Reads a transcript page, checking it against the tool's declared output schema.
async fn transcript(client: &Client, args: Value) -> Value {
    call_checked(client, "session_transcript", args).await
}

/// Spec (`TRA-2`): without `from` a read returns the last entries of the agent's own transcript, so the end needs no
/// knowledge of the length; with `from` it pages forwards and says where to continue. Entries are the agent's own JSON.
#[tokio::test]
async fn reads_a_codex_transcript_from_the_end_and_forwards() {
    let h = Harness::new();
    let client = connect(&h).await;
    session_with_a_turn(&client, "alpha").await;
    let path = write_rollout(&h, 30, "11");

    let tail = transcript(&client, json!({ "name": "alpha" })).await;
    assert_eq!(tail["total"], 30);
    let entries = tail["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 20);
    assert_eq!(entries[0]["index"], 10);
    assert_eq!(entries[19]["index"], 29);
    assert_eq!(
        entries[19]["entry"],
        json!({ "type": "event_msg", "n": 29 })
    );
    assert_eq!(tail["next_from"], Value::Null);

    let page = transcript(&client, json!({ "name": "alpha", "from": 0, "limit": 5 })).await;
    assert_eq!(page["entries"].as_array().unwrap().len(), 5);
    assert_eq!(page["next_from"], 5);

    // An entry too large to return as JSON next to ordinary ones, so the page holds both shapes of an entry, with
    // `truncated` and without, and both must match the declared schema.
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    writeln!(file, "{}", json!({ "huge": vec![1; 40_000] })).unwrap();
    let tail = transcript(&client, json!({ "name": "alpha", "limit": 2 })).await;
    assert_eq!(tail["entries"][1]["truncated"], true);
    assert!(tail["entries"][0].get("truncated").is_none());
    client.cancel().await.unwrap();
}

/// Spec (`TRA-2`): when there is no transcript to read, the caller is told why, never given an empty one: a session
/// with nothing sent yet, a rollout that cannot be found, and two candidate rollouts are errors.
#[tokio::test]
async fn reports_a_missing_or_ambiguous_transcript() {
    let h = Harness::new();
    let client = connect(&h).await;
    call(&client, "session_create", json!({ "name": "fresh" }))
        .await
        .unwrap();
    let error = error_text(call(&client, "session_transcript", json!({ "name": "fresh" })).await);
    assert!(error.contains("nothing has been sent"), "{error}");

    session_with_a_turn(&client, "alpha").await;
    let error = error_text(call(&client, "session_transcript", json!({ "name": "alpha" })).await);
    assert!(error.contains("was not found"), "{error}");
    assert!(error.contains("session_send"), "{error}");

    write_rollout(&h, 3, "11");
    write_rollout(&h, 3, "12");
    let error = error_text(call(&client, "session_transcript", json!({ "name": "alpha" })).await);
    assert!(error.contains("found 2 files"), "{error}");
    client.cancel().await.unwrap();
}

/// Spec (`DOC-3`): a transcript asked for while the first turn is starting, before the agent has reported its
/// conversation, is not there yet, and the caller is told to follow the turn and retry, not that the turn ended.
#[tokio::test]
async fn a_starting_turn_has_no_transcript_yet() {
    let h = Harness::new();
    let client = connect(&h).await;
    std::fs::write(h.fake_dir.join("hold-start"), "").unwrap();
    call(&client, "session_create", json!({ "name": "alpha" }))
        .await
        .unwrap();
    let sent = structured(
        &call(
            &client,
            "session_send",
            json!({ "name": "alpha", "message": "hello" }),
        )
        .await
        .unwrap(),
    );
    let error = error_text(call(&client, "session_transcript", json!({ "name": "alpha" })).await);
    assert!(
        error.contains("not reported its conversation ID"),
        "{error}"
    );
    assert!(error.contains("session_read"), "{error}");
    std::fs::remove_file(h.fake_dir.join("hold-start")).unwrap();
    read_until_ended(&client, "alpha", sent["cursor"].as_str().map(str::to_owned)).await;
    client.cancel().await.unwrap();
}

/// Spec (SPEC_impl.md "Configuration"): a relative `CODEX_HOME` makes devbox-mcp refuse to start, naming the variable.
/// Codex resolves it against each session's working directory, so devbox-mcp could not tell where a session's
/// transcript is; starting anyway would make every transcript read search the wrong place.
#[tokio::test]
async fn refuses_a_relative_agent_home() {
    let h = Harness::new();
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::process::Command::new(support::DEVBOX_MCP)
            .env_clear()
            .envs(support::hermetic_env(h.tmp.path()))
            .env("CODEX_HOME", "agent-data")
            .args(h.devbox_args())
            .stdin(std::process::Stdio::null())
            .output(),
    )
    .await
    .expect("devbox-mcp did not exit")
    .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("CODEX_HOME is the relative path agent-data"),
        "{stderr}"
    );
}

/// Spec (SPEC_impl.md "Tools"): `limit` is 1 to 50, and anything else is rejected with the allowed range.
#[tokio::test]
async fn checks_the_limit() {
    let h = Harness::new();
    let client = connect(&h).await;
    session_with_a_turn(&client, "alpha").await;
    write_rollout(&h, 3, "11");
    for limit in [0, 51] {
        let error = error_text(
            call(
                &client,
                "session_transcript",
                json!({ "name": "alpha", "limit": limit }),
            )
            .await,
        );
        assert!(error.contains("limit must be 1 to 50"), "{error}");
    }
    client.cancel().await.unwrap();
}
