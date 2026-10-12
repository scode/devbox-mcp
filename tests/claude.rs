//! Tests of Claude sessions (SPEC.md `SES-5` to `SES-7`, `TRA-2`; SPEC_impl.md "Claude"), against the real devbox-mcp
//! binary over stdio with the fake Claude from `support`. devbox-mcp runs with a temporary `HOME`, so Claude's
//! configuration directory is `<tmp>/.claude`, where the fake keeps its per-session files the way Claude Code does.
//! The stream mapping is unit-tested in `src/claude.rs`; these tests cover the session lifecycle around it: the chosen
//! conversation ID, resuming and restarting it, rejected efforts, removal, and transcripts.

mod support;

use std::path::{Path, PathBuf};

use rmcp::service::{RoleClient, RunningService};
use serde_json::{Value, json};
use support::{Harness, call, call_checked, connect, error_text, read_until_ended, structured};

type Client = RunningService<RoleClient, ()>;

/// Creates a Claude session with the given extra arguments and returns its summary.
async fn create(client: &Client, name: &str, extra: Value) -> Value {
    let mut args = json!({ "name": name, "agent": "claude" });
    args.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    structured(&call(client, "session_create", args).await.unwrap())
}

/// Sends a message and follows the turn to its end; returns the send's result, the turn's events, and the final read.
async fn send(client: &Client, name: &str, message: &str) -> (Value, Vec<Value>, Value) {
    let sent = structured(
        &call(
            client,
            "session_send",
            json!({ "name": name, "message": message }),
        )
        .await
        .unwrap(),
    );
    let (events, last) =
        read_until_ended(client, name, sent["cursor"].as_str().map(str::to_owned)).await;
    (sent, events, last)
}

/// The conversation ID the fake Claude was last started with, and whether it was asked to resume.
fn last_conversation(h: &Harness) -> (String, bool) {
    let argv: Vec<String> = h
        .recorded("claude-argv")
        .lines()
        .map(str::to_owned)
        .collect();
    for (i, arg) in argv.iter().enumerate() {
        match arg.as_str() {
            "--session-id" => return (argv[i + 1].clone(), false),
            "--resume" => return (argv[i + 1].clone(), true),
            _ => {}
        }
    }
    panic!("no conversation ID in {argv:?}");
}

/// Claude's configuration directory as devbox-mcp under test sees it.
fn claude_home(h: &Harness) -> PathBuf {
    h.tmp.path().join(".claude")
}

/// Every path under `root` whose name contains `id`.
fn named_with(root: &Path, id: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_owned()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries {
            let path = entry.unwrap().path();
            if path.file_name().unwrap().to_string_lossy().contains(id) {
                found.push(path.clone());
            }
            if path.is_dir() {
                pending.push(path);
            }
        }
    }
    found
}

/// Spec (SPEC_impl.md "Claude", "Tools"): Claude sessions default to opus at high effort; an effort Claude's CLI does
/// not accept is rejected at creation, since Claude would ignore it with only a warning (`SES-7`), while the same value
/// is accepted for Codex, which rejects it itself when a turn runs.
#[tokio::test]
async fn creates_claude_sessions_and_checks_the_effort() {
    let h = Harness::new();
    let client = connect(&h).await;
    let summary = create(&client, "alpha", json!({})).await;
    assert_eq!(summary["agent"], "claude");
    assert_eq!(summary["model"], "opus");
    assert_eq!(summary["effort"], "high");
    let summary = create(
        &client,
        "beta",
        json!({ "model": "sonnet", "effort": "xhigh" }),
    )
    .await;
    assert_eq!(summary["effort"], "xhigh");

    let error = error_text(
        call(
            &client,
            "session_create",
            json!({ "name": "gamma", "agent": "claude", "effort": "turbo" }),
        )
        .await,
    );
    assert!(error.contains("low, medium, high, xhigh, max"), "{error}");
    let codex = structured(
        &call(
            &client,
            "session_create",
            json!({ "name": "gamma", "effort": "turbo" }),
        )
        .await
        .unwrap(),
    );
    assert_eq!(codex["agent"], "codex");
    client.cancel().await.unwrap();
}

/// Spec (`SES-5`, SPEC_impl.md "Claude"): the first turn starts a conversation under an ID devbox-mcp chose, with the
/// session's model and effort, the message on stdin, and permission checks bypassed (`SEC-4`); the next turn resumes
/// that same conversation. Claude's stream becomes devbox-mcp's events and its `result` the final message.
#[tokio::test]
async fn runs_and_resumes_a_conversation() {
    let h = Harness::new();
    let client = connect(&h).await;
    let summary = create(
        &client,
        "alpha",
        json!({ "model": "sonnet", "effort": "low" }),
    )
    .await;

    let (sent, events, last) = send(&client, "alpha", "hello").await;
    assert_eq!(sent["context_reset"], false);
    assert_eq!(last["turn"]["state"], "completed");
    assert_eq!(last["turn"]["final_message"], "fake claude reply: hello");
    let types: Vec<&str> = events.iter().map(|e| e["type"].as_str().unwrap()).collect();
    assert_eq!(
        types,
        [
            "turn_started",
            "agent_message",
            "command_started",
            "command_finished",
            "turn_ended"
        ]
    );
    let argv = h.recorded("claude-argv");
    for expected in [
        "-p",
        "stream-json",
        "--dangerously-skip-permissions",
        "sonnet",
        "low",
    ] {
        assert!(
            argv.lines().any(|a| a == expected),
            "{expected} not in {argv}"
        );
    }
    assert_eq!(h.recorded("claude-stdin"), "hello");
    assert_eq!(
        h.recorded("claude-cwd").trim_end(),
        summary["workdir"].as_str().unwrap()
    );
    let (first_id, resumed) = last_conversation(&h);
    assert!(!resumed);
    assert_eq!(first_id.len(), 36, "{first_id}");

    let (sent, _, last) = send(&client, "alpha", "again").await;
    assert_eq!(sent["context_reset"], false);
    assert_eq!(last["turn"]["state"], "completed");
    assert_eq!(last_conversation(&h), (first_id, true));
    client.cancel().await.unwrap();
}

/// Spec (`SES-5`, SPEC_impl.md "Claude"): when Claude's transcript of the conversation is gone, the next turn starts
/// the conversation again under the same ID and reports `context_reset`. Nothing is untracked by that, so removal
/// needs no `force` afterwards.
#[tokio::test]
async fn restarts_a_lost_conversation_under_the_same_id() {
    let h = Harness::new();
    let client = connect(&h).await;
    create(&client, "alpha", json!({})).await;
    send(&client, "alpha", "hello").await;
    let (id, _) = last_conversation(&h);
    for path in named_with(&claude_home(&h).join("projects"), &id) {
        if path.is_file() {
            std::fs::remove_file(path).unwrap();
        }
    }

    let (sent, _, last) = send(&client, "alpha", "again").await;
    assert_eq!(sent["context_reset"], true);
    assert_eq!(last["turn"]["state"], "completed");
    assert_eq!(last_conversation(&h), (id, false));
    let removed = structured(
        &call(&client, "session_remove", json!({ "name": "alpha" }))
            .await
            .unwrap(),
    );
    assert_eq!(removed["agent_data_deleted"], true);
    client.cancel().await.unwrap();
}

/// Spec (SPEC_impl.md "Claude"): with more than one transcript for the conversation, devbox-mcp cannot tell which to
/// continue, so the send fails and Claude is not started, rather than resuming a guess.
#[tokio::test]
async fn refuses_to_send_with_ambiguous_transcripts() {
    let h = Harness::new();
    let client = connect(&h).await;
    create(&client, "alpha", json!({})).await;
    send(&client, "alpha", "hello").await;
    let (id, _) = last_conversation(&h);
    let projects = claude_home(&h).join("projects");
    let original = named_with(&projects, &format!("{id}.jsonl")).remove(0);
    std::fs::create_dir_all(projects.join("-elsewhere")).unwrap();
    std::fs::copy(&original, projects.join(format!("-elsewhere/{id}.jsonl"))).unwrap();
    std::fs::remove_file(h.fake_dir.join("claude-argv")).unwrap();

    let error = error_text(
        call(
            &client,
            "session_send",
            json!({ "name": "alpha", "message": "again" }),
        )
        .await,
    );
    assert!(error.contains("found 2 transcripts"), "{error}");
    assert!(
        !h.fake_dir.join("claude-argv").exists(),
        "Claude was started"
    );
    client.cancel().await.unwrap();
}

/// Spec (SPEC_impl.md "Claude", "Storage"): Claude reporting a conversation other than the chosen one stops the turn
/// and marks the session as possibly holding untracked data, so removal then needs `force`.
#[tokio::test]
async fn a_foreign_conversation_fails_the_turn_and_needs_force_to_remove() {
    let h = Harness::new();
    let client = connect(&h).await;
    std::fs::write(h.fake_dir.join("claude-mode"), "mismatch").unwrap();
    create(&client, "alpha", json!({})).await;
    let (_, _, last) = send(&client, "alpha", "hello").await;
    assert_eq!(last["turn"]["state"], "failed");
    assert!(
        last["turn"]["error"]
            .as_str()
            .unwrap()
            .contains("instead of"),
        "{last}"
    );

    let error = error_text(call(&client, "session_remove", json!({ "name": "alpha" })).await);
    assert!(error.contains("force"), "{error}");
    let removed = structured(
        &call(
            &client,
            "session_remove",
            json!({ "name": "alpha", "force": true }),
        )
        .await
        .unwrap(),
    );
    assert_eq!(removed["agent_data_deleted"], false);
    client.cancel().await.unwrap();
}

/// Spec (SPEC_impl.md "Storage"): the mark that a foreign conversation may hold untracked data is saved as soon as
/// Claude reports the other ID, so a turn that then fails some other way (here, output devbox-mcp cannot read) still
/// leaves removal refusing to claim the data deleted. A regression: the mark used to travel with the turn's outcome,
/// which such a failure discards.
#[tokio::test]
async fn a_foreign_conversation_is_marked_even_if_the_turn_then_breaks() {
    let h = Harness::new();
    let client = connect(&h).await;
    std::fs::write(h.fake_dir.join("claude-mode"), "mismatch-garbage").unwrap();
    create(&client, "alpha", json!({})).await;
    let (_, _, last) = send(&client, "alpha", "hello").await;
    assert_eq!(last["turn"]["state"], "failed");
    let error = error_text(call(&client, "session_remove", json!({ "name": "alpha" })).await);
    assert!(error.contains("force"), "{error}");
    client.cancel().await.unwrap();
}

/// Spec (`TRN-5`, SPEC_impl.md "Claude"): an error `result` fails the turn with its text (an unknown model arrives
/// this way), and an exit without any `result` fails it with Claude's stderr.
#[tokio::test]
async fn reports_failed_turns() {
    let h = Harness::new();
    let client = connect(&h).await;
    create(&client, "alpha", json!({})).await;
    std::fs::write(h.fake_dir.join("claude-mode"), "error").unwrap();
    let (_, _, last) = send(&client, "alpha", "hello").await;
    assert_eq!(last["turn"]["state"], "failed");
    assert_eq!(last["turn"]["error"], "API Error: fake model problem");

    std::fs::write(h.fake_dir.join("claude-mode"), "no-result").unwrap();
    let (_, _, last) = send(&client, "alpha", "again").await;
    assert_eq!(last["turn"]["state"], "failed");
    let error = last["turn"]["error"].as_str().unwrap();
    assert!(error.contains("fake claude crashed"), "{error}");
    client.cancel().await.unwrap();
}

/// Spec (`SES-6`, SPEC_impl.md "Claude"): removal deletes Claude's per-session files and confirms nothing named with
/// the conversation ID is left. Something it does not know how to delete keeps the session, named in the error, until
/// the caller forces the removal.
#[tokio::test]
async fn removes_claude_data_and_reports_leftovers() {
    let h = Harness::new();
    let client = connect(&h).await;
    create(&client, "alpha", json!({})).await;
    send(&client, "alpha", "hello").await;
    let (id, _) = last_conversation(&h);
    assert_eq!(named_with(&claude_home(&h), &id).len(), 3);
    let removed = structured(
        &call(&client, "session_remove", json!({ "name": "alpha" }))
            .await
            .unwrap(),
    );
    assert_eq!(removed["agent_data_deleted"], true);
    assert!(named_with(&claude_home(&h), &id).is_empty());

    std::fs::write(h.fake_dir.join("claude-mode"), "leftover").unwrap();
    create(&client, "beta", json!({})).await;
    send(&client, "beta", "hello").await;
    let (id, _) = last_conversation(&h);
    let error = error_text(call(&client, "session_remove", json!({ "name": "beta" })).await);
    assert!(error.contains(&format!("tasks/{id}.json")), "{error}");
    let listed = structured(&call(&client, "session_list", json!({})).await.unwrap());
    assert_eq!(listed["sessions"][0]["name"], "beta");
    let removed = structured(
        &call(
            &client,
            "session_remove",
            json!({ "name": "beta", "force": true }),
        )
        .await
        .unwrap(),
    );
    assert_eq!(removed["agent_data_deleted"], false);
    client.cancel().await.unwrap();
}

/// Spec (`TRA-2`): `session_transcript` reads Claude's own transcript of the conversation, and reports it missing
/// rather than returning an empty one.
#[tokio::test]
async fn reads_a_claude_transcript() {
    let h = Harness::new();
    let client = connect(&h).await;
    create(&client, "alpha", json!({})).await;
    send(&client, "alpha", "hello").await;
    send(&client, "alpha", "again").await;
    let (id, _) = last_conversation(&h);
    let page = call_checked(&client, "session_transcript", json!({ "name": "alpha" })).await;
    assert_eq!(page["total"], 2);
    assert_eq!(page["entries"][1]["entry"]["sessionId"], id.as_str());

    for path in named_with(&claude_home(&h).join("projects"), &id) {
        std::fs::remove_file(path).unwrap();
    }
    let error = error_text(call(&client, "session_transcript", json!({ "name": "alpha" })).await);
    assert!(error.contains("was not found"), "{error}");
    client.cancel().await.unwrap();
}
