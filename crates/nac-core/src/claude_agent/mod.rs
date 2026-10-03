//! Supervised Claude Agent SDK process boundary.
//!
//! NAC owns durable admission and approval decisions. This adapter only runs
//! one Claude turn on the selected execution target and streams its protocol.

use std::future::Future;
use std::path::PathBuf;

use anyhow::Result;
use serde_json::Value;
use tokio::sync::watch;

pub use crate::sandbox::SshConnection;

mod output;
mod preflight;
mod process;
#[cfg(test)]
mod tests;
pub(crate) use output::sanitize_output_text;
pub use preflight::{preflight, PreflightStatus};
pub use process::{
    local_process_handle, probe_ssh_path, reconcile_local_process,
    reconcile_local_process_blocking, reconcile_remote_process, reconcile_remote_process_blocking,
    remote_process_handle, LocalProcessHandle, RemoteProcessHandle,
};

#[derive(Debug, Clone)]
pub enum Target {
    Local,
    Ssh(SshConnection),
}

#[derive(Debug, Clone)]
pub struct RunRequest {
    pub target: Target,
    pub cwd: PathBuf,
    pub executable: String,
    pub config_dir: Option<PathBuf>,
    pub model: Option<String>,
    pub resume_id: Option<String>,
    pub prompt: String,
    /// Opaque NAC durable run/dispatch identity, never sent to Claude.
    pub run_id: String,
    pub generation: u64,
}

#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    /// Bridge reply correlation ID, scoped to this process.
    pub id: String,
    /// Native Claude tool call ID, bound to the SDK PreToolUse hook.
    pub tool_use_id: String,
    pub tool_name: String,
    pub input: Value,
    pub run_id: String,
    pub generation: u64,
}

#[derive(Debug, Clone)]
pub enum ApprovalDecision {
    Allow,
    Deny(String),
}

#[derive(Debug, Clone)]
pub struct RunResult {
    pub session_id: Option<String>,
    pub result: String,
    pub is_error: bool,
    pub cancelled: bool,
}

/// Start one turn. `on_init` must durably record the native ID before it
/// returns; no subsequent event or tool approval is consumed until then.
pub async fn run<FI, IF, FE, EF, FA, AF>(
    request: RunRequest,
    cancelled: watch::Receiver<bool>,
    on_init: FI,
    on_event: FE,
    on_approval: FA,
) -> Result<RunResult>
where
    FI: FnMut(String) -> IF,
    IF: Future<Output = Result<()>>,
    FE: FnMut(Value) -> EF,
    EF: Future<Output = Result<()>>,
    FA: FnMut(ApprovalRequest) -> AF,
    AF: Future<Output = Result<ApprovalDecision>>,
{
    process::run(request, cancelled, on_init, on_event, on_approval).await
}
