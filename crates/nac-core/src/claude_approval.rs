//! Interactive approval for Claude Code's own tools.
//!
//! Claude owns these invocations. NAC's prepared-tool grants and auto-approval
//! mode do not authorize them. A broker is scoped to one NAC session and every
//! request is bound to one active run or dispatch generation.
//!
//! The supported file tools are intentionally narrower than NAC's native tool
//! policy. Bash, other process tools, MCP tools, and unknown tools are denied.
//! File paths are checked on the execution host before and after approval, but
//! Claude Code does not execute through NAC's prepared no-follow file handles.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::{oneshot, watch};

use crate::claude_agent::{ApprovalDecision, ApprovalRequest};
use crate::events::{SessionEvent, SessionEventBus};

const APPROVAL_TIMEOUT: Duration = Duration::from_secs(300);
const SUBSCRIBER_POLL: Duration = Duration::from_millis(100);
const MAX_INPUT_PREVIEW_CHARS: usize = 16 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ClaudePermissionRequest {
    pub id: String,
    pub claude_request_id: String,
    pub session_id: String,
    pub run_id: String,
    pub generation: u64,
    pub tool_name: String,
    /// Complete JSON input within the UI's review limit. Larger requests are
    /// denied instead of displaying an incomplete command for approval.
    pub input_preview: String,
    pub created_at_epoch_ms: u64,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum ClaudePermissionReply {
    AllowOnce,
    Deny,
}

struct Pending {
    request: ClaudePermissionRequest,
    answer: oneshot::Sender<ClaudePermissionReply>,
}

#[derive(Clone, Debug)]
pub struct ClaudeApprovalScope {
    pub target: crate::claude_agent::Target,
    pub workspace: PathBuf,
    pub store_path: Option<PathBuf>,
}

#[derive(Clone)]
struct Active {
    generation: u64,
    scope: Option<ClaudeApprovalScope>,
}

#[derive(Default)]
struct State {
    active: HashMap<String, Active>,
    pending: HashMap<String, Pending>,
}

/// Process-local broker. A restart drops waiters and their authority; the
/// service must reconcile the interrupted durable run before activating it.
pub struct ClaudeApprovalBroker {
    session_id: String,
    events: SessionEventBus,
    state: Mutex<State>,
}

impl ClaudeApprovalBroker {
    pub fn new(session_id: impl Into<String>, events: SessionEventBus) -> Arc<Self> {
        Arc::new(Self {
            session_id: session_id.into(),
            events,
            state: Mutex::new(State::default()),
        })
    }

    /// Activate only after durable admission has claimed this exact run.
    /// A newer generation closes every pending request from the old one.
    pub fn activate(&self, run_id: &str, generation: u64) {
        self.activate_inner(run_id, generation, None);
    }

    pub fn activate_with_scope(&self, run_id: &str, generation: u64, scope: ClaudeApprovalScope) {
        self.activate_inner(run_id, generation, Some(scope));
    }

    fn activate_inner(&self, run_id: &str, generation: u64, scope: Option<ClaudeApprovalScope>) {
        let obsolete = {
            let mut state = self.lock_state();
            state
                .active
                .insert(run_id.to_string(), Active { generation, scope });
            state
                .pending
                .iter()
                .filter(|(_, pending)| {
                    pending.request.run_id == run_id && pending.request.generation != generation
                })
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>()
        };
        for id in obsolete {
            self.dismiss(&id, "the run generation changed");
        }
    }

    pub fn pending(&self) -> Vec<ClaudePermissionRequest> {
        let mut pending = self
            .lock_state()
            .pending
            .values()
            .map(|entry| entry.request.clone())
            .collect::<Vec<_>>();
        pending.sort_by(|a, b| {
            a.created_at_epoch_ms
                .cmp(&b.created_at_epoch_ms)
                .then_with(|| a.id.cmp(&b.id))
        });
        pending
    }

    /// The caller must supply the run identity seen by the UI. An answer to
    /// an old generation can never authorize a request in a later run.
    pub fn reply(
        &self,
        request_id: &str,
        run_id: &str,
        generation: u64,
        reply: ClaudePermissionReply,
    ) -> anyhow::Result<()> {
        let delivered = {
            let mut state = self.lock_state();
            let request = state
                .pending
                .get(request_id)
                .ok_or_else(|| anyhow::anyhow!("Claude approval request is no longer active"))?;
            anyhow::ensure!(
                request.request.run_id == run_id
                    && request.request.generation == generation
                    && state
                        .active
                        .get(run_id)
                        .is_some_and(|active| active.generation == generation),
                "Claude approval request does not belong to this active run"
            );
            let pending = state
                .pending
                .remove(request_id)
                .ok_or_else(|| anyhow::anyhow!("Claude approval request ended"))?;
            pending.answer.send(reply).is_ok()
        };
        if !delivered {
            self.events.emit(SessionEvent::ClaudePermissionDismissed {
                request_id: request_id.to_string(),
                reason: "Claude approval waiter ended before the reply".to_string(),
            });
            anyhow::bail!("Claude approval waiter ended before the reply");
        }
        self.events.emit(SessionEvent::ClaudePermissionReplied {
            request_id: request_id.to_string(),
            reply,
        });
        Ok(())
    }

    pub fn close_scope(&self, run_id: &str, generation: u64) {
        let ids = {
            let mut state = self.lock_state();
            if state
                .active
                .get(run_id)
                .is_none_or(|active| active.generation != generation)
            {
                return;
            }
            state.active.remove(run_id);
            state
                .pending
                .iter()
                .filter(|(_, pending)| {
                    pending.request.run_id == run_id && pending.request.generation == generation
                })
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>()
        };
        for id in ids {
            self.dismiss(&id, "the Claude run ended");
        }
    }

    pub fn close_all(&self) {
        let ids = {
            let mut state = self.lock_state();
            state.active.clear();
            state.pending.keys().cloned().collect::<Vec<_>>()
        };
        for id in ids {
            self.dismiss(&id, "the Claude session ended");
        }
    }

    /// Called only by the supervised bridge callback. Headless execution,
    /// stale generations, lost subscribers, cancellation and timeout deny.
    pub async fn ask(
        self: &Arc<Self>,
        request: ApprovalRequest,
        mut cancelled: watch::Receiver<bool>,
    ) -> ApprovalDecision {
        if *cancelled.borrow() {
            return deny("Claude run was cancelled before approval");
        }
        if request.input.to_string().chars().count() > MAX_INPUT_PREVIEW_CHARS {
            return deny("Claude tool input is too large to review safely in the approval UI");
        }
        let scope = {
            let state = self.lock_state();
            match state.active.get(&request.run_id) {
                Some(active) if active.generation == request.generation => active.scope.clone(),
                _ => return deny("Claude approval belongs to an inactive run generation"),
            }
        };
        let Some(scope) = scope else {
            return deny("Claude approval has no bound execution workspace");
        };
        if let Err(reason) = hard_policy(&request.tool_name, &request.input, &scope).await {
            return deny(&reason);
        }
        let policy_tool = request.tool_name.clone();
        let policy_input = request.input.clone();
        let (sender, receiver) = oneshot::channel();
        let ui_request = ClaudePermissionRequest {
            id: uuid::Uuid::new_v4().to_string(),
            claude_request_id: request.tool_use_id,
            session_id: self.session_id.clone(),
            run_id: request.run_id,
            generation: request.generation,
            tool_name: request.tool_name,
            input_preview: bounded_preview(&request.input),
            created_at_epoch_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
        };
        {
            let mut state = self.lock_state();
            if state
                .active
                .get(&ui_request.run_id)
                .is_none_or(|active| active.generation != ui_request.generation)
            {
                return deny("Claude approval belongs to an inactive run generation");
            }
            if !self.events.has_interactive_subscribers() {
                return deny("Claude approval requires an interactive session client");
            }
            state.pending.insert(
                ui_request.id.clone(),
                Pending {
                    request: ui_request.clone(),
                    answer: sender,
                },
            );
            self.events.emit(SessionEvent::ClaudePermissionAsked {
                request: ui_request.clone(),
            });
        }
        let _guard = WaiterGuard {
            broker: Arc::clone(self),
            request_id: ui_request.id.clone(),
        };
        let mut interval = tokio::time::interval(SUBSCRIBER_POLL);
        let result = tokio::select! {
            biased;
            answer = receiver => match answer {
                Ok(ClaudePermissionReply::AllowOnce) if !*cancelled.borrow() => ApprovalDecision::Allow,
                Ok(ClaudePermissionReply::AllowOnce) => deny("Claude run was cancelled while awaiting approval"),
                Ok(ClaudePermissionReply::Deny) => deny("Claude tool request was denied"),
                Err(_) => deny("Claude approval ended before a reply"),
            },
            _ = cancelled.changed() => deny("Claude run was cancelled while awaiting approval"),
            _ = tokio::time::sleep(APPROVAL_TIMEOUT) => deny("Claude approval timed out"),
            _ = async {
                loop {
                    interval.tick().await;
                    if !self.events.has_interactive_subscribers() { break; }
                }
            } => deny("interactive session client disconnected while Claude approval was pending"),
        };
        match result {
            ApprovalDecision::Allow => {
                let still_active = self
                    .lock_state()
                    .active
                    .get(&ui_request.run_id)
                    .is_some_and(|active| {
                        active.generation == ui_request.generation && active.scope.is_some()
                    });
                if !still_active {
                    return deny("Claude approval belongs to an inactive run generation");
                }
                if let Err(reason) = hard_policy(&policy_tool, &policy_input, &scope).await {
                    return deny(&reason);
                }
                if !self
                    .lock_state()
                    .active
                    .get(&ui_request.run_id)
                    .is_some_and(|active| {
                        active.generation == ui_request.generation && active.scope.is_some()
                    })
                {
                    return deny("Claude approval belongs to an inactive run generation");
                }
                ApprovalDecision::Allow
            }
            other => other,
        }
    }

    fn dismiss(&self, request_id: &str, reason: &str) {
        if self.lock_state().pending.remove(request_id).is_some() {
            self.events.emit(SessionEvent::ClaudePermissionDismissed {
                request_id: request_id.to_string(),
                reason: reason.to_string(),
            });
        }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

struct WaiterGuard {
    broker: Arc<ClaudeApprovalBroker>,
    request_id: String,
}

impl Drop for WaiterGuard {
    fn drop(&mut self) {
        self.broker
            .dismiss(&self.request_id, "Claude approval waiter ended");
    }
}

fn bounded_preview(value: &serde_json::Value) -> String {
    let text = value.to_string();
    let mut preview = text
        .chars()
        .take(MAX_INPUT_PREVIEW_CHARS)
        .collect::<String>();
    if text.chars().count() > MAX_INPUT_PREVIEW_CHARS {
        preview.push('…');
    }
    preview
}

async fn hard_policy(
    tool: &str,
    input: &serde_json::Value,
    scope: &ClaudeApprovalScope,
) -> Result<(), String> {
    // These tools can execute commands, launch untracked agents, modify
    // notebooks through embedded code, or reach resources whose authority NAC
    // cannot independently bind. A UI answer never overrides this denial.
    let path_key = match tool {
        "Read" | "Edit" | "Write" => "file_path",
        "Glob" | "Grep" => "path",
        _ => {
            return Err(format!(
                "Claude tool '{tool}' is unavailable because NAC cannot enforce its hard policy"
            ))
        }
    };
    let input = input
        .as_object()
        .ok_or_else(|| format!("Claude {tool} input is not an object"))?;
    let requested = match input.get(path_key) {
        Some(serde_json::Value::String(path)) if !path.is_empty() => PathBuf::from(path),
        None if matches!(tool, "Glob" | "Grep") => scope.workspace.clone(),
        _ => return Err(format!("Claude {tool} requires a valid {path_key}")),
    };
    let path_pattern = match tool {
        "Glob" => input.get("pattern"),
        "Grep" => input.get("glob"),
        _ => None,
    };
    if path_pattern
        .and_then(serde_json::Value::as_str)
        .is_some_and(unsafe_glob)
    {
        return Err(format!("Claude {tool} pattern can escape the workspace"));
    }
    let bound = bind_path(&scope.workspace, &requested)?;
    let (workspace, resolved) = match &scope.target {
        crate::claude_agent::Target::Local => local_probe_path(&scope.workspace, &bound)?,
        crate::claude_agent::Target::Ssh(connection) => {
            crate::claude_agent::probe_ssh_path(connection, &scope.workspace, &bound)
                .await
                .map_err(|error| format!("SSH Claude path validation failed: {error:#}"))?
        }
    };
    if !resolved.starts_with(&workspace) {
        return Err(format!("Claude {tool} path escapes the selected workspace"));
    }
    if matches!(tool, "Edit" | "Write") {
        if resolved.components().any(|part| part.as_os_str() == ".git") {
            return Err("Claude cannot mutate Git metadata directly".to_string());
        }
        if scope.store_path.as_ref().is_some_and(|store| {
            matches!(&scope.target, crate::claude_agent::Target::Local) && store == &resolved
        }) {
            return Err("Claude cannot mutate the active NAC session store".to_string());
        }
    }
    Ok(())
}

fn bind_path(workspace: &Path, requested: &Path) -> Result<PathBuf, String> {
    if !workspace.is_absolute() {
        return Err("Claude workspace must be an absolute path".to_string());
    }
    if requested
        .components()
        .any(|part| matches!(part, Component::ParentDir))
    {
        return Err("Claude path contains parent traversal".to_string());
    }
    let bound = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        workspace.join(requested)
    };
    if !bound.starts_with(workspace) {
        return Err("Claude path is outside the selected workspace".to_string());
    }
    Ok(bound)
}

fn unsafe_glob(pattern: &str) -> bool {
    pattern.starts_with('/') || pattern.split('/').any(|part| part == "..")
}

fn local_probe_path(workspace: &Path, requested: &Path) -> Result<(PathBuf, PathBuf), String> {
    let canonical_workspace = workspace
        .canonicalize()
        .map_err(|error| format!("Claude workspace could not be resolved: {error}"))?;
    let suffix = requested
        .strip_prefix(workspace)
        .map_err(|_| "Claude path is outside the selected workspace".to_string())?;
    let mut cursor = workspace.to_path_buf();
    for component in suffix.components() {
        cursor.push(component.as_os_str());
        match std::fs::symlink_metadata(&cursor) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err("Claude path contains a symbolic link".to_string());
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(format!("Claude path could not be inspected: {error}")),
        }
    }
    let canonical_requested = crate::tools::mutation::resolve_target_path(requested)
        .map_err(|error| format!("Claude path could not be resolved: {error}"))?;
    Ok((canonical_workspace, canonical_requested))
}

fn deny(message: &str) -> ApprovalDecision {
    ApprovalDecision::Deny(message.to_string())
}

#[cfg(test)]
#[path = "claude_approval_tests.rs"]
mod tests;
