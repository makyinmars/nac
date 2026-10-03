use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;

use tokio::sync::watch;

use crate::claude_agent::{self, RunRequest, Target};
use crate::events::AgentEvent;
use crate::model::TokenUsage;
use crate::store::{self, WorkerContext};
use crate::tools::{ThreadCancellation, ToolRuntime};

use super::worker::{WorkerInvocation, WorkerRun};

const MAX_HANDOFF_CHARS: usize = 64_000;

pub(super) async fn run(
    runtime: &ToolRuntime,
    invocation: WorkerInvocation<'_>,
    cancellation: ThreadCancellation,
    mut binding: store::ClaudeThreadBinding,
) -> io::Result<WorkerRun> {
    let store_path = runtime.store_path.clone();
    let session_id = invocation.session_id.to_string();
    let dispatch_id = invocation.dispatch_id.to_string();
    let (committed, active_markers) = tokio::task::spawn_blocking(move || {
        let committed = store::load_episode_for_dispatch(&store_path, &session_id, &dispatch_id)?;
        let active_markers = store::list_claude_process_markers(&store_path, &session_id)?
            .into_iter()
            .any(|marker| marker.operation_id == dispatch_id);
        anyhow::Ok((committed, active_markers))
    })
    .await
    .map_err(io::Error::other)?
    .map_err(io::Error::other)?;
    if active_markers {
        return Err(io::Error::other(
            "Claude dispatch process recovery must finish before replay",
        ));
    }
    if let Some(committed) = committed {
        if committed.thread_name != invocation.thread_name || committed.action != invocation.action
        {
            return Err(io::Error::other(
                "Claude dispatch ID belongs to a different thread or action",
            ));
        }
        return Ok(WorkerRun {
            stdout: committed.content,
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
            cancelled: false,
            timeout_reason: None,
            usage: None,
            model_error: None,
            cleanup_error: None,
        });
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(invocation.timeout_secs);
    let mut prompt_override: Option<String> = None;
    let mut pending_steering: Vec<store::ThreadSteeringRecord> = Vec::new();
    let mut total_usage = TokenUsage::default();
    let mut completed_generations = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Ok(WorkerRun {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 1,
                timed_out: true,
                cancelled: false,
                timeout_reason: Some("The Claude worker exceeded its dispatch timeout".to_string()),
                usage: Some(total_usage),
                model_error: None,
                cleanup_error: None,
            });
        }
        let turn_invocation = WorkerInvocation {
            timeout_secs: remaining.as_secs().max(1),
            ..invocation
        };
        let mut turn = run_turn(
            runtime,
            turn_invocation,
            cancellation.clone(),
            binding.clone(),
            prompt_override.as_deref(),
            completed_generations.len() as u64,
        )
        .await?;
        if let Some(usage) = turn.usage.take() {
            total_usage.add_cost_saturating(&usage);
        }
        turn.usage = Some(total_usage.clone());
        if turn.timed_out || turn.cancelled || turn.exit_code != 0 {
            return Ok(turn);
        }
        completed_generations.push(completed_generations.len() as i64);
        if cancellation.is_cancelled() {
            turn.cancelled = true;
            return Ok(turn);
        }
        if tokio::time::Instant::now() >= deadline {
            turn.timed_out = true;
            turn.timeout_reason =
                Some("The Claude worker exceeded its dispatch timeout".to_string());
            return Ok(turn);
        }
        if !pending_steering.is_empty() {
            let ids: Vec<i64> = pending_steering.iter().map(|record| record.id).collect();
            let store_path = runtime.store_path.clone();
            let session_id = invocation.session_id.to_string();
            let dispatch_id = invocation.dispatch_id.to_string();
            tokio::task::spawn_blocking(move || {
                store::acknowledge_thread_steering_batch(
                    &store_path,
                    &ids,
                    &session_id,
                    &dispatch_id,
                )
            })
            .await
            .map_err(io::Error::other)?
            .map_err(io::Error::other)?;
            for steering in pending_steering.drain(..) {
                runtime
                    .event_sink
                    .emit(AgentEvent::ThreadSteeringDelivered {
                        name: invocation.thread_name.to_string(),
                        steering_id: steering.id,
                        instruction_preview: steering.instruction.chars().take(160).collect(),
                    });
            }
        }
        let store_path = runtime.store_path.clone();
        let session_id = invocation.session_id.to_string();
        let dispatch_id = invocation.dispatch_id.to_string();
        let queued = tokio::task::spawn_blocking(move || {
            store::claim_thread_steering(&store_path, &session_id, &dispatch_id)
        })
        .await
        .map_err(io::Error::other)?
        .map_err(io::Error::other)?;
        if queued.is_empty() {
            if cancellation.is_cancelled() {
                turn.cancelled = true;
                return Ok(turn);
            }
            let committed = commit_handoff(runtime, &invocation, &turn.stdout).await?;
            turn.stdout = committed.content;
            for generation in completed_generations {
                let store_path = runtime.store_path.clone();
                let session_id = invocation.session_id.to_string();
                let dispatch_id = invocation.dispatch_id.to_string();
                tokio::task::spawn_blocking(move || {
                    store::clear_claude_process_marker(
                        &store_path,
                        &session_id,
                        &dispatch_id,
                        generation,
                    )
                })
                .await
                .map_err(io::Error::other)?
                .map_err(io::Error::other)?;
            }
            return Ok(turn);
        }
        prompt_override = Some(steering_prompt(&queued));
        pending_steering = queued;
        let store_path = runtime.store_path.clone();
        let session_id = invocation.session_id.to_string();
        let thread_name = invocation.thread_name.to_string();
        binding = tokio::task::spawn_blocking(move || {
            store::load_thread_claude_binding(&store_path, &session_id, &thread_name)?.ok_or_else(
                || anyhow::anyhow!("Claude thread binding disappeared before queued steering"),
            )
        })
        .await
        .map_err(io::Error::other)?
        .map_err(io::Error::other)?;
        if binding.native_session_id.is_none() {
            return Err(io::Error::other(
                "Claude worker cannot resume queued steering without a native session ID",
            ));
        }
    }
}

fn steering_prompt(records: &[store::ThreadSteeringRecord]) -> String {
    let mut prompt = String::from("Apply these queued steering instructions to the work you just completed, then return the updated final handoff:");
    for record in records {
        prompt.push_str(&format!(
            "\n\nSteering #{}:\n{}",
            record.id, record.instruction
        ));
    }
    prompt
}

async fn commit_handoff(
    runtime: &ToolRuntime,
    invocation: &WorkerInvocation<'_>,
    answer: &str,
) -> io::Result<store::EpisodeRecord> {
    let store_path = runtime.store_path.clone();
    let session_id = invocation.session_id.to_string();
    let thread_name = invocation.thread_name.to_string();
    let dispatch_id = invocation.dispatch_id.to_string();
    let action = invocation.action.to_string();
    let answer = answer.to_string();
    tokio::task::spawn_blocking(move || {
        store::append_episode_for_dispatch_once(
            &store_path,
            &session_id,
            &thread_name,
            &dispatch_id,
            &action,
            &answer,
        )
    })
    .await
    .map_err(io::Error::other)?
    .map_err(io::Error::other)
}

async fn run_turn(
    runtime: &ToolRuntime,
    invocation: WorkerInvocation<'_>,
    cancellation: ThreadCancellation,
    binding: store::ClaudeThreadBinding,
    prompt_override: Option<&str>,
    generation: u64,
) -> io::Result<WorkerRun> {
    let broker = runtime
        .claude_approval_broker
        .clone()
        .ok_or_else(|| io::Error::other("Claude worker approval channel is unavailable"))?;
    let prompt = match prompt_override {
        Some(prompt) => prompt.to_string(),
        None => build_prompt(runtime, &invocation)
            .await
            .map_err(io::Error::other)?,
    };
    let target = match runtime.backend.ssh_connection() {
        Some(connection) => Target::Ssh(connection.clone()),
        None => Target::Local,
    };
    let request = RunRequest {
        target,
        cwd: binding.workspace.clone(),
        executable: "claude".to_string(),
        config_dir: binding.config_dir.as_ref().map(PathBuf::from),
        model: None,
        resume_id: binding.native_session_id.clone(),
        prompt,
        run_id: invocation.dispatch_id.to_string(),
        generation,
    };
    let pidfile = match &request.target {
        Target::Local => claude_agent::local_process_handle(&request)
            .ok_or_else(|| io::Error::other("cannot establish local Claude process identity"))?
            .pidfile
            .display()
            .to_string(),
        Target::Ssh(_) => {
            claude_agent::remote_process_handle(&request)
                .ok_or_else(|| io::Error::other("cannot establish SSH Claude process identity"))?
                .pidfile
        }
    };
    let marker = store::ClaudeProcessMarker {
        session_id: invocation.session_id.to_string(),
        operation_id: invocation.dispatch_id.to_string(),
        generation: generation as i64,
        kind: store::ClaudeProcessKind::Worker,
        thread_name: Some(invocation.thread_name.to_string()),
        host_id: binding.host_id.clone(),
        ssh_port: binding.ssh_port,
        ssh_identity_file: binding.ssh_identity_file.clone(),
        workspace: binding.workspace.clone(),
        config_dir: binding.config_dir.clone(),
        pidfile,
        native_session_id: binding.native_session_id.clone(),
    };
    let marker_store_path = runtime.store_path.clone();
    tokio::task::spawn_blocking(move || {
        store::insert_claude_process_marker(&marker_store_path, &marker)
    })
    .await
    .map_err(io::Error::other)?
    .map_err(io::Error::other)?;
    let (cancel_sender, cancel_receiver) = watch::channel(cancellation.is_cancelled());
    let timeout_triggered = Arc::new(AtomicBool::new(false));
    let timeout_flag = Arc::clone(&timeout_triggered);
    let cancellation_state = cancellation.clone();
    let timeout_secs = invocation.timeout_secs;
    let timeout_task = tokio::spawn(async move {
        tokio::select! {
            () = cancellation.cancelled() => {}
            () = tokio::time::sleep(Duration::from_secs(timeout_secs)) => {
                timeout_flag.store(true, Ordering::Release);
            }
        }
        let _ = cancel_sender.send(true);
    });
    broker.activate_with_scope(
        invocation.dispatch_id,
        generation,
        crate::claude_approval::ClaudeApprovalScope {
            target: request.target.clone(),
            workspace: binding.workspace.clone(),
            store_path: Some(runtime.store_path.clone()),
        },
    );
    let store_path = runtime.store_path.clone();
    let session_id = invocation.session_id.to_string();
    let thread_name = invocation.thread_name.to_string();
    let init_dispatch_id = invocation.dispatch_id.to_string();
    let init_binding = binding.clone();
    let init_generation = generation as i64;
    let events = runtime.event_sink.clone();
    let event_thread_name = thread_name.clone();
    let active_tools = Arc::new(tokio::sync::Mutex::new(HashMap::<String, String>::new()));
    let event_tools = Arc::clone(&active_tools);
    let usage = Arc::new(tokio::sync::Mutex::new(TokenUsage::default()));
    let event_usage = Arc::clone(&usage);
    let approval_broker = Arc::clone(&broker);
    let approval_cancel = cancel_receiver.clone();
    let outcome = claude_agent::run(
        request,
        cancel_receiver,
        move |native_id| {
            let store_path = store_path.clone();
            let session_id = session_id.clone();
            let thread_name = thread_name.clone();
            let dispatch_id = init_dispatch_id.clone();
            let binding = init_binding.clone();
            async move {
                tokio::task::spawn_blocking(move || {
                    store::save_worker_claude_init(
                        &store_path,
                        &session_id,
                        &thread_name,
                        &dispatch_id,
                        init_generation,
                        &binding,
                        &native_id,
                    )
                })
                .await??;
                Ok(())
            }
        },
        move |message| {
            let events = events.clone();
            let thread_name = event_thread_name.clone();
            let active_tools = Arc::clone(&event_tools);
            let usage = Arc::clone(&event_usage);
            async move {
                if let Some(current) = assistant_usage(&message) {
                    usage.lock().await.add_cost_saturating(&current);
                }
                for event in assistant_events(&message, Some(&thread_name), &active_tools).await {
                    events.emit(event);
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
    broker.close_scope(invocation.dispatch_id, generation);
    timeout_task.abort();

    if let Ok(completed) = &outcome {
        if !completed.cancelled {
            let store_path = runtime.store_path.clone();
            let session_id = invocation.session_id.to_string();
            let thread_name = invocation.thread_name.to_string();
            let stored = tokio::task::spawn_blocking(move || {
                store::load_thread_claude_binding(&store_path, &session_id, &thread_name)
            })
            .await
            .map_err(io::Error::other)?
            .map_err(io::Error::other)?;
            if stored
                .as_ref()
                .and_then(|binding| binding.native_session_id.as_ref())
                != completed.session_id.as_ref()
                || completed.session_id.is_none()
            {
                return Err(io::Error::other(
                    "Claude worker returned without its native session ID being durably bound",
                ));
            }
        }
    }

    let timed_out = timeout_triggered.load(Ordering::Acquire);
    let mut outcome = match outcome {
        Ok(outcome) => outcome,
        Err(error) if timed_out => claude_agent::RunResult {
            session_id: binding.native_session_id,
            result: claude_agent::sanitize_output_text(&error.to_string(), MAX_HANDOFF_CHARS),
            is_error: true,
            cancelled: true,
        },
        Err(error) if cancellation_state.is_cancelled() => claude_agent::RunResult {
            session_id: binding.native_session_id,
            result: claude_agent::sanitize_output_text(&error.to_string(), MAX_HANDOFF_CHARS),
            is_error: true,
            cancelled: true,
        },
        Err(error) => {
            return Err(io::Error::other(claude_agent::sanitize_output_text(
                &error.to_string(),
                MAX_HANDOFF_CHARS,
            )))
        }
    };
    outcome.result = claude_agent::sanitize_output_text(&outcome.result, MAX_HANDOFF_CHARS + 1);
    if outcome.result.chars().count() > MAX_HANDOFF_CHARS {
        outcome.result = format!(
            "Claude worker final result exceeded the {MAX_HANDOFF_CHARS}-character handoff limit; inspect its streamed events on the host and ask the thread for a shorter summary."
        );
        outcome.is_error = true;
    }
    if !outcome.is_error && !outcome.cancelled && outcome.result.trim().is_empty() {
        outcome.result = "Claude worker completed without a handoff".to_string();
        outcome.is_error = true;
    }
    let usage = usage.lock().await.clone();
    let has_usage = usage.input_tokens > 0
        || usage.output_tokens > 0
        || usage.cache_read_tokens > 0
        || usage.cache_write_tokens > 0;
    Ok(WorkerRun {
        stdout: outcome.result.clone(),
        stderr: String::new(),
        exit_code: if outcome.is_error { 1 } else { 0 },
        timed_out,
        cancelled: outcome.cancelled && !timed_out,
        timeout_reason: timed_out
            .then(|| "The Claude worker exceeded its dispatch timeout".to_string()),
        usage: has_usage.then_some(usage),
        model_error: outcome.is_error.then_some(outcome.result),
        cleanup_error: None,
    })
}

pub(crate) fn assistant_usage(message: &serde_json::Value) -> Option<TokenUsage> {
    if message.get("type").and_then(serde_json::Value::as_str) != Some("assistant") {
        return None;
    }
    let usage = message.get("message")?.get("usage")?;
    let input_tokens = usage
        .get("input_tokens")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let output_tokens = usage
        .get("output_tokens")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let cache_read_tokens = usage
        .get("cache_read_input_tokens")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let cache_write_tokens = usage
        .get("cache_creation_input_tokens")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    Some(TokenUsage {
        input_tokens,
        output_tokens,
        cache_read_tokens,
        cache_write_tokens,
        orchestrator_context_tokens: 0,
        ..TokenUsage::default()
    })
}

pub(crate) async fn assistant_events(
    message: &serde_json::Value,
    thread_name: Option<&str>,
    active_tools: &tokio::sync::Mutex<HashMap<String, String>>,
) -> Vec<AgentEvent> {
    let message_type = message.get("type").and_then(serde_json::Value::as_str);
    let parent_call_id = message
        .get("parent_tool_use_id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let blocks = message
        .get("message")
        .and_then(|value| value.get("content"))
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten();
    let mut events = Vec::new();
    for block in blocks {
        match (
            message_type,
            block.get("type").and_then(serde_json::Value::as_str),
        ) {
            (Some("assistant"), Some("text")) => {
                if let Some(text) = block.get("text").and_then(serde_json::Value::as_str) {
                    events.push(AgentEvent::AssistantMessage {
                        thread_name: thread_name.map(str::to_string),
                        content: claude_agent::sanitize_output_text(text, 16_000),
                        usage: None,
                    });
                }
            }
            (Some("assistant"), Some("tool_use")) => {
                if let (Some(id), Some(name)) = (
                    block.get("id").and_then(serde_json::Value::as_str),
                    block.get("name").and_then(serde_json::Value::as_str),
                ) {
                    active_tools
                        .lock()
                        .await
                        .insert(id.to_string(), name.to_string());
                    events.push(AgentEvent::ToolCallStarted {
                        thread_name: thread_name.map(str::to_string),
                        call_id: id.to_string(),
                        parent_call_id: parent_call_id.clone(),
                        name: name.to_string(),
                        args_preview: "Claude tool invocation".to_string(),
                        key_arg_preview: None,
                        args_detail: None,
                    });
                }
            }
            (Some("user"), Some("tool_result")) => {
                if let Some(id) = block.get("tool_use_id").and_then(serde_json::Value::as_str) {
                    let name = active_tools
                        .lock()
                        .await
                        .remove(id)
                        .unwrap_or_else(|| "Claude tool".to_string());
                    events.push(AgentEvent::ToolCallFinished {
                        thread_name: thread_name.map(str::to_string),
                        call_id: id.to_string(),
                        parent_call_id: parent_call_id.clone(),
                        name,
                        content_preview: "Claude tool completed".to_string(),
                        is_error: block
                            .get("is_error")
                            .and_then(serde_json::Value::as_bool)
                            .unwrap_or(false),
                        command_status: None,
                        exit_code: None,
                        completion_status: None,
                        effective_timeout_ms: None,
                        execution_duration_ms: None,
                        cleanup_duration_ms: None,
                        remote_outcome_uncertain: false,
                    });
                }
            }
            _ => {}
        }
    }
    events
}

/// Claude keeps its own native conversation. NAC supplies only fresh source
/// handoffs on each dispatch, so a resumed native session never receives its
/// previous answers a second time.
pub(super) async fn build_prompt(
    runtime: &ToolRuntime,
    invocation: &WorkerInvocation<'_>,
) -> anyhow::Result<String> {
    let store_path = runtime.store_path.clone();
    let session_id = invocation.session_id.to_string();
    let thread_name = invocation.thread_name.to_string();
    let source_threads = invocation.source_threads.to_vec();
    let context = tokio::task::spawn_blocking(move || {
        store::load_worker_context(&store_path, &session_id, &thread_name, &source_threads)
    })
    .await??;
    prompt_from_context(invocation, &context, runtime.skills.as_deref())
}

fn prompt_from_context(
    invocation: &WorkerInvocation<'_>,
    context: &WorkerContext,
    skills: Option<&crate::skills::SkillRegistry>,
) -> anyhow::Result<String> {
    let mut prompt = format!(
        "You are Claude worker thread '{}', dispatched by a NAC orchestrator. Work in the selected workspace. Return a concise handoff with what you changed, what you verified, and any blockers.\n\nTask:\n{}",
        invocation.thread_name, invocation.action
    );
    for episode in &context.source_episodes {
        prompt.push_str("\n\n");
        prompt.push_str(&store::render_source_context(episode));
    }
    if !invocation.scheduled_skills.is_empty() {
        let registry = skills
            .ok_or_else(|| anyhow::anyhow!("requested skills but no skills are available"))?;
        for name in invocation.scheduled_skills {
            anyhow::ensure!(registry.has_skill(name), "unknown skill '{name}'");
            prompt.push_str("\n\nPreloaded worker skill: ");
            prompt.push_str(name);
            prompt.push('\n');
            prompt.push_str(
                &registry
                    .render_for_prompt(name)
                    .ok_or_else(|| anyhow::anyhow!("unknown skill '{name}'"))?,
            );
        }
    }
    Ok(prompt)
}

#[cfg(test)]
#[path = "claude_worker_tests.rs"]
mod tests;
