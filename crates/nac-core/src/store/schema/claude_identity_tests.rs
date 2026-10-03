use super::*;

#[test]
fn partial_claude_cancellation_suffix_recovers_as_cancelled_terminal() {
    let path = temp_store_path("claude_partial_cancel_recovery");
    initialize(&path).unwrap();
    let conn = Connection::open(&path).unwrap();
    insert_legacy_session(&conn, "claude-parent");
    drop(conn);
    let writer = TranscriptLogWriter::new(&path).unwrap();
    writer
        .append_run_prompt(
            "claude-parent",
            0,
            &crate::types::Message::User {
                content: "change files".into(),
            },
            "run-1",
        )
        .unwrap();
    writer
        .append(
            "claude-parent",
            1,
            &crate::types::Message::Assistant {
                content: Some(format!(
                    "partial Claude draft\n\n{}",
                    crate::agent::RUN_CANCELLED_MARKER
                )),
                reasoning_text: None,
                reasoning_details: None,
                tool_calls: None,
                duration_ms: None,
                model_origin: None,
                reasoning_field: None,
            },
        )
        .unwrap();
    assert!(matches!(
        reconcile_active_run(&path, "claude-parent").unwrap(),
        ActiveRunReconciliation::CanonicalTerminal
    ));
    assert!(load_run_recovery(&path, "claude-parent").unwrap().is_none());
    let entries = writer.read_from("claude-parent", 0).unwrap();
    assert_eq!(entries.len(), 2);
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn process_markers_survive_restart_and_clear_only_matching_generation() {
    let path = temp_store_path("claude_process_marker");
    initialize(&path).unwrap();
    let conn = Connection::open(&path).unwrap();
    insert_legacy_session(&conn, "parent");
    drop(conn);
    let marker = ClaudeProcessMarker {
        session_id: "parent".into(),
        operation_id: "dispatch-1".into(),
        generation: 7,
        kind: ClaudeProcessKind::Worker,
        thread_name: Some("review".into()),
        host_id: Some("host".into()),
        ssh_port: Some(22),
        ssh_identity_file: None,
        workspace: PathBuf::from("/remote/work"),
        config_dir: None,
        pidfile: "/tmp/claude.pid".into(),
        native_session_id: None,
    };
    insert_claude_process_marker(&path, &marker).unwrap();
    update_claude_process_native_session_id(&path, "parent", "dispatch-1", 7, "native-1").unwrap();
    assert!(
        update_claude_process_native_session_id(&path, "parent", "dispatch-1", 7, "native-2")
            .is_err()
    );
    let records = list_claude_process_markers(&path, "parent").unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].native_session_id.as_deref(), Some("native-1"));
    clear_claude_process_marker(&path, "parent", "dispatch-1", 8).unwrap();
    assert_eq!(
        list_claude_process_markers(&path, "parent").unwrap().len(),
        1
    );
    clear_claude_process_marker(&path, "parent", "dispatch-1", 7).unwrap();
    assert!(list_claude_process_markers(&path, "parent")
        .unwrap()
        .is_empty());
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn v29_migration_defaults_legacy_runtime_and_thread_agent_without_rewriting_identity() {
    let path = temp_store_path("v29_claude_identity");
    initialize(&path).unwrap();
    let conn = Connection::open(&path).unwrap();
    insert_legacy_session(&conn, "legacy-session");
    conn.execute("INSERT INTO threads (name, session_id, created_at, updated_at) VALUES ('worker', 'legacy-session', 'old', 'old')", []).unwrap();
    conn.execute_batch(
        "ALTER TABLE sessions DROP COLUMN agent_runtime;
                        ALTER TABLE sessions DROP COLUMN claude_agent_json;
                        ALTER TABLE sessions DROP COLUMN claude_native_session_id;
                        ALTER TABLE sessions DROP COLUMN claude_worker_trusted_workspace;
                        ALTER TABLE sessions DROP COLUMN claude_worker_trust_binding_json;
                        ALTER TABLE threads DROP COLUMN agent;
                        ALTER TABLE threads DROP COLUMN claude_native_session_id;
                        ALTER TABLE threads DROP COLUMN claude_binding_json;
                        PRAGMA user_version = 29;",
    )
    .unwrap();
    drop(conn);
    initialize(&path).unwrap();
    let conn = Connection::open(&path).unwrap();
    let identity: (String, String, String) = conn.query_row(
        "SELECT session_id, behavior, agent_runtime FROM sessions WHERE session_id = 'legacy-session'", [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    ).unwrap();
    assert_eq!(
        identity,
        ("legacy-session".into(), "orchestrator".into(), "nac".into())
    );
    let agent: String = conn
        .query_row(
            "SELECT agent FROM threads WHERE name = 'worker'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(agent, "nac");
    drop(conn);
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn claude_thread_binding_rejects_agent_host_and_native_id_changes() {
    let path = temp_store_path("claude_thread_binding");
    initialize(&path).unwrap();
    let binding = ClaudeThreadBinding {
        host_id: Some("user@example".into()),
        ssh_port: Some(22),
        ssh_identity_file: None,
        workspace: PathBuf::from("/work/project"),
        config_dir: Some("/home/user/.claude".into()),
        native_session_id: None,
    };
    ensure_thread_agent(
        &path,
        "parent",
        "review",
        ThreadAgent::Claude,
        Some(&binding),
    )
    .unwrap();
    save_thread_claude_session_id(&path, "parent", "review", &binding, "native-1").unwrap();
    assert_eq!(
        load_thread_claude_binding(&path, "parent", "review")
            .unwrap()
            .unwrap()
            .native_session_id
            .as_deref(),
        Some("native-1")
    );
    assert!(ensure_thread_agent(&path, "parent", "review", ThreadAgent::Nac, None).is_err());
    let mut different_host = binding.clone();
    different_host.host_id = Some("other@example".into());
    assert!(ensure_thread_agent(
        &path,
        "parent",
        "review",
        ThreadAgent::Claude,
        Some(&different_host)
    )
    .is_err());
    assert!(
        save_thread_claude_session_id(&path, "parent", "review", &binding, "native-2").is_err()
    );
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn claude_dispatch_handoff_is_exactly_once_and_returns_original_answer() {
    let path = temp_store_path("claude_dispatch_handoff");
    initialize(&path).unwrap();
    let first = append_episode_for_dispatch_once(
        &path,
        "parent",
        "review",
        "dispatch-1",
        "review code",
        "first answer",
    )
    .unwrap();
    let retry = append_episode_for_dispatch_once(
        &path,
        "parent",
        "review",
        "dispatch-1",
        "review code",
        "different answer",
    )
    .unwrap();
    assert_eq!(retry, first);
    assert_eq!(retry.content, "first answer");
    assert_eq!(
        load_episode_for_dispatch(&path, "parent", "dispatch-1").unwrap(),
        Some(first)
    );
    assert_eq!(thread_read(&path, "parent", "review").unwrap().len(), 1);
    assert!(append_episode_for_dispatch_once(
        &path,
        "parent",
        "other",
        "dispatch-1",
        "review code",
        "wrong thread",
    )
    .is_err());
    assert!(append_episode_for_dispatch_once(
        &path,
        "parent",
        "review",
        "dispatch-1",
        "different action",
        "wrong action",
    )
    .is_err());
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn worker_init_is_atomic_and_recovery_restores_old_split_write() {
    let path = temp_store_path("claude_worker_init_atomic");
    initialize(&path).unwrap();
    let conn = Connection::open(&path).unwrap();
    insert_legacy_session(&conn, "parent");
    drop(conn);
    let binding = ClaudeThreadBinding {
        host_id: Some("user@example".into()),
        ssh_port: Some(22),
        ssh_identity_file: None,
        workspace: PathBuf::from("/remote/work"),
        config_dir: None,
        native_session_id: None,
    };
    ensure_thread_agent(
        &path,
        "parent",
        "review",
        ThreadAgent::Claude,
        Some(&binding),
    )
    .unwrap();
    let marker = ClaudeProcessMarker {
        session_id: "parent".into(),
        operation_id: "dispatch-1".into(),
        generation: 7,
        kind: ClaudeProcessKind::Worker,
        thread_name: Some("review".into()),
        host_id: binding.host_id.clone(),
        ssh_port: binding.ssh_port,
        ssh_identity_file: None,
        workspace: binding.workspace.clone(),
        config_dir: binding.config_dir.clone(),
        pidfile: "/tmp/claude.pid".into(),
        native_session_id: None,
    };
    insert_claude_process_marker(&path, &marker).unwrap();
    save_worker_claude_init(
        &path,
        "parent",
        "review",
        "dispatch-1",
        7,
        &binding,
        "native-1",
    )
    .unwrap();
    assert_eq!(
        load_thread_claude_binding(&path, "parent", "review")
            .unwrap()
            .unwrap()
            .native_session_id
            .as_deref(),
        Some("native-1")
    );
    let current = list_claude_process_markers(&path, "parent")
        .unwrap()
        .remove(0);
    assert_eq!(current.native_session_id.as_deref(), Some("native-1"));
    reconcile_worker_claude_marker_identity(&path, &current).unwrap();
    let conn = Connection::open(&path).unwrap();
    conn.execute("UPDATE threads SET claude_native_session_id = NULL WHERE session_id = 'parent' AND name = 'review'", []).unwrap();
    drop(conn);
    reconcile_worker_claude_marker_identity(&path, &current).unwrap();
    assert_eq!(
        load_thread_claude_binding(&path, "parent", "review")
            .unwrap()
            .unwrap()
            .native_session_id
            .as_deref(),
        Some("native-1")
    );
    assert!(save_worker_claude_init(
        &path,
        "parent",
        "review",
        "dispatch-1",
        7,
        &binding,
        "native-2"
    )
    .is_err());
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn v30_migration_adds_unique_dispatch_handoff_without_losing_episodes() {
    let path = temp_store_path("v30_dispatch_handoff");
    initialize(&path).unwrap();
    append_episode(&path, "parent", "review", "old action", "old answer").unwrap();
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        "DROP INDEX idx_episode_dispatch_handoff;
         ALTER TABLE episodes DROP COLUMN dispatch_id;
         ALTER TABLE claude_processes DROP COLUMN thread_name;
         ALTER TABLE claude_processes DROP COLUMN config_dir;
         PRAGMA user_version = 30;",
    )
    .unwrap();
    drop(conn);
    initialize(&path).unwrap();
    assert_eq!(thread_read(&path, "parent", "review").unwrap().len(), 1);
    let committed = append_episode_for_dispatch_once(
        &path,
        "parent",
        "review",
        "dispatch-1",
        "new action",
        "new answer",
    )
    .unwrap();
    assert_eq!(committed.content, "new answer");
    assert_eq!(
        load_episode_for_dispatch(&path, "parent", "dispatch-1").unwrap(),
        Some(committed)
    );
    let conn = Connection::open(&path).unwrap();
    assert_eq!(
        conn.pragma_query_value::<i64, _>(None, "user_version", |row| row.get(0))
            .unwrap(),
        STORE_SCHEMA_VERSION
    );
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn claude_worker_trust_is_scoped_to_pinned_workspace_and_host() {
    let path = temp_store_path("claude_worker_trust");
    initialize(&path).unwrap();
    let conn = Connection::open(&path).unwrap();
    insert_legacy_session(&conn, "parent");
    drop(conn);
    assert!(!crate::sessions::load_claude_worker_workspace_trust(&path, "parent").unwrap());
    crate::sessions::trust_claude_worker_workspace(&path, "parent").unwrap();
    assert!(crate::sessions::load_claude_worker_workspace_trust(&path, "parent").unwrap());
    let conn = Connection::open(&path).unwrap();
    conn.execute(
        "UPDATE sessions SET host_id = 'other-host' WHERE session_id = 'parent'",
        [],
    )
    .unwrap();
    drop(conn);
    assert!(!crate::sessions::load_claude_worker_workspace_trust(&path, "parent").unwrap());
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}
#[test]
fn runtime_connections_restore_wal_mode() {
    let path = temp_store_path("runtime_wal");
    initialize(&path).unwrap();

    let conn = Connection::open(&path).unwrap();
    conn.pragma_update(None, "journal_mode", "DELETE").unwrap();
    drop(conn);

    let runtime = open_runtime_connection(&path).unwrap();
    let journal_mode: String = runtime
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .unwrap();
    assert_eq!(journal_mode.to_ascii_lowercase(), "wal");

    drop(runtime);
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}
