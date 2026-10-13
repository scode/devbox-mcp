//! Tests of session creation, listing, and persistence (SPEC.md "Sessions" and the `session_create` and `session_list`
//! tools), against the real devbox-mcp binary over stdio with an injected state directory.
//!
//! These cover the rules that are easy to get subtly wrong and that callers depend on: name validation, working
//! directories, persistence across restarts (the point of sessions, `SES-5`), how unreadable records are handled, the
//! single-instance lock on the state directory, and paging.

mod support;

use std::time::Duration;

use rmcp::model::CallToolResult;
use serde_json::{Value, json};
use support::{Harness, call, connect};

fn structured(result: &CallToolResult) -> &Value {
    result
        .structured_content
        .as_ref()
        .expect("structured content")
}

/// The error text of a failed call, for checking that errors say what went wrong (`DOC-3`).
fn error_text(result: Result<CallToolResult, rmcp::ServiceError>) -> String {
    match result {
        Err(err) => err.to_string(),
        Ok(result) => panic!("expected an error, got {result:?}"),
    }
}

/// Spec (`SES-1`, `SES-3`, `SES-4`, `SES-7`): a created session shows up in the listing with its agent, the documented
/// default model and effort, and a default working directory named after it under the configured root, which is
/// created.
#[tokio::test]
async fn creates_and_lists_a_session() {
    let h = Harness::new();
    let client = connect(&h).await;

    let created = call(&client, "session_create", json!({ "name": "alpha" }))
        .await
        .unwrap();
    let workdir = h.workdir_root.join("alpha");
    assert_eq!(
        structured(&created),
        &json!({
            "name": "alpha", "agent": "codex", "model": "gpt-6.1-sol", "effort": "high",
            "workdir": workdir.to_str().unwrap(), "turn": null
        })
    );
    assert!(workdir.is_dir());

    let listed = call(&client, "session_list", json!({})).await.unwrap();
    assert_eq!(
        structured(&listed),
        &json!({ "sessions": [structured(&created)], "next_after": null })
    );
    client.cancel().await.unwrap();
}

/// Spec (`SES-7`, SPEC_impl.md "Tools"): model and effort are checked only for characters that are safe to pass to the
/// agent, and a value outside them is rejected with an error that says what is allowed, before anything is created.
/// Unknown but well-formed values are accepted, since the agent is the one that knows what it supports.
#[tokio::test]
async fn checks_model_and_effort_characters() {
    let h = Harness::new();
    let client = connect(&h).await;

    for (field, value) in [
        ("model", "gpt 6"),
        ("model", ""),
        ("effort", "High"),
        ("effort", "x\"y"),
    ] {
        let error = error_text(
            call(
                &client,
                "session_create",
                json!({ "name": "alpha", field: value }),
            )
            .await,
        );
        assert!(error.contains(&format!("invalid {field}")), "{error}");
    }
    assert!(!h.workdir_root.join("alpha").exists());

    let created = call(
        &client,
        "session_create",
        json!({ "name": "alpha", "model": "some-future/model:v2", "effort": "ultra" }),
    )
    .await
    .unwrap();
    let created = structured(&created);
    assert_eq!(created["model"], "some-future/model:v2");
    assert_eq!(created["effort"], "ultra");
    client.cancel().await.unwrap();
}

/// Spec (`SES-3`): an explicit working directory is used as given (and may already exist); a relative one or one that
/// is not a directory is rejected with an error saying so, and no session is created.
#[tokio::test]
async fn handles_explicit_working_directories() {
    let h = Harness::new();
    let client = connect(&h).await;

    let dir = h.tmp.path().join("existing");
    std::fs::create_dir(&dir).unwrap();
    let created = call(
        &client,
        "session_create",
        json!({ "name": "beta", "workdir": dir.to_str().unwrap() }),
    )
    .await
    .unwrap();
    assert_eq!(structured(&created)["workdir"], dir.to_str().unwrap());

    let err = error_text(
        call(
            &client,
            "session_create",
            json!({ "name": "gamma", "workdir": "relative/dir" }),
        )
        .await,
    );
    assert!(err.contains("absolute"), "{err}");

    let file = h.tmp.path().join("a-file");
    std::fs::write(&file, "").unwrap();
    let err = error_text(
        call(
            &client,
            "session_create",
            json!({ "name": "delta", "workdir": file.to_str().unwrap() }),
        )
        .await,
    );
    assert!(
        err.contains("working directory") || err.contains("not a directory"),
        "{err}"
    );

    let listed = call(&client, "session_list", json!({})).await.unwrap();
    assert_eq!(structured(&listed)["sessions"].as_array().unwrap().len(), 1);
    client.cancel().await.unwrap();
}

/// Spec (`SES-2`): names follow the stated rule exactly; anything else is rejected with an error that states the rule.
/// Names become file names, so this also keeps paths like `../x` out.
#[tokio::test]
async fn validates_names() {
    let h = Harness::new();
    let client = connect(&h).await;
    for bad in [
        "",
        "Alpha",
        "1abc",
        "a b",
        "a/b",
        "../x",
        "a.b",
        &"a".repeat(65),
    ] {
        let err = error_text(call(&client, "session_create", json!({ "name": bad })).await);
        assert!(err.contains("lowercase"), "{bad:?}: {err}");
    }
    for good in ["a", "a-b_c9", &"a".repeat(64)] {
        call(&client, "session_create", json!({ "name": good }))
            .await
            .unwrap();
    }
    client.cancel().await.unwrap();
}

/// Spec (`SES-2`, `DOC-3`): a taken name is rejected, and the error shows the existing session so a caller retrying a
/// create whose response it lost can recognize its own session.
#[tokio::test]
async fn rejects_a_taken_name_and_shows_the_session() {
    let h = Harness::new();
    let client = connect(&h).await;
    call(&client, "session_create", json!({ "name": "alpha" }))
        .await
        .unwrap();
    let err = error_text(call(&client, "session_create", json!({ "name": "alpha" })).await);
    assert!(
        err.contains("already exists") && err.contains("\"workdir\""),
        "{err}"
    );
    client.cancel().await.unwrap();
}

/// Spec (`SES-5`): sessions survive a restart of devbox-mcp; that is what makes them worth coming back to.
#[tokio::test]
async fn sessions_survive_a_restart() {
    let h = Harness::new();
    let client = connect(&h).await;
    let created = call(&client, "session_create", json!({ "name": "alpha" }))
        .await
        .unwrap();
    client.cancel().await.unwrap();

    let client = connect(&h).await;
    let listed = call(&client, "session_list", json!({})).await.unwrap();
    assert_eq!(
        structured(&listed)["sessions"],
        json!([structured(&created)])
    );
    client.cancel().await.unwrap();
}

/// Spec (`TRN-5`, SPEC_impl.md "Storage"): a turn a previous devbox-mcp left running is reported as lost, never as
/// still running, because nothing is running it any more.
#[tokio::test]
async fn reports_a_turn_left_running_as_lost() {
    let h = Harness::new();
    let sessions = h.state_dir.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let record = json!({
        "name": "alpha", "agent": "codex", "workdir": "/tmp", "model": "m", "effort": "high", "native_id": "n",
        "turn": { "id": 3, "state": "running", "started_at": "2026-10-11T10:00:00Z", "ended_at": null,
                  "final_message": null, "error": null }
    });
    std::fs::write(sessions.join("alpha.json"), record.to_string()).unwrap();

    let client = connect(&h).await;
    let listed = call(&client, "session_list", json!({})).await.unwrap();
    let turn = &structured(&listed)["sessions"][0]["turn"];
    assert_eq!(turn["state"], "lost");
    assert_eq!(turn["id"], 3);
    client.cancel().await.unwrap();
}

/// SPEC_impl.md "Storage": a record that cannot be parsed does not stop devbox-mcp, is left untouched, is not listed,
/// and blocks its name so nothing overwrites it.
#[tokio::test]
async fn skips_unreadable_records_and_protects_their_names() {
    let h = Harness::new();
    let sessions = h.state_dir.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::write(sessions.join("broken.json"), "{ not json").unwrap();

    let client = connect(&h).await;
    let listed = call(&client, "session_list", json!({})).await.unwrap();
    assert_eq!(structured(&listed)["sessions"], json!([]));
    let err = error_text(call(&client, "session_create", json!({ "name": "broken" })).await);
    assert!(err.contains("cannot read"), "{err}");
    assert_eq!(
        std::fs::read_to_string(sessions.join("broken.json")).unwrap(),
        "{ not json"
    );
    client.cancel().await.unwrap();
}

/// SPEC_impl.md "Storage": two devbox-mcp processes never share a state directory. tunnel-client can start a new one
/// while the old one is still shutting down, so the new one waits for the lock instead of failing at once.
#[tokio::test]
async fn a_second_instance_waits_for_the_first() {
    let h = Harness::new();
    let first = connect(&h).await;
    call(&first, "session_create", json!({ "name": "alpha" }))
        .await
        .unwrap();

    let second = tokio::spawn({
        let h_args = h.devbox_args();
        let home = h.tmp.path().to_owned();
        async move {
            use rmcp::ServiceExt as _;
            let mut cmd = tokio::process::Command::new(support::DEVBOX_MCP);
            cmd.env_clear()
                .envs(support::hermetic_env(&home))
                .args(h_args);
            ().serve(rmcp::transport::TokioChildProcess::new(cmd).unwrap())
                .await
                .unwrap()
        }
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !second.is_finished(),
        "second instance did not wait for the lock"
    );
    first.cancel().await.unwrap();

    let second = tokio::time::timeout(Duration::from_secs(10), second)
        .await
        .expect("second instance never got the lock")
        .unwrap();
    let listed = call(&second, "session_list", json!({})).await.unwrap();
    assert_eq!(structured(&listed)["sessions"][0]["name"], "alpha");
    second.cancel().await.unwrap();
}

/// Spec (`SEC-1`, SPEC_impl.md "Storage"): session records hold the agents' replies and failure text, which can include
/// anything an agent saw, so the store keeps them private to the user whatever the umask is: directories 0700, the lock
/// and records 0600. A regression: they used to take the umask's modes, typically readable by everyone.
#[tokio::test]
async fn keeps_session_records_private() {
    use std::os::unix::fs::PermissionsExt as _;
    let h = Harness::new();
    let client = connect(&h).await;
    call(&client, "session_create", json!({ "name": "alpha" }))
        .await
        .unwrap();
    let mode = |rel: &str| {
        std::fs::metadata(h.state_dir.join(rel))
            .unwrap()
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(mode(""), 0o700);
    assert_eq!(mode("sessions"), 0o700);
    assert_eq!(mode("lock"), 0o600);
    assert_eq!(mode("sessions/alpha.json"), 0o600);
    client.cancel().await.unwrap();
}

/// Spec (`SES-4`, `BND-1`): listing is paged, so every session is reachable however many there are, and each page is
/// bounded.
#[tokio::test]
async fn lists_in_pages() {
    let h = Harness::new();
    let client = connect(&h).await;
    for i in 0..55 {
        call(
            &client,
            "session_create",
            json!({ "name": format!("s{i:02}") }),
        )
        .await
        .unwrap();
    }
    let first = call(&client, "session_list", json!({})).await.unwrap();
    let first = structured(&first);
    assert_eq!(first["sessions"].as_array().unwrap().len(), 50);
    assert_eq!(first["next_after"], "s49");
    let second = call(&client, "session_list", json!({ "after": "s49" }))
        .await
        .unwrap();
    let second = structured(&second);
    let names: Vec<&str> = second["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["s50", "s51", "s52", "s53", "s54"]);
    assert_eq!(second["next_after"], Value::Null);
    client.cancel().await.unwrap();
}

/// Spec (`SES-5`): if the session records cannot even be listed, devbox-mcp refuses to start. Starting with no sessions
/// would let a create overwrite a record it never saw, losing that session's conversation.
#[test]
fn refuses_to_start_when_records_cannot_be_listed() {
    use std::os::unix::fs::PermissionsExt as _;
    // SAFETY: getuid has no preconditions.
    if unsafe { libc::getuid() } == 0 {
        // Root lists any directory regardless of permissions, so the situation cannot be staged.
        return;
    }
    let h = Harness::new();
    let sessions = h.state_dir.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::write(sessions.join("alpha.json"), "{}").unwrap();
    // Searchable and writable, but not listable.
    std::fs::set_permissions(&sessions, std::fs::Permissions::from_mode(0o300)).unwrap();

    let output = std::process::Command::new(support::DEVBOX_MCP)
        .env_clear()
        .envs(support::hermetic_env(h.tmp.path()))
        .args(h.devbox_args())
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    std::fs::set_permissions(&sessions, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("cannot list session records"), "{stderr}");
}
