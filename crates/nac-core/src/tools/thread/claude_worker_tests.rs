use super::*;
use serde_json::json;

fn runtime_with_claude_thread() -> (ToolRuntime, store::ClaudeThreadBinding, PathBuf) {
    let root =
        std::env::temp_dir().join(format!("nac_claude_worker_test_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let mut runtime = crate::tools::test_runtime();
    runtime.store_path = root.join("store.db");
    store::initialize(&runtime.store_path).unwrap();
    let snapshot = crate::sessions::new_snapshot(
        "test-session".to_string(),
        runtime.workspace_cwd.clone(),
        "test-model".to_string(),
        "https://api.openai.com/v1".to_string(),
        crate::model::BackendKind::OpenAiResponses,
        None,
        None,
        None,
        Vec::new(),
        None,
        std::collections::BTreeMap::new(),
    );
    crate::sessions::create_session(&runtime.store_path, &snapshot).unwrap();
    let binding = store::ClaudeThreadBinding {
        host_id: None,
        ssh_port: None,
        ssh_identity_file: None,
        workspace: runtime.workspace_cwd.clone(),
        config_dir: None,
        native_session_id: None,
    };
    store::ensure_thread_agent(
        &runtime.store_path,
        "test-session",
        "impl",
        store::ThreadAgent::Claude,
        Some(&binding),
    )
    .unwrap();
    runtime.claude_approval_broker = Some(crate::claude_approval::ClaudeApprovalBroker::new(
        "test-session",
        crate::events::SessionEventBus::new(Some("test-session".to_string())),
    ));
    (runtime, binding, root)
}

fn invocation() -> WorkerInvocation<'static> {
    WorkerInvocation {
        session_id: "test-session",
        thread_name: "impl",
        dispatch_id: "dispatch-test",
        action: "task",
        source_threads: &[],
        scheduled_skills: &[],
        timeout_secs: 1800,
    }
}

#[test]
fn resumed_prompt_uses_fresh_sources_without_replaying_self_history() {
    let self_episode = store::EpisodeRecord {
        id: 1,
        thread_name: "worker".to_string(),
        session_id: "parent".to_string(),
        action: "old task".to_string(),
        content: "old private worker answer".to_string(),
        status: "ok".to_string(),
        created_at: "earlier".to_string(),
    };
    let source_episode = store::EpisodeRecord {
        thread_name: "research".to_string(),
        content: "new research handoff".to_string(),
        ..self_episode.clone()
    };
    let context = WorkerContext {
        self_episodes: vec![self_episode],
        source_episodes: vec![source_episode],
    };
    let invocation = WorkerInvocation {
        session_id: "parent",
        thread_name: "worker",
        dispatch_id: "dispatch-2",
        action: "new task",
        source_threads: &[],
        scheduled_skills: &[],
        timeout_secs: 1800,
    };
    let prompt = prompt_from_context(&invocation, &context, None).unwrap();
    assert!(prompt.contains("new task"));
    assert!(prompt.contains("new research handoff"));
    assert!(!prompt.contains("old private worker answer"));
}

#[tokio::test]
async fn claude_event_mapping_preserves_native_tool_id_and_bounded_text() {
    let active = tokio::sync::Mutex::new(HashMap::new());
    let message = json!({
        "type": "assistant",
        "parent_tool_use_id": "toolu-parent-1",
        "message": {
            "content": [
                { "type": "tool_use", "id": "toolu-native-1", "name": "Bash", "input": {"command":"secret"} },
                { "type": "text", "text": "work in progress" }
            ],
            "usage": { "input_tokens": 10, "output_tokens": 4 }
        }
    });
    let events = assistant_events(&message, Some("impl"), &active).await;
    assert!(
        matches!(&events[0], AgentEvent::ToolCallStarted { call_id, parent_call_id: Some(parent), args_detail: None, .. } if call_id == "toolu-native-1" && parent == "toolu-parent-1")
    );
    assert!(
        matches!(&events[1], AgentEvent::AssistantMessage { content, .. } if content == "work in progress")
    );
    assert!(!format!("{events:?}").contains("secret"));
    let result = json!({
        "type": "user",
        "parent_tool_use_id": "toolu-parent-1",
        "message": { "content": [{ "type": "tool_result", "tool_use_id": "toolu-native-1", "is_error": false }] }
    });
    let events = assistant_events(&result, Some("impl"), &active).await;
    assert!(
        matches!(&events[0], AgentEvent::ToolCallFinished { call_id, parent_call_id: Some(parent), name, .. } if call_id == "toolu-native-1" && parent == "toolu-parent-1" && name == "Bash")
    );
    assert_eq!(assistant_usage(&message).unwrap().input_tokens, 10);
}

#[tokio::test]
async fn claude_assistant_event_redacts_credentials_before_emission() {
    let active = tokio::sync::Mutex::new(HashMap::new());
    let message = json!({
        "type": "assistant",
        "message": { "content": [{
            "type": "text", "text": "Authorization: Bearer sk-live-canary-8421; done"
        }] }
    });
    let events = assistant_events(&message, None, &active).await;
    let AgentEvent::AssistantMessage { content, .. } = &events[0] else {
        panic!("expected Claude assistant event");
    };
    assert!(!content.contains("sk-live-canary-8421"));
    assert!(content.contains("[REDACTED]"));
}

#[test]
fn queued_steering_prompt_keeps_durable_ids_and_order() {
    let record = |id, instruction: &str| store::ThreadSteeringRecord {
        id,
        session_id: "parent".to_string(),
        thread_name: "impl".to_string(),
        dispatch_id: "dispatch".to_string(),
        instruction: instruction.to_string(),
        status: "claimed".to_string(),
        created_at: "now".to_string(),
        claimed_at: Some("now".to_string()),
        delivered_at: None,
        expired_at: None,
    };
    let prompt = steering_prompt(&[record(4, "check the UI"), record(5, "add a test")]);
    assert!(prompt.find("Steering #4").unwrap() < prompt.find("Steering #5").unwrap());
    assert!(prompt.contains("check the UI"));
    assert!(prompt.contains("add a test"));
}

#[tokio::test]
async fn cancellation_before_claude_spawn_retains_marker_until_terminal_record() {
    let (runtime, binding, root) = runtime_with_claude_thread();
    let cancellation = ThreadCancellation::default();
    cancellation.cancel();
    let run = run_turn(&runtime, invocation(), cancellation, binding, None, 0)
        .await
        .unwrap();
    assert!(run.cancelled);
    assert_eq!(
        store::list_claude_process_markers(&runtime.store_path, "test-session")
            .unwrap()
            .len(),
        1
    );
    assert!(
        store::thread_read(&runtime.store_path, "test-session", "impl")
            .unwrap()
            .is_empty()
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn adapter_error_retains_process_marker_for_recovery() {
    let (runtime, mut binding, root) = runtime_with_claude_thread();
    binding.native_session_id = Some("invalid-native-id".to_string());
    let failed = run_turn(
        &runtime,
        invocation(),
        ThreadCancellation::default(),
        binding,
        None,
        0,
    )
    .await;
    assert!(failed.is_err());
    let markers = store::list_claude_process_markers(&runtime.store_path, "test-session").unwrap();
    assert_eq!(markers.len(), 1);
    assert_eq!(markers[0].operation_id, "dispatch-test");
    assert!(
        store::thread_read(&runtime.store_path, "test-session", "impl")
            .unwrap()
            .is_empty()
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn handoff_retry_returns_original_episode_and_skips_claude_launch() {
    let (runtime, binding, root) = runtime_with_claude_thread();
    let first = commit_handoff(&runtime, &invocation(), "first committed answer")
        .await
        .unwrap();
    assert!(
        super::super::handed_off(
            &runtime,
            "test-session",
            "impl",
            "dispatch-test",
            super::super::DispatchAgent::Claude,
            None,
        )
        .await
    );
    // Simulate a crash after the episode commit but before the caller
    // observes the result or clears its process marker.
    store::insert_claude_process_marker(
        &runtime.store_path,
        &store::ClaudeProcessMarker {
            session_id: "test-session".to_string(),
            operation_id: "dispatch-test".to_string(),
            generation: 0,
            kind: store::ClaudeProcessKind::Worker,
            thread_name: Some("impl".to_string()),
            host_id: None,
            ssh_port: None,
            ssh_identity_file: None,
            workspace: runtime.workspace_cwd.clone(),
            config_dir: None,
            pidfile: root.join("missing-pidfile").display().to_string(),
            native_session_id: None,
        },
    )
    .unwrap();
    let retried = commit_handoff(&runtime, &invocation(), "different retry answer")
        .await
        .unwrap();
    assert_eq!(first.id, retried.id);
    assert_eq!(retried.content, "first committed answer");
    let unreconciled = run(
        &runtime,
        invocation(),
        ThreadCancellation::default(),
        binding.clone(),
    )
    .await
    .err()
    .expect("uncleared process marker must block replay");
    assert!(unreconciled.to_string().contains("recovery must finish"));
    store::clear_claude_process_marker(&runtime.store_path, "test-session", "dispatch-test", 0)
        .unwrap();
    let resumed = run(
        &runtime,
        invocation(),
        ThreadCancellation::default(),
        binding,
    )
    .await
    .unwrap();
    assert_eq!(resumed.stdout, "first committed answer");
    assert_eq!(resumed.exit_code, 0);
    assert_eq!(
        store::thread_read(&runtime.store_path, "test-session", "impl")
            .unwrap()
            .len(),
        1
    );
    std::fs::remove_dir_all(root).unwrap();
}
