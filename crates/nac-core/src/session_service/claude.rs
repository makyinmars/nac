use super::*;
use crate::claude_agent::{self, RunRequest, Target};
use crate::model::TokenUsage;
use crate::tools::thread::claude_worker::{assistant_events, assistant_usage};
use std::collections::HashMap;

impl SessionService {
    pub(super) async fn settle_claude_process_markers(&self) -> Result<()> {
        if !self.durable_session_row_present {
            return Ok(());
        }
        let service = self.clone();
        tokio::task::spawn_blocking(move || service.reconcile_claude_processes_under_lease())
            .await
            .map_err(|error| anyhow::anyhow!("Claude cleanup task failed: {error}"))?
    }

    pub(super) async fn execute_claude_run(
        &self,
        prompt: &str,
        run_id: &SessionRunId,
        prompt_commit: watch::Sender<RunPromptCommitStatus>,
        inbox_item_id: Option<i64>,
        client_id: Option<SessionClientId>,
    ) -> (
        std::result::Result<String, crate::run_failure::RunFailure>,
        Option<TokenUsage>,
    ) {
        let result = self
            .execute_claude_run_inner(prompt, run_id, &prompt_commit, inbox_item_id, client_id)
            .await;
        match result {
            Ok((answer, usage)) => (Ok(answer), usage),
            Err(error) => {
                if *prompt_commit.borrow() == RunPromptCommitStatus::Pending {
                    prompt_commit.send_replace(RunPromptCommitStatus::Failed);
                }
                if error.to_string() != "Claude run was cancelled" {
                    if let Err(append_error) = self
                        .append_claude_terminal_marker(
                            crate::agent::RUN_FAILED_PARTIAL_MARKER,
                            false,
                        )
                        .await
                    {
                        eprintln!("nac: failed to retain partial Claude output: {append_error:#}");
                    }
                }
                (
                    Err(crate::run_failure::RunFailure::unknown(
                        claude_agent::sanitize_output_text(&error.to_string(), 1024),
                    )),
                    None,
                )
            }
        }
    }

    async fn execute_claude_run_inner(
        &self,
        prompt: &str,
        run_id: &SessionRunId,
        prompt_commit: &watch::Sender<RunPromptCommitStatus>,
        inbox_item_id: Option<i64>,
        client_id: Option<SessionClientId>,
    ) -> Result<(String, Option<TokenUsage>)> {
        let engine = Arc::clone(
            self.claude_engine()
                .ok_or_else(|| anyhow::anyhow!("Claude engine is unavailable"))?,
        );
        engine
            .partial_output
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        let session_id = self
            .metadata
            .session_id
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Claude session id is unavailable"))?
            .clone();
        let writer = Arc::clone(
            self.transcript_log
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Claude transcript writer is unavailable"))?,
        );
        let idx = self.transcript_len().await?;
        let prompt_message = Message::User {
            content: prompt.to_string(),
        };
        let prompt_run_id = run_id.to_string();
        let prompt_session_id = session_id.clone();
        tokio::task::spawn_blocking(move || match inbox_item_id {
            Some(item_id) => writer.append_inbox_run_prompt(
                &prompt_session_id,
                idx,
                &prompt_message,
                &prompt_run_id,
                item_id,
            ),
            None => {
                writer.append_run_prompt(&prompt_session_id, idx, &prompt_message, &prompt_run_id)
            }
        })
        .await??;
        prompt_commit.send_replace(RunPromptCommitStatus::Committed);
        self.event_bus.emit_with_context(
            SessionEvent::TranscriptAppended {
                transcript_len: idx + 1,
            },
            Some(run_id.clone()),
            client_id.clone(),
        );
        self.event_bus.emit_agent_with_context(
            AgentEvent::RunStarted {
                thread_name: None,
                prompt_preview: prompt.chars().take(160).collect(),
            },
            Some(run_id.clone()),
            client_id.clone(),
        );

        let resume_id = sessions::load_session(&self.metadata.store_path, &session_id)?
            .claude_agent
            .and_then(|config| config.native_session_id);
        let request = RunRequest {
            target: engine.target.clone(),
            cwd: engine.cwd.clone(),
            executable: engine.config.executable.clone(),
            config_dir: engine.config.config_dir.as_ref().map(PathBuf::from),
            model: engine.config.model.clone(),
            resume_id,
            prompt: prompt.to_string(),
            run_id: run_id.to_string(),
            generation: 0,
        };
        let (pidfile, host_id, ssh_port, ssh_identity_file) = match &request.target {
            Target::Local => {
                let handle = claude_agent::local_process_handle(&request).ok_or_else(|| {
                    anyhow::anyhow!("cannot derive local Claude process identity")
                })?;
                (handle.pidfile.display().to_string(), None, None, None)
            }
            Target::Ssh(connection) => {
                let handle = claude_agent::remote_process_handle(&request)
                    .ok_or_else(|| anyhow::anyhow!("cannot derive SSH Claude process identity"))?;
                (
                    handle.pidfile,
                    Some(connection.host.clone()),
                    connection.port,
                    connection
                        .identity_file
                        .as_ref()
                        .map(|path| path.display().to_string()),
                )
            }
        };
        crate::store::insert_claude_process_marker(
            &self.metadata.store_path,
            &crate::store::ClaudeProcessMarker {
                session_id: session_id.clone(),
                operation_id: run_id.to_string(),
                generation: 0,
                kind: crate::store::ClaudeProcessKind::Session,
                thread_name: None,
                host_id,
                ssh_port,
                ssh_identity_file,
                workspace: engine.cwd.clone(),
                config_dir: engine.config.config_dir.clone(),
                pidfile,
                native_session_id: request.resume_id.clone(),
            },
        )?;

        let cancellation = {
            let guard = self.lock_active_operation();
            match guard.as_ref() {
                Some(ActiveSessionOperation::Run(active)) if &active.snapshot.run_id == run_id => {
                    active.command_cancellation.clone()
                }
                _ => return Err(anyhow::anyhow!("Claude run generation is no longer active")),
            }
        };
        let (cancel_sender, cancel_receiver) = watch::channel(cancellation.is_cancelled());
        let cancellation_task = tokio::spawn(async move {
            cancellation.cancelled().await;
            let _ = cancel_sender.send(true);
        });
        let broker = Arc::clone(
            self.claude_approval_broker
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Claude approval channel is unavailable"))?,
        );
        broker.activate_with_scope(
            run_id.as_str(),
            0,
            crate::claude_approval::ClaudeApprovalScope {
                target: request.target.clone(),
                workspace: PathBuf::from(&engine.cwd),
                store_path: Some(self.metadata.store_path.clone()),
            },
        );
        let init_store = self.metadata.store_path.clone();
        let init_session = session_id.clone();
        let init_run = run_id.to_string();
        let init_snapshot = Arc::clone(&self.session_snapshot);
        let event_bus = self.event_bus.clone();
        let event_run = run_id.clone();
        let event_client = client_id.clone();
        let active_tools = Arc::new(Mutex::new(HashMap::<String, String>::new()));
        let event_tools = Arc::clone(&active_tools);
        let usage = Arc::new(Mutex::new(TokenUsage::default()));
        let event_usage = Arc::clone(&usage);
        let event_partial = Arc::clone(&engine.partial_output);
        let approval_broker = Arc::clone(&broker);
        let approval_cancel = cancel_receiver.clone();
        let result = claude_agent::run(
            request,
            cancel_receiver,
            move |native_id| {
                let store_path = init_store.clone();
                let session_id = init_session.clone();
                let run_id = init_run.clone();
                let snapshot = Arc::clone(&init_snapshot);
                async move {
                    let native_copy = native_id.clone();
                    tokio::task::spawn_blocking(move || {
                        sessions::save_claude_native_session_id(
                            &store_path,
                            &session_id,
                            &native_copy,
                        )?;
                        crate::store::update_claude_process_native_session_id(
                            &store_path,
                            &session_id,
                            &run_id,
                            0,
                            &native_copy,
                        )
                    })
                    .await??;
                    if let Some(config) = snapshot
                        .lock()
                        .await
                        .as_mut()
                        .and_then(|snapshot| snapshot.claude_agent.as_mut())
                    {
                        config.native_session_id = Some(native_id);
                    }
                    Ok(())
                }
            },
            move |message| {
                let bus = event_bus.clone();
                let run_id = event_run.clone();
                let client_id = event_client.clone();
                let tools = Arc::clone(&event_tools);
                let usage = Arc::clone(&event_usage);
                let partial = Arc::clone(&event_partial);
                async move {
                    if let Some(tokens) = assistant_usage(&message) {
                        usage.lock().await.add_cost_saturating(&tokens);
                    }
                    if let Some(text) = message
                        .get("event")
                        .and_then(|event| event.get("delta"))
                        .or_else(|| message.get("delta"))
                        .and_then(|delta| delta.get("text"))
                        .and_then(serde_json::Value::as_str)
                    {
                        let mut retained = partial
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        if retained.len() < 64_000 {
                            let remaining = 64_000 - retained.len();
                            retained.extend(text.chars().take(remaining));
                        }
                        drop(retained);
                        // Keep split deltas only in this bounded, private
                        // buffer. The complete assistant event and final
                        // result cross the event/persistence boundary after
                        // exact-value redaction.
                    }
                    for event in assistant_events(&message, None, &tools).await {
                        bus.emit_agent_with_context(event, Some(run_id.clone()), client_id.clone());
                    }
                    Ok(())
                }
            },
            move |request| {
                let broker = Arc::clone(&approval_broker);
                let cancelled = approval_cancel.clone();
                async move { Ok(broker.ask(request, cancelled).await) }
            },
        )
        .await;
        broker.close_scope(run_id.as_str(), 0);
        cancellation_task.abort();
        let outcome = result?;
        if outcome.cancelled {
            anyhow::bail!("Claude run was cancelled");
        }
        if outcome.is_error {
            anyhow::bail!(
                "Claude Agent failed: {}",
                outcome.result.chars().take(1024).collect::<String>()
            );
        }
        let answer = claude_agent::sanitize_output_text(&outcome.result, 64_000);
        let idx = self.transcript_len().await?;
        let writer = Arc::clone(
            self.transcript_log
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Claude transcript writer is unavailable"))?,
        );
        let append_session = session_id.clone();
        let append_answer = answer.clone();
        tokio::task::spawn_blocking(move || {
            writer.append(
                &append_session,
                idx,
                &Message::Assistant {
                    content: Some(append_answer),
                    reasoning_text: None,
                    reasoning_details: None,
                    tool_calls: None,
                    duration_ms: None,
                    model_origin: None,
                    reasoning_field: None,
                },
            )
        })
        .await??;
        engine
            .partial_output
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        // The assistant row is the canonical terminal evidence for crash
        // recovery. Keep the process marker until that row commits, so a
        // successor never mistakes an unrecorded result for a settled turn.
        crate::store::clear_claude_process_marker(
            &self.metadata.store_path,
            &session_id,
            run_id.as_str(),
            0,
        )?;
        self.event_bus.emit_with_context(
            SessionEvent::TranscriptAppended {
                transcript_len: idx + 1,
            },
            Some(run_id.clone()),
            client_id.clone(),
        );
        let usage = usage.lock().await.clone();
        let has_usage = usage.input_tokens > 0
            || usage.output_tokens > 0
            || usage.cache_read_tokens > 0
            || usage.cache_write_tokens > 0;
        self.event_bus.emit_agent_with_context(
            AgentEvent::RunFinished { thread_name: None },
            Some(run_id.clone()),
            client_id,
        );
        Ok((answer, has_usage.then_some(usage)))
    }

    pub(super) async fn append_claude_terminal_marker(
        &self,
        marker: &str,
        always: bool,
    ) -> Result<()> {
        let Some(engine) = self.claude_engine() else {
            return Ok(());
        };
        let partial = std::mem::take(
            &mut *engine
                .partial_output
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        if partial.is_empty() && !always {
            return Ok(());
        }
        let content = if partial.is_empty() {
            marker.to_string()
        } else {
            let safe_partial = claude_agent::sanitize_output_text(&partial, 63_000);
            format!("{safe_partial}\n\n{marker}")
        };
        let session_id = self
            .metadata
            .session_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Claude session id is unavailable"))?;
        let writer = Arc::clone(
            self.transcript_log
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Claude transcript writer is unavailable"))?,
        );
        let idx = self.transcript_len().await?;
        let session_id = session_id.to_string();
        let append = tokio::task::spawn_blocking(move || {
            writer.append(
                &session_id,
                idx,
                &Message::Assistant {
                    content: Some(content),
                    reasoning_text: None,
                    reasoning_details: None,
                    tool_calls: None,
                    duration_ms: None,
                    model_origin: None,
                    reasoning_field: None,
                },
            )
        })
        .await?;
        if let Err(error) = append {
            let mut retained = engine
                .partial_output
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if retained.is_empty() {
                *retained = partial;
            }
            return Err(error);
        }
        self.event_bus.emit(SessionEvent::TranscriptAppended {
            transcript_len: idx + 1,
        });
        Ok(())
    }
    /// Reconcile every supervised Claude process left by a crashed owner
    /// before a successor can enter the same workspace. The caller holds the
    /// session operation lease. An unreachable SSH host keeps its marker and
    /// blocks admission rather than falling back to local execution.
    pub(super) fn reconcile_claude_processes_under_lease(&self) -> Result<()> {
        if !self.durable_session_row_present {
            return Ok(());
        }
        let Some(session_id) = self.metadata.session_id.as_deref() else {
            return Ok(());
        };
        let markers =
            crate::store::list_claude_process_markers(&self.metadata.store_path, session_id)?;
        if markers.is_empty() {
            return Ok(());
        }
        let snapshot = sessions::load_session(&self.metadata.store_path, session_id)?;
        for marker in markers {
            let matching_host = match snapshot.ssh.as_ref() {
                Some(connection) => {
                    marker.host_id.as_deref() == Some(connection.host.as_str())
                        && marker.ssh_port == connection.port
                        && marker.ssh_identity_file.as_deref()
                            == connection.identity_file.as_deref().and_then(Path::to_str)
                }
                None => marker.host_id.is_none(),
            };
            if marker.workspace != snapshot.cwd || !matching_host {
                anyhow::bail!(
                    "Claude process {} has a different host/workspace binding; inspect the durable process marker before resuming",
                    marker.operation_id
                );
            }
            if marker.kind == crate::store::ClaudeProcessKind::Session
                && marker.config_dir
                    != snapshot
                        .claude_agent
                        .as_ref()
                        .and_then(|config| config.config_dir.clone())
            {
                anyhow::bail!(
                    "Claude process {} has a different configuration binding; inspect the durable process marker before resuming",
                    marker.operation_id
                );
            }
            match snapshot.ssh.as_ref() {
                Some(connection) => crate::claude_agent::reconcile_remote_process_blocking(
                    &crate::claude_agent::RemoteProcessHandle {
                        connection: connection.clone(),
                        pidfile: marker.pidfile.clone(),
                    },
                )?,
                None => crate::claude_agent::reconcile_local_process_blocking(
                    &crate::claude_agent::LocalProcessHandle {
                        pidfile: PathBuf::from(&marker.pidfile),
                    },
                )?,
            }
            if marker.kind == crate::store::ClaudeProcessKind::Worker {
                crate::store::reconcile_worker_claude_marker_identity(
                    &self.metadata.store_path,
                    &marker,
                )?;
            }
            crate::store::clear_claude_process_marker(
                &self.metadata.store_path,
                session_id,
                &marker.operation_id,
                marker.generation,
            )?;
        }
        Ok(())
    }

    pub fn list_claude_permission_requests(
        &self,
    ) -> Vec<crate::claude_approval::ClaudePermissionRequest> {
        self.claude_approval_broker
            .as_ref()
            .map_or_else(Vec::new, |broker| broker.pending())
    }

    pub fn reply_claude_permission_request(
        &self,
        request_id: &str,
        run_id: &str,
        generation: u64,
        reply: crate::claude_approval::ClaudePermissionReply,
    ) -> Result<()> {
        self.claude_approval_broker
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Claude approval channel is unavailable"))?
            .reply(request_id, run_id, generation, reply)
    }
}
