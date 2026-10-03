use super::*;
use crate::runtime::{build_claude_run_config_for_project, NacConfig, RunOptions, StoreOptions};
use crate::sessions::{ClaudeAgentSession, SessionOperationLease};
use crate::store::{
    self, ActiveRunReconciliation, ClaudeProcessKind, ClaudeProcessMarker, TranscriptLogWriter,
};
use std::fs;
use std::path::Path;
use std::time::Duration;
use uuid::Uuid;

fn fake_cli(root: &Path) -> PathBuf {
    let cli = root.join("claude-fake");
    fs::write(&cli, "#!/bin/sh\ncase \"$1\" in\n  --version) printf '2.1.280 (Claude Code)\\n' ;;\n  auth) printf '{\"loggedIn\":true,\"authMethod\":\"claude.ai\",\"apiProvider\":\"firstParty\"}\\n' ;;\nesac\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&cli, fs::Permissions::from_mode(0o700)).unwrap();
    }
    cli
}

async fn claude_service(label: &str) -> (SessionService, PathBuf, PathBuf, String) {
    let store_path = test_store_path(label);
    let root = store_path.parent().unwrap().to_path_buf();
    fs::create_dir_all(root.join("node_modules")).unwrap();
    let cli = fake_cli(&root);
    let config = build_claude_run_config_for_project(
        RunOptions {
            workspace_cwd: root.clone(),
            store: StoreOptions {
                store_path: Some(store_path.clone()),
            },
            ..RunOptions::default()
        },
        &NacConfig::default(),
        None,
        ClaudeAgentSession {
            executable: cli.display().to_string(),
            model: None,
            config_dir: None,
            trusted_workspace: true,
            native_session_id: None,
        },
    )
    .await
    .unwrap();
    let session_id = config.snapshot.session_id.clone();
    let service = SessionService::from_claude_run_config(config)
        .unwrap()
        .service;
    (service, store_path, root, session_id)
}

async fn wait_for_idle(service: &SessionService) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while service.active_run().is_some() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("Claude run did not settle");
}

#[tokio::test(flavor = "current_thread")]
async fn claude_run_commits_prompt_and_native_id_before_result_then_settles_once() {
    let _env = crate::TEST_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (service, store_path, root, session_id) = claude_service("claude_service_success").await;
    let bridge = root.join("bridge.mjs");
    let release = root.join("release-result");
    let started = root.join("bridge-started");
    fs::write(&bridge, format!(r#"
        import {{ createInterface }} from 'node:readline';
        import fs from 'node:fs';
        const lines = createInterface({{input: process.stdin}});
        lines.once('line', () => {{
          console.log(JSON.stringify({{type:'init',session_id:'ccae5920-848e-4ab1-a950-195ab345a379'}}));
          fs.writeFileSync({started:?}, 'ready');
          const timer = setInterval(() => {{
            if (fs.existsSync({release:?})) {{
              clearInterval(timer);
              console.log(JSON.stringify({{type:'result',session_id:'ccae5920-848e-4ab1-a950-195ab345a379',result:'Claude answer',is_error:false}}));
              lines.close();
            }}
          }}, 10);
        }});
    "#)).unwrap();
    unsafe {
        std::env::set_var("NAC_TEST_CLAUDE_BRIDGE", &bridge);
    }

    let handle = service.try_submit_prompt("Claude request".into()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !started.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("bridge did not initialize");
    let log = TranscriptLogWriter::new(&store_path)
        .unwrap()
        .read_from(&session_id, 0)
        .unwrap();
    assert_eq!(log.len(), 1);
    assert!(matches!(&log[0].1, Message::User { content } if content == "Claude request"));
    assert_eq!(
        crate::sessions::load_session(&store_path, &session_id)
            .unwrap()
            .claude_agent
            .unwrap()
            .native_session_id
            .as_deref(),
        Some("ccae5920-848e-4ab1-a950-195ab345a379")
    );
    assert_eq!(
        store::list_claude_process_markers(&store_path, &session_id)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        store::load_run_recovery(&store_path, &session_id)
            .unwrap()
            .unwrap()
            .run_id,
        handle.run_id.to_string()
    );

    fs::write(&release, "go").unwrap();
    wait_for_idle(&service).await;
    let log = TranscriptLogWriter::new(&store_path)
        .unwrap()
        .read_from(&session_id, 0)
        .unwrap();
    assert_eq!(log.len(), 2);
    assert!(
        matches!(&log[1].1, Message::Assistant { content: Some(text), .. } if text == "Claude answer")
    );
    assert!(store::list_claude_process_markers(&store_path, &session_id)
        .unwrap()
        .is_empty());
    assert!(matches!(
        store::reconcile_active_run(&store_path, &session_id).unwrap(),
        ActiveRunReconciliation::None | ActiveRunReconciliation::CanonicalTerminal
    ));

    unsafe {
        std::env::remove_var("NAC_TEST_CLAUDE_BRIDGE");
    }
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn claude_cancellation_retains_prompt_and_clears_supervised_process() {
    let _env = crate::TEST_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (service, store_path, root, session_id) = claude_service("claude_service_cancel").await;
    let bridge = root.join("bridge.mjs");
    let started = root.join("bridge-started");
    fs::write(&bridge, format!(r#"
        import {{ createInterface }} from 'node:readline';
        import fs from 'node:fs';
        createInterface({{input: process.stdin}}).once('line', () => {{
          console.log(JSON.stringify({{type:'init',session_id:'ccae5920-848e-4ab1-a950-195ab345a379'}}));
          console.log(JSON.stringify({{type:'event',event:{{type:'stream_event',event:{{delta:{{text:'partial Claude draft Authorization: Bearer sk-live-'}}}}}}}}));
          console.log(JSON.stringify({{type:'event',event:{{type:'stream_event',event:{{delta:{{text:'canary-8421'}}}}}}}}));
          fs.writeFileSync({started:?}, 'ready');
          setInterval(() => {{}}, 1000);
        }});
    "#)).unwrap();
    unsafe {
        std::env::set_var("NAC_TEST_CLAUDE_BRIDGE", &bridge);
    }
    let handle = service.try_submit_prompt("cancel me".into()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !started.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("bridge did not initialize");
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let partial = service
                .claude_engine()
                .unwrap()
                .partial_output
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            if partial.contains("sk-live-canary-8421") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("Claude partial output was not buffered before cancellation");
    service.request_cancel(&handle.run_id).await.unwrap();
    wait_for_idle(&service).await;
    let log = TranscriptLogWriter::new(&store_path)
        .unwrap()
        .read_from(&session_id, 0)
        .unwrap();
    assert!(matches!(&log[0].1, Message::User { content } if content == "cancel me"));
    assert_eq!(log.len(), 2);
    assert!(
        matches!(&log[1].1, Message::Assistant { content: Some(text), .. }
        if text == &format!("partial Claude draft Authorization: [REDACTED]\n\n{}", crate::agent::RUN_CANCELLED_MARKER))
    );
    assert!(!format!("{:?}", log).contains("sk-live-canary-8421"));
    assert!(store::list_claude_process_markers(&store_path, &session_id)
        .unwrap()
        .is_empty());
    unsafe {
        std::env::remove_var("NAC_TEST_CLAUDE_BRIDGE");
    }
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn claude_recovery_distinguishes_prompt_init_and_committed_answer() {
    for stage in 0..3 {
        let (service, store_path, root, session_id) = claude_service("claude_recovery_stage").await;
        let run_id = SessionRunId::new();
        let writer = TranscriptLogWriter::new(&store_path).unwrap();
        writer
            .append_run_prompt(
                &session_id,
                0,
                &Message::User {
                    content: "recover me".into(),
                },
                run_id.as_str(),
            )
            .unwrap();
        if stage >= 1 {
            let marker = ClaudeProcessMarker {
                session_id: session_id.clone(),
                operation_id: run_id.to_string(),
                generation: 0,
                kind: ClaudeProcessKind::Session,
                thread_name: None,
                host_id: None,
                ssh_port: None,
                ssh_identity_file: None,
                workspace: crate::sessions::load_session(&store_path, &session_id)
                    .unwrap()
                    .cwd,
                config_dir: None,
                pidfile: root
                    .join(format!("missing-{}.pid", Uuid::new_v4()))
                    .display()
                    .to_string(),
                native_session_id: None,
            };
            store::insert_claude_process_marker(&store_path, &marker).unwrap();
            let native_id = "ccae5920-848e-4ab1-a950-195ab345a379";
            store::update_claude_process_native_session_id(
                &store_path,
                &session_id,
                run_id.as_str(),
                0,
                native_id,
            )
            .unwrap();
            crate::sessions::save_claude_native_session_id(&store_path, &session_id, native_id)
                .unwrap();
        }
        if stage == 2 {
            writer
                .append(
                    &session_id,
                    1,
                    &Message::Assistant {
                        content: Some("committed answer".into()),
                        reasoning_text: None,
                        reasoning_details: None,
                        tool_calls: None,
                        duration_ms: None,
                        model_origin: None,
                        reasoning_field: None,
                    },
                )
                .unwrap();
        }
        let lease = SessionOperationLease::try_acquire(&store_path, &session_id).unwrap();
        let outcome = service
            .reconcile_durable_run_recovery(&lease)
            .await
            .unwrap();
        assert!(store::list_claude_process_markers(&store_path, &session_id)
            .unwrap()
            .is_empty());
        if stage == 2 {
            assert!(matches!(
                outcome,
                ActiveRunReconciliation::CanonicalTerminal
            ));
        } else {
            assert!(matches!(
                outcome,
                ActiveRunReconciliation::Interrupted { .. }
            ));
        }
        drop(lease);
        fs::remove_dir_all(root).unwrap();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn worker_recovery_repairs_native_identity_before_clearing_marker() {
    let (service, store_path, root, session_id) =
        claude_service("claude_worker_identity_recovery").await;
    let workspace = crate::sessions::load_session(&store_path, &session_id)
        .unwrap()
        .cwd;
    let binding = store::ClaudeThreadBinding {
        host_id: None,
        ssh_port: None,
        ssh_identity_file: None,
        workspace: workspace.clone(),
        config_dir: None,
        native_session_id: None,
    };
    store::ensure_thread_agent(
        &store_path,
        &session_id,
        "review",
        store::ThreadAgent::Claude,
        Some(&binding),
    )
    .unwrap();
    let marker = ClaudeProcessMarker {
        session_id: session_id.clone(),
        operation_id: Uuid::new_v4().to_string(),
        generation: 0,
        kind: ClaudeProcessKind::Worker,
        thread_name: Some("review".to_string()),
        host_id: None,
        ssh_port: None,
        ssh_identity_file: None,
        workspace,
        config_dir: None,
        pidfile: root.join("missing-worker.pid").display().to_string(),
        native_session_id: Some("ccae5920-848e-4ab1-a950-195ab345a379".to_string()),
    };
    store::insert_claude_process_marker(&store_path, &marker).unwrap();
    service.reconcile_claude_processes_under_lease().unwrap();
    assert_eq!(
        store::load_thread_claude_binding(&store_path, &session_id, "review")
            .unwrap()
            .unwrap()
            .native_session_id
            .as_deref(),
        marker.native_session_id.as_deref()
    );
    assert!(store::list_claude_process_markers(&store_path, &session_id)
        .unwrap()
        .is_empty());
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn claude_recovery_keeps_marker_when_configuration_binding_changes() {
    let (service, store_path, root, session_id) = claude_service("claude_config_recovery").await;
    let workspace = crate::sessions::load_session(&store_path, &session_id)
        .unwrap()
        .cwd;
    let marker = ClaudeProcessMarker {
        session_id: session_id.clone(),
        operation_id: Uuid::new_v4().to_string(),
        generation: 0,
        kind: ClaudeProcessKind::Session,
        thread_name: None,
        host_id: None,
        ssh_port: None,
        ssh_identity_file: None,
        workspace,
        config_dir: Some("/other-claude-config".to_string()),
        pidfile: root.join("missing-config.pid").display().to_string(),
        native_session_id: None,
    };
    store::insert_claude_process_marker(&store_path, &marker).unwrap();
    let error = service
        .reconcile_claude_processes_under_lease()
        .unwrap_err();
    assert!(error.to_string().contains("configuration binding"));
    assert_eq!(
        store::list_claude_process_markers(&store_path, &session_id)
            .unwrap()
            .len(),
        1
    );
    fs::remove_dir_all(root).unwrap();
}
