use super::*;
use serde_json::json;

fn request(id: &str, run_id: &str, generation: u64) -> ApprovalRequest {
    ApprovalRequest {
        id: id.to_string(),
        tool_use_id: format!("native-{id}"),
        tool_name: "Read".to_string(),
        input: json!({"file_path": "Cargo.toml"}),
        run_id: run_id.to_string(),
        generation,
    }
}

fn activate(broker: &ClaudeApprovalBroker, run_id: &str, generation: u64) {
    broker.activate_with_scope(
        run_id,
        generation,
        ClaudeApprovalScope {
            target: crate::claude_agent::Target::Local,
            workspace: std::env::current_dir().unwrap(),
            store_path: None,
        },
    );
}

fn fixture() -> (Arc<ClaudeApprovalBroker>, SessionEventBus) {
    let bus = SessionEventBus::new(Some("session-a".to_string()));
    (ClaudeApprovalBroker::new("session-a", bus.clone()), bus)
}

async fn wait_pending(broker: &ClaudeApprovalBroker, count: usize) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while broker.pending().len() != count {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn headless_and_inactive_requests_fail_closed() {
    let (broker, bus) = fixture();
    let (_sender, cancelled) = watch::channel(false);
    assert!(matches!(
        broker
            .ask(request("one", "run", 1), cancelled.clone())
            .await,
        ApprovalDecision::Deny(_)
    ));
    activate(&broker, "run", 1);
    assert!(matches!(
        broker.ask(request("two", "run", 1), cancelled).await,
        ApprovalDecision::Deny(_)
    ));
    assert!(broker.pending().is_empty());
    assert!(!bus.has_interactive_subscribers());
}

#[tokio::test]
async fn input_too_large_to_show_in_full_is_denied_without_prompt() {
    let (broker, bus) = fixture();
    let _subscriber = bus.subscribe_assistant_deltas();
    let (_sender, cancelled) = watch::channel(false);
    activate(&broker, "run", 1);
    let mut oversized = request("large", "run", 1);
    oversized.input = json!({"content": "x".repeat(MAX_INPUT_PREVIEW_CHARS)});
    assert!(matches!(
        broker.ask(oversized, cancelled).await,
        ApprovalDecision::Deny(_)
    ));
    assert!(broker.pending().is_empty());
}

#[tokio::test]
async fn reply_is_bound_to_exact_generation_and_not_remembered() {
    let (broker, bus) = fixture();
    let _subscriber = bus.subscribe_assistant_deltas();
    let mut events = bus.subscribe();
    let (_sender, cancelled) = watch::channel(false);
    activate(&broker, "dispatch", 4);
    let task = {
        let broker = Arc::clone(&broker);
        tokio::spawn(async move {
            broker
                .ask(request("tool-1", "dispatch", 4), cancelled)
                .await
        })
    };
    wait_pending(&broker, 1).await;
    let request_id = broker.pending()[0].id.clone();
    let asked = events.recv().await.unwrap();
    assert!(matches!(
        asked.event,
        SessionEvent::ClaudePermissionAsked { request }
            if request.claude_request_id == "native-tool-1"
    ));
    assert!(broker
        .reply(&request_id, "dispatch", 3, ClaudePermissionReply::AllowOnce)
        .is_err());
    assert_eq!(broker.pending().len(), 1);
    broker
        .reply(&request_id, "dispatch", 4, ClaudePermissionReply::AllowOnce)
        .unwrap();
    assert!(matches!(task.await.unwrap(), ApprovalDecision::Allow));
    assert!(broker.pending().is_empty());
    assert!(broker
        .reply(&request_id, "dispatch", 4, ClaudePermissionReply::AllowOnce)
        .is_err());
}

#[tokio::test]
async fn generation_change_and_scope_close_deny_pending_requests() {
    let (broker, bus) = fixture();
    let _subscriber = bus.subscribe_assistant_deltas();
    let (_sender, cancelled) = watch::channel(false);
    activate(&broker, "run", 1);
    let first = {
        let broker = Arc::clone(&broker);
        let cancelled = cancelled.clone();
        tokio::spawn(async move { broker.ask(request("first", "run", 1), cancelled).await })
    };
    wait_pending(&broker, 1).await;
    let first_request_id = broker.pending()[0].id.clone();
    activate(&broker, "run", 2);
    assert!(matches!(first.await.unwrap(), ApprovalDecision::Deny(_)));
    assert!(broker
        .reply(
            &first_request_id,
            "run",
            1,
            ClaudePermissionReply::AllowOnce
        )
        .is_err());
    let second = {
        let broker = Arc::clone(&broker);
        tokio::spawn(async move { broker.ask(request("second", "run", 2), cancelled).await })
    };
    wait_pending(&broker, 1).await;
    broker.close_scope("run", 1);
    assert_eq!(broker.pending().len(), 1);
    broker.close_scope("run", 2);
    assert!(matches!(second.await.unwrap(), ApprovalDecision::Deny(_)));
}

#[tokio::test]
async fn cancellation_and_subscriber_loss_dismiss_requests() {
    let (broker, bus) = fixture();
    let subscriber = bus.subscribe_assistant_deltas();
    let (sender, cancelled) = watch::channel(false);
    activate(&broker, "run", 1);
    let first = {
        let broker = Arc::clone(&broker);
        let cancelled = cancelled.clone();
        tokio::spawn(async move { broker.ask(request("first", "run", 1), cancelled).await })
    };
    wait_pending(&broker, 1).await;
    sender.send(true).unwrap();
    assert!(matches!(first.await.unwrap(), ApprovalDecision::Deny(_)));
    wait_pending(&broker, 0).await;
    let (_sender, cancelled) = watch::channel(false);
    let second = {
        let broker = Arc::clone(&broker);
        tokio::spawn(async move { broker.ask(request("second", "run", 1), cancelled).await })
    };
    wait_pending(&broker, 1).await;
    drop(subscriber);
    assert!(matches!(second.await.unwrap(), ApprovalDecision::Deny(_)));
    wait_pending(&broker, 0).await;
}

#[tokio::test]
async fn closing_one_worker_dispatch_keeps_another_dispatch_pending() {
    let (broker, bus) = fixture();
    let _subscriber = bus.subscribe_assistant_deltas();
    let (_sender, cancelled) = watch::channel(false);
    activate(&broker, "dispatch-a", 0);
    activate(&broker, "dispatch-b", 0);
    let first = {
        let broker = Arc::clone(&broker);
        let cancelled = cancelled.clone();
        tokio::spawn(async move {
            broker
                .ask(request("same-native-id", "dispatch-a", 0), cancelled)
                .await
        })
    };
    let second = {
        let broker = Arc::clone(&broker);
        tokio::spawn(async move {
            broker
                .ask(request("same-native-id", "dispatch-b", 0), cancelled)
                .await
        })
    };
    wait_pending(&broker, 2).await;
    assert_ne!(broker.pending()[0].id, broker.pending()[1].id);
    broker.close_scope("dispatch-a", 0);
    assert!(matches!(first.await.unwrap(), ApprovalDecision::Deny(_)));
    assert_eq!(broker.pending().len(), 1);
    let remaining = broker.pending().pop().unwrap();
    assert_eq!(remaining.run_id, "dispatch-b");
    broker
        .reply(
            &remaining.id,
            "dispatch-b",
            0,
            ClaudePermissionReply::AllowOnce,
        )
        .unwrap();
    assert!(matches!(second.await.unwrap(), ApprovalDecision::Allow));
}

#[tokio::test]
async fn ui_cannot_override_bash_or_opaque_tool_hard_denial() {
    let (broker, bus) = fixture();
    let _subscriber = bus.subscribe_assistant_deltas();
    let (_sender, cancelled) = watch::channel(false);
    activate(&broker, "run", 1);
    for tool in ["Bash", "NotebookEdit", "Task", "mcp__unknown__write"] {
        let mut attempted = request(tool, "run", 1);
        attempted.tool_name = tool.to_string();
        attempted.input = json!({"command": "pwd"});
        assert!(matches!(
            broker.ask(attempted, cancelled.clone()).await,
            ApprovalDecision::Deny(_)
        ));
    }
    assert!(broker.pending().is_empty());
    assert!(broker
        .reply("any", "run", 1, ClaudePermissionReply::AllowOnce)
        .is_err());
}

#[tokio::test]
async fn workspace_escape_and_git_metadata_mutation_are_hard_denied() {
    let (broker, bus) = fixture();
    let _subscriber = bus.subscribe_assistant_deltas();
    let (_sender, cancelled) = watch::channel(false);
    activate(&broker, "run", 1);
    for path in ["../outside", "/tmp/outside", ".git/config"] {
        let mut attempted = request(path, "run", 1);
        attempted.tool_name = "Write".to_string();
        attempted.input = json!({"file_path": path, "content": "test"});
        assert!(matches!(
            broker.ask(attempted, cancelled.clone()).await,
            ApprovalDecision::Deny(_)
        ));
    }
    assert!(broker.pending().is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn symlinked_file_tool_path_is_denied_before_ui() {
    let (broker, bus) = fixture();
    let _subscriber = bus.subscribe_assistant_deltas();
    let (_sender, cancelled) = watch::channel(false);
    let workspace = std::env::temp_dir().join(format!(
        "nac-claude-approval-symlink-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&workspace).unwrap();
    std::os::unix::fs::symlink("inside.txt", workspace.join("alias.txt")).unwrap();
    broker.activate_with_scope(
        "run",
        1,
        ClaudeApprovalScope {
            target: crate::claude_agent::Target::Local,
            workspace: workspace.clone(),
            store_path: None,
        },
    );
    let mut attempted = request("symlink", "run", 1);
    attempted.input = json!({"file_path": "alias.txt"});
    assert!(matches!(
        broker.ask(attempted, cancelled).await,
        ApprovalDecision::Deny(_)
    ));
    assert!(broker.pending().is_empty());
    std::fs::remove_dir_all(workspace).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn path_changed_to_symlink_after_ui_approval_is_denied() {
    let (broker, bus) = fixture();
    let _subscriber = bus.subscribe_assistant_deltas();
    let (_sender, cancelled) = watch::channel(false);
    let workspace =
        std::env::temp_dir().join(format!("nac-claude-approval-race-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&workspace).unwrap();
    broker.activate_with_scope(
        "run",
        1,
        ClaudeApprovalScope {
            target: crate::claude_agent::Target::Local,
            workspace: workspace.clone(),
            store_path: None,
        },
    );
    let mut attempted = request("write", "run", 1);
    attempted.tool_name = "Write".to_string();
    attempted.input = json!({"file_path": "new.txt", "content": "safe"});
    let task = {
        let broker = Arc::clone(&broker);
        tokio::spawn(async move { broker.ask(attempted, cancelled).await })
    };
    wait_pending(&broker, 1).await;
    let request_id = broker.pending()[0].id.clone();
    std::os::unix::fs::symlink("elsewhere.txt", workspace.join("new.txt")).unwrap();
    broker
        .reply(&request_id, "run", 1, ClaudePermissionReply::AllowOnce)
        .unwrap();
    assert!(matches!(task.await.unwrap(), ApprovalDecision::Deny(_)));
    std::fs::remove_dir_all(workspace).unwrap();
}
