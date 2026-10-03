use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::watch;
use uuid::Uuid;

use super::{
    preflight, reconcile_remote_process, reconcile_remote_process_blocking, remote_process_handle,
    run, ApprovalDecision, RunRequest, SshConnection, Target,
};

fn fixture() -> Result<(PathBuf, RunRequest)> {
    let root = std::env::temp_dir().join(format!("nac-claude-agent-test-{}", Uuid::new_v4()));
    fs::create_dir_all(root.join("node_modules"))?;
    let cli = root.join("claude-fake");
    fs::write(&cli, "#!/bin/sh\ncase \"$1\" in\n  --version) printf '2.1.283 (Claude Code)\\n' ;;\n  auth) printf '{\"loggedIn\":true,\"authMethod\":\"claude.ai\",\"apiProvider\":\"firstParty\"}\\n' ;;\nesac\n")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&cli, fs::Permissions::from_mode(0o700))?;
    }
    let request = RunRequest {
        target: Target::Local,
        cwd: root.clone(),
        executable: cli.display().to_string(),
        config_dir: None,
        model: None,
        resume_id: None,
        prompt: "test".to_string(),
        run_id: Uuid::new_v4().to_string(),
        generation: 1,
    };
    Ok((root, request))
}

fn install_bridge(root: &Path, source: &str) -> Result<PathBuf> {
    let bridge = root.join("bridge.mjs");
    fs::write(&bridge, source)?;
    Ok(bridge)
}

#[tokio::test]
async fn older_logged_in_cli_is_rejected_before_run() -> Result<()> {
    let _env = crate::TEST_ENV_LOCK.lock().unwrap();
    let (root, request) = fixture()?;
    fs::write(
        &request.executable,
        "#!/bin/sh\ncase \"$1\" in\n  --version) printf '2.1.274 (Claude Code)\\n' ;;\n  auth) printf '{\"loggedIn\":true,\"authMethod\":\"claude.ai\",\"apiProvider\":\"firstParty\"}\\n' ;;\nesac\n",
    )?;
    let status = preflight(&request.target, &root, &request.executable, None).await?;
    assert!(status.available);
    assert!(!status.authenticated);
    assert!(status.reason.unwrap().contains("2.1.280"));
    fs::remove_dir_all(root)?;
    Ok(())
}

#[tokio::test]
async fn persists_init_before_forwarding_events_and_denies_approval() -> Result<()> {
    let _env = crate::TEST_ENV_LOCK.lock().unwrap();
    let (root, request) = fixture()?;
    let bridge = install_bridge(
        &root,
        r#"
        import { createInterface } from 'node:readline';
        const lines = createInterface({input: process.stdin});
        let started = false;
        lines.on('line', line => {
          const item = JSON.parse(line);
          if (!started) {
            started = true;
            console.log(JSON.stringify({type:'init',session_id:'ccae5920-848e-4ab1-a950-195ab345a379'}));
            console.log(JSON.stringify({type:'event',event:{type:'assistant',message:{content:[{type:'text',text:'partial'}]}}}));
            console.log(JSON.stringify({type:'approval_request',id:'one',tool_use_id:'tool-native-one',tool_name:'Bash',input:{command:'true'}}));
          } else if (item.type === 'approval') {
            console.log(JSON.stringify({type:'result',session_id:'ccae5920-848e-4ab1-a950-195ab345a379',result:item.allow?'wrong':'denied',is_error:false}));
            lines.close();
            process.stdin.pause();
          }
        });
    "#,
    )?;
    unsafe {
        std::env::set_var("NAC_TEST_CLAUDE_BRIDGE", &bridge);
    }
    let (_signal, cancelled) = watch::channel(false);
    let initialized = Arc::new(AtomicBool::new(false));
    let on_init_flag = Arc::clone(&initialized);
    let on_event_flag = Arc::clone(&initialized);
    let outcome = run(
        request,
        cancelled,
        move |_| {
            on_init_flag.store(true, Ordering::SeqCst);
            async { Ok(()) }
        },
        move |_| {
            assert!(on_event_flag.load(Ordering::SeqCst));
            async { Ok(()) }
        },
        |approval| async move {
            assert_eq!(approval.tool_name, "Bash");
            assert_eq!(approval.tool_use_id, "tool-native-one");
            assert_eq!(approval.generation, 1);
            Ok(ApprovalDecision::Deny("test denial".to_string()))
        },
    )
    .await?;
    assert_eq!(outcome.result, "denied");
    assert!(!outcome.cancelled);
    unsafe {
        std::env::remove_var("NAC_TEST_CLAUDE_BRIDGE");
    }
    fs::remove_dir_all(root)?;
    Ok(())
}

#[tokio::test]
async fn cancellation_stops_supervised_local_bridge() -> Result<()> {
    let _env = crate::TEST_ENV_LOCK.lock().unwrap();
    let (root, request) = fixture()?;
    let bridge = install_bridge(
        &root,
        r#"
        import { createInterface } from 'node:readline';
        const lines = createInterface({input: process.stdin});
        lines.once('line', () => {
          console.log(JSON.stringify({type:'init',session_id:'ccae5920-848e-4ab1-a950-195ab345a379'}));
          setInterval(() => {}, 1000);
        });
    "#,
    )?;
    unsafe {
        std::env::set_var("NAC_TEST_CLAUDE_BRIDGE", &bridge);
    }
    let (signal, cancelled) = watch::channel(false);
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        run(
            request,
            cancelled,
            move |_| {
                let signal = signal.clone();
                async move {
                    signal.send(true)?;
                    Ok(())
                }
            },
            |_| async { Ok(()) },
            |_| async { Ok(ApprovalDecision::Deny("cancelled".to_string())) },
        ),
    )
    .await??;
    assert!(result.cancelled);
    unsafe {
        std::env::remove_var("NAC_TEST_CLAUDE_BRIDGE");
    }
    fs::remove_dir_all(root)?;
    Ok(())
}

#[tokio::test]
async fn missing_native_transcript_refuses_resume_before_bridge_launch() -> Result<()> {
    let _env = crate::TEST_ENV_LOCK.lock().unwrap();
    let (root, mut request) = fixture()?;
    request.resume_id = Some(Uuid::new_v4().to_string());
    request.config_dir = Some(root.join("empty-config"));
    let bridge = install_bridge(&root, "process.exit(99);")?;
    unsafe {
        std::env::set_var("NAC_TEST_CLAUDE_BRIDGE", &bridge);
    }
    let (_signal, cancelled) = watch::channel(false);
    let error = run(
        request,
        cancelled,
        |_| async { Ok(()) },
        |_| async { Ok(()) },
        |_| async { Ok(ApprovalDecision::Deny("no approver".to_string())) },
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("native transcript"));
    unsafe {
        std::env::remove_var("NAC_TEST_CLAUDE_BRIDGE");
    }
    fs::remove_dir_all(root)?;
    Ok(())
}

#[tokio::test]
async fn unauthenticated_cli_refuses_run_before_bridge_launch() -> Result<()> {
    let _env = crate::TEST_ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (root, request) = fixture()?;
    fs::write(&request.executable, "#!/bin/sh\ncase \"$1\" in\n  --version) printf '2.1.283 (Claude Code)\\n' ;;\n  auth) printf '{\"loggedIn\":false}\\n' ;;\nesac\n")?;
    let bridge = install_bridge(&root, "process.exit(99);")?;
    unsafe {
        std::env::set_var("NAC_TEST_CLAUDE_BRIDGE", &bridge);
    }
    let (_signal, cancelled) = watch::channel(false);
    let error = run(
        request,
        cancelled,
        |_| async { Ok(()) },
        |_| async { Ok(()) },
        |_| async { Ok(ApprovalDecision::Deny("no approver".to_string())) },
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("subscription login"));
    unsafe {
        std::env::remove_var("NAC_TEST_CLAUDE_BRIDGE");
    }
    fs::remove_dir_all(root)?;
    Ok(())
}

fn fake_ssh(root: &Path, override_key: bool) -> Result<PathBuf> {
    let path = root.join("ssh-fake");
    let prefix = if override_key {
        "export ANTHROPIC_API_KEY=probe\n"
    } else {
        ""
    };
    fs::write(&path, format!("#!/bin/sh\nprintf invoked >> '{}'\nwhile [ \"$1\" != -- ]; do shift; done\nshift\nshift\n{prefix}exec sh -c \"$1\"\n", root.join("ssh-invoked").display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(path)
}

#[tokio::test]
async fn ssh_preflight_uses_selected_host_and_rejects_remote_api_key_override() -> Result<()> {
    let _env = crate::TEST_ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (root, request) = fixture()?;
    let ssh = fake_ssh(&root, false)?;
    unsafe {
        std::env::set_var("NAC_TEST_CLAUDE_SSH_PROGRAM", &ssh);
        std::env::set_var("ANTHROPIC_API_KEY", "local-nac-provider-key");
    }
    let target = Target::Ssh(SshConnection::new("fake-host"));
    let status = preflight(&target, &root, &request.executable, None).await?;
    assert!(status.authenticated);
    unsafe { std::env::remove_var("ANTHROPIC_API_KEY") };
    fake_ssh(&root, true)?;
    let error = preflight(&target, &root, &request.executable, None)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("Claude Code unavailable"));
    unsafe {
        std::env::remove_var("NAC_TEST_CLAUDE_SSH_PROGRAM");
    }
    fs::remove_dir_all(root)?;
    Ok(())
}

#[tokio::test]
async fn subscription_preflight_rejects_local_and_remote_oauth_token_overrides() -> Result<()> {
    let _env = crate::TEST_ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (root, request) = fixture()?;
    unsafe { std::env::set_var("CLAUDE_CODE_OAUTH_TOKEN", "test-only-token") };
    let local_error = preflight(&Target::Local, &root, &request.executable, None)
        .await
        .unwrap_err();
    assert!(local_error.to_string().contains("CLAUDE_CODE_OAUTH_TOKEN"));
    unsafe { std::env::remove_var("CLAUDE_CODE_OAUTH_TOKEN") };

    let ssh = fake_ssh(&root, false)?;
    fs::write(
        &ssh,
        "#!/bin/sh\nwhile [ \"$1\" != -- ]; do shift; done\nshift\nshift\nexport CLAUDE_CODE_OAUTH_TOKEN=test-only-token\nexec sh -c \"$1\"\n",
    )?;
    unsafe { std::env::set_var("NAC_TEST_CLAUDE_SSH_PROGRAM", &ssh) };
    let target = Target::Ssh(SshConnection::new("fake-host"));
    let remote_error = preflight(&target, &root, &request.executable, None)
        .await
        .unwrap_err();
    assert!(remote_error.to_string().contains("Claude Code unavailable"));
    unsafe { std::env::remove_var("NAC_TEST_CLAUDE_SSH_PROGRAM") };
    fs::remove_dir_all(root)?;
    Ok(())
}

#[tokio::test]
async fn ssh_resume_requires_parsed_top_level_workspace_identity() -> Result<()> {
    let _env = crate::TEST_ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (root, mut request) = fixture()?;
    let ssh = fake_ssh(&root, false)?;
    unsafe { std::env::set_var("NAC_TEST_CLAUDE_SSH_PROGRAM", &ssh) };
    request.target = Target::Ssh(SshConnection::new("fake-host"));
    request.resume_id = Some(Uuid::new_v4().to_string());
    let project = root.join("config/projects/project");
    fs::create_dir_all(&project)?;
    request.config_dir = Some(root.join("config"));
    let transcript = project.join(format!("{}.jsonl", request.resume_id.as_deref().unwrap()));
    let cwd = fs::canonicalize(&root)?;
    fs::write(
        &transcript,
        format!(
            "{{ \"cwd\" : {} }}\n",
            serde_json::to_string(&cwd.display().to_string())?
        ),
    )?;
    super::preflight::verify_native_transcript(&request).await?;
    fs::write(
        &transcript,
        format!(
            "{{\"message\":{{\"cwd\":{}}}}}\n",
            serde_json::to_string(&cwd.display().to_string())?
        ),
    )?;
    assert!(super::preflight::verify_native_transcript(&request)
        .await
        .is_err());
    unsafe { std::env::remove_var("NAC_TEST_CLAUDE_SSH_PROGRAM") };
    fs::remove_dir_all(root)?;
    Ok(())
}

#[tokio::test]
async fn ssh_subprocess_does_not_inherit_local_claude_credentials_or_config() -> Result<()> {
    let _env = crate::TEST_ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (root, _) = fixture()?;
    let ssh = root.join("ssh-env-check");
    fs::write(&ssh, "#!/bin/sh\n[ -z \"${ANTHROPIC_API_KEY+x}\" ] && [ -z \"${CLAUDE_CODE_OAUTH_TOKEN+x}\" ] && [ -z \"${CLAUDE_CODE_SESSION_ACCESS_TOKEN+x}\" ] && [ -z \"${CLAUDE_CONFIG_DIR+x}\" ]\n")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700))?;
    }
    unsafe {
        std::env::set_var("NAC_TEST_CLAUDE_SSH_PROGRAM", &ssh);
        std::env::set_var("CLAUDE_CODE_OAUTH_TOKEN", "test-only-token");
        std::env::set_var("CLAUDE_CODE_SESSION_ACCESS_TOKEN", "test-only-session");
        std::env::set_var("CLAUDE_CONFIG_DIR", "/test-only-config");
        std::env::set_var("ANTHROPIC_API_KEY", "local-nac-provider-key");
    }
    let mut command = super::process::ssh_command(&SshConnection::new("fake-host"), "true")?;
    let status = command.status().await?;
    unsafe {
        std::env::remove_var("NAC_TEST_CLAUDE_SSH_PROGRAM");
        std::env::remove_var("CLAUDE_CODE_OAUTH_TOKEN");
        std::env::remove_var("CLAUDE_CODE_SESSION_ACCESS_TOKEN");
        std::env::remove_var("CLAUDE_CONFIG_DIR");
        std::env::remove_var("ANTHROPIC_API_KEY");
    }
    assert!(
        status.success(),
        "SSH subprocess inherited local Claude credentials"
    );
    fs::remove_dir_all(root)?;
    Ok(())
}

#[tokio::test]
async fn failed_ssh_preflight_never_falls_back_to_local_claude() -> Result<()> {
    let _env = crate::TEST_ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (root, mut request) = fixture()?;
    let ssh = root.join("ssh-fail");
    fs::write(&ssh, "#!/bin/sh\nexit 44\n")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700))?;
    }
    unsafe {
        std::env::set_var("NAC_TEST_CLAUDE_SSH_PROGRAM", &ssh);
    }
    request.target = Target::Ssh(SshConnection::new("fake-host"));
    let (_signal, cancelled) = watch::channel(false);
    let error = run(
        request,
        cancelled,
        |_| async { panic!("SSH failure must not initialize a local Claude") },
        |_| async { Ok(()) },
        |_| async { Ok(ApprovalDecision::Deny("no approver".to_string())) },
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("Claude Code unavailable"));
    unsafe {
        std::env::remove_var("NAC_TEST_CLAUDE_SSH_PROGRAM");
    }
    fs::remove_dir_all(root)?;
    Ok(())
}

#[tokio::test]
async fn ssh_process_handle_reconciles_via_target_and_retains_errors() -> Result<()> {
    let _env = crate::TEST_ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (root, mut request) = fixture()?;
    request.target = Target::Ssh(SshConnection::new("fake-host"));
    let handle = remote_process_handle(&request).expect("SSH run has durable handle");
    assert!(handle.pidfile.contains("claude-"));
    let ssh = fake_ssh(&root, false)?;
    unsafe {
        std::env::set_var("NAC_TEST_CLAUDE_SSH_PROGRAM", &ssh);
    }
    reconcile_remote_process(&handle).await?;
    assert!(fs::read_to_string(root.join("ssh-invoked"))?.contains("invoked"));
    fs::write(&ssh, "#!/bin/sh\nexit 44\n")?;
    assert!(reconcile_remote_process_blocking(&handle).is_err());
    unsafe {
        std::env::remove_var("NAC_TEST_CLAUDE_SSH_PROGRAM");
    }
    fs::remove_dir_all(root)?;
    Ok(())
}
