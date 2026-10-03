use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Output, Stdio};
use std::thread;
use std::time::Instant;

use anyhow::{anyhow, bail, Context, Result};
use nac_process::ProcessTreeGuard;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::watch;
use tokio::time::{timeout, Duration};

use crate::paths::PathContext;
use crate::sandbox::ssh_command::{prepare_control_socket_dir, shell_quote, shell_quote_path};
use crate::sandbox::{ssh_wrapper_script, SANDBOX_EXEC_WRAPPER, SANDBOX_KILL_WRAPPER};

use super::output::ClaudeOutputRedactor;
use super::preflight::{preflight, verify_native_transcript};
use super::{ApprovalDecision, ApprovalRequest, RunRequest, RunResult, SshConnection, Target};

const MAX_PROTOCOL_LINE: usize = 1024 * 1024;
const MAX_STDERR: usize = 4096;
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

// The remote check uses shell builtins and examines each path component. A
// missing suffix is valid for Write, but symlinks below the workspace are not.
const SSH_PATH_PROBE: &str = r#"set -f
workspace=$1
requested=$2
cd "$workspace" || exit 41
root=$(pwd -P) || exit 41
case "$requested" in
  /*) candidate=$requested ;;
  *) candidate=$root/$requested ;;
esac
case "$candidate" in
  "$root") relative= ;;
  "$root"/*) relative=${candidate#"$root"/} ;;
  *) exit 42 ;;
esac
case "/$relative/" in
  *"/../"*|*"/./"*) exit 43 ;;
esac
old_ifs=$IFS
IFS=/
set -- $relative
IFS=$old_ifs
current=$root
for component do
  [ -n "$component" ] || exit 43
  next=$current/$component
  [ ! -L "$next" ] || exit 44
  current=$next
done
printf '%s\n%s\n' "$root" "$current""#;

#[derive(Debug, Clone)]
pub struct RemoteProcessHandle {
    pub connection: SshConnection,
    pub pidfile: String,
}

#[derive(Debug, Clone)]
pub struct LocalProcessHandle {
    pub pidfile: PathBuf,
}

/// This handle is known before launching. Store it with the durable run marker,
/// so recovery can reconcile a remote edit even if NAC dies before `init`.
pub fn remote_process_handle(request: &RunRequest) -> Option<RemoteProcessHandle> {
    let Target::Ssh(connection) = &request.target else {
        return None;
    };
    let digest = Sha256::digest(request.run_id.as_bytes());
    let id: String = digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Some(RemoteProcessHandle {
        connection: connection.clone(),
        pidfile: format!("~/.cache/nac/exec/claude-{}-{}.pid", id, request.generation),
    })
}

pub fn local_process_handle(request: &RunRequest) -> Option<LocalProcessHandle> {
    if !matches!(request.target, Target::Local) {
        return None;
    }
    let digest = Sha256::digest(request.run_id.as_bytes());
    let id: String = digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    Some(LocalProcessHandle {
        pidfile: home.join(format!(
            ".cache/nac/exec/claude-{id}-{}.pid",
            request.generation
        )),
    })
}

pub(super) fn ssh_command(connection: &SshConnection, remote: &str) -> Result<Command> {
    let paths = PathContext::new(std::env::current_dir()?);
    let control = connection.control_path(&paths);
    prepare_control_socket_dir(&control)?;
    let mut command = Command::new(ssh_program());
    command.args(connection.ssh_args(&control));
    command.arg("--").arg(&connection.host).arg(remote);
    command.kill_on_drop(true);
    command.env_remove("ANTHROPIC_API_KEY");
    command.env_remove("ANTHROPIC_AUTH_TOKEN");
    command.env_remove("ANTHROPIC_BASE_URL");
    command.env_remove("CLAUDE_CODE_OAUTH_TOKEN");
    command.env_remove("CLAUDE_CODE_SESSION_ACCESS_TOKEN");
    command.env_remove("CLAUDE_CODE_HOST_SESSION_ID");
    command.env_remove("CLAUDE_CONFIG_DIR");
    command.env_remove("CLAUDE_CODE_USE_BEDROCK");
    command.env_remove("CLAUDE_CODE_USE_VERTEX");
    command.env_remove("CLAUDE_CODE_USE_FOUNDRY");
    for (name, _) in std::env::vars_os() {
        let label = name.to_string_lossy();
        if label.starts_with("ANTHROPIC_")
            || (label.starts_with("CLAUDE_CODE_") && label.ends_with("_TOKEN"))
        {
            command.env_remove(name);
        }
    }
    Ok(command)
}

fn ssh_program() -> std::ffi::OsString {
    #[cfg(test)]
    if let Some(program) = std::env::var_os("NAC_TEST_CLAUDE_SSH_PROGRAM") {
        return program;
    }
    std::ffi::OsString::from("ssh")
}

/// Identity-checked cleanup. An SSH failure is an error, never evidence that
/// the Claude process is gone. Callers must retain the marker and block resume.
pub async fn reconcile_remote_process(handle: &RemoteProcessHandle) -> Result<()> {
    let remote = format!(
        "sh -c {} nac-claude-kill {}",
        shell_quote(SANDBOX_KILL_WRAPPER),
        shell_quote_path(&handle.pidfile)
    );
    let mut command = ssh_command(&handle.connection, &remote)?;
    command.stdin(Stdio::null());
    let output = timeout(Duration::from_secs(15), command.output())
        .await
        .context("SSH Claude cleanup timed out")??;
    if !output.status.success() {
        bail!(
            "SSH Claude cleanup on {} failed: {}",
            handle.connection.describe(),
            bounded_error(&output.stderr)
        );
    }
    Ok(())
}

pub async fn reconcile_local_process(handle: &LocalProcessHandle) -> Result<()> {
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(SANDBOX_KILL_WRAPPER)
        .arg("nac-claude-kill")
        .arg(&handle.pidfile);
    command.kill_on_drop(true);
    let output = timeout(Duration::from_secs(15), command.output())
        .await
        .context("local Claude cleanup timed out")??;
    if !output.status.success() {
        bail!(
            "local Claude cleanup failed: {}",
            bounded_error(&output.stderr)
        );
    }
    Ok(())
}

fn blocking_output(mut command: StdCommand) -> Result<Output> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if child.try_wait()?.is_some() {
            return child.wait_with_output().map_err(Into::into);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("Claude process reconciliation timed out");
        }
        thread::sleep(Duration::from_millis(25));
    }
}

pub fn reconcile_local_process_blocking(handle: &LocalProcessHandle) -> Result<()> {
    let mut command = StdCommand::new("sh");
    command
        .arg("-c")
        .arg(SANDBOX_KILL_WRAPPER)
        .arg("nac-claude-kill")
        .arg(&handle.pidfile);
    let output = blocking_output(command)?;
    if !output.status.success() {
        bail!(
            "local Claude cleanup failed: {}",
            bounded_error(&output.stderr)
        );
    }
    Ok(())
}

pub fn reconcile_remote_process_blocking(handle: &RemoteProcessHandle) -> Result<()> {
    let remote = format!(
        "sh -c {} nac-claude-kill {}",
        shell_quote(SANDBOX_KILL_WRAPPER),
        shell_quote_path(&handle.pidfile)
    );
    let paths = PathContext::new(std::env::current_dir()?);
    let control = handle.connection.control_path(&paths);
    prepare_control_socket_dir(&control)?;
    let mut command = StdCommand::new(ssh_program());
    command.args(handle.connection.ssh_args(&control));
    command.arg("--").arg(&handle.connection.host).arg(remote);
    command
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("ANTHROPIC_AUTH_TOKEN")
        .env_remove("ANTHROPIC_BASE_URL")
        .env_remove("CLAUDE_CODE_OAUTH_TOKEN")
        .env_remove("CLAUDE_CODE_SESSION_ACCESS_TOKEN")
        .env_remove("CLAUDE_CODE_HOST_SESSION_ID")
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("CLAUDE_CODE_USE_BEDROCK")
        .env_remove("CLAUDE_CODE_USE_VERTEX")
        .env_remove("CLAUDE_CODE_USE_FOUNDRY");
    for (name, _) in std::env::vars_os() {
        let label = name.to_string_lossy();
        if label.starts_with("ANTHROPIC_")
            || (label.starts_with("CLAUDE_CODE_") && label.ends_with("_TOKEN"))
        {
            command.env_remove(name);
        }
    }
    let output = blocking_output(command)?;
    if !output.status.success() {
        bail!(
            "SSH Claude cleanup on {} failed: {}",
            handle.connection.describe(),
            bounded_error(&output.stderr)
        );
    }
    Ok(())
}

pub(super) fn bounded_error(bytes: &[u8]) -> String {
    super::sanitize_output_text(
        String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_STDERR)]).trim(),
        MAX_STDERR,
    )
}

/// Resolve a Claude file-tool path on the selected SSH host before and after
/// approval. The caller must compare the result with its protected paths.
pub async fn probe_ssh_path(
    connection: &SshConnection,
    workspace: &Path,
    requested: &Path,
) -> Result<(PathBuf, PathBuf)> {
    for path in [workspace, requested] {
        let text = path.to_string_lossy();
        if text.contains('\n') || text.contains('\r') || text.contains('\0') {
            bail!("Claude file path contains an unsupported control character");
        }
    }
    let remote = format!(
        "sh -c {} nac-claude-path {} {}",
        shell_quote(SSH_PATH_PROBE),
        shell_quote_path(&workspace.display().to_string()),
        shell_quote_path(&requested.display().to_string())
    );
    let mut command = ssh_command(connection, &remote)?;
    command.stdin(Stdio::null());
    let output = timeout(Duration::from_secs(15), command.output())
        .await
        .context("SSH Claude path verification timed out")??;
    if !output.status.success() {
        bail!(
            "Claude path is outside the SSH workspace or traverses a symlink (status {})",
            output.status
        );
    }
    let text = std::str::from_utf8(&output.stdout)
        .context("SSH Claude path verification returned invalid UTF-8")?;
    let mut lines = text.lines();
    let root = lines
        .next()
        .filter(|line| !line.is_empty())
        .ok_or_else(|| anyhow!("SSH Claude path verification omitted workspace"))?;
    let path = lines
        .next()
        .filter(|line| !line.is_empty())
        .ok_or_else(|| anyhow!("SSH Claude path verification omitted path"))?;
    if lines.next().is_some() {
        bail!("SSH Claude path verification returned unexpected output");
    }
    Ok((PathBuf::from(root), PathBuf::from(path)))
}

fn bridge_path() -> PathBuf {
    #[cfg(test)]
    if let Some(path) = std::env::var_os("NAC_TEST_CLAUDE_BRIDGE") {
        return PathBuf::from(path);
    }
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("claude_bridge")
        .join("bridge.mjs")
}

pub(super) fn check_api_override() -> Result<()> {
    for (name, _) in std::env::vars_os() {
        let name = name.to_string_lossy();
        if (name.starts_with("CLAUDE_CODE_") && name.ends_with("_TOKEN"))
            || (name.starts_with("ANTHROPIC_")
                && (name.ends_with("_TOKEN") || name.ends_with("_KEY")))
        {
            bail!("{name} is set; Claude subscription execution requires removing API/provider overrides");
        }
    }
    for name in [
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_BASE_URL",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "CLAUDE_CODE_SESSION_ACCESS_TOKEN",
        "CLAUDE_CODE_HOST_SESSION_ID",
        "CLAUDE_CODE_USE_BEDROCK",
        "CLAUDE_CODE_USE_VERTEX",
        "CLAUDE_CODE_USE_FOUNDRY",
    ] {
        if std::env::var_os(name).is_some() {
            bail!("{name} is set; Claude subscription execution requires removing API/provider overrides");
        }
    }
    Ok(())
}

async fn next_line<R: AsyncBufRead + Unpin>(reader: &mut R) -> Result<Option<Vec<u8>>> {
    let mut out = Vec::new();
    loop {
        let buffer = reader.fill_buf().await?;
        if buffer.is_empty() {
            return if out.is_empty() {
                Ok(None)
            } else {
                Ok(Some(out))
            };
        }
        let count = buffer
            .iter()
            .position(|b| *b == b'\n')
            .map_or(buffer.len(), |n| n + 1);
        if out.len() + count > MAX_PROTOCOL_LINE {
            bail!("Claude bridge message exceeds {MAX_PROTOCOL_LINE} bytes");
        }
        let ended = buffer[count - 1] == b'\n';
        out.extend_from_slice(&buffer[..count]);
        reader.consume(count);
        if ended {
            return Ok(Some(out));
        }
    }
}

pub async fn run<FI, IF, FE, EF, FA, AF>(
    request: RunRequest,
    mut cancelled: watch::Receiver<bool>,
    mut on_init: FI,
    mut on_event: FE,
    mut on_approval: FA,
) -> Result<RunResult>
where
    FI: FnMut(String) -> IF,
    IF: Future<Output = Result<()>>,
    FE: FnMut(Value) -> EF,
    EF: Future<Output = Result<()>>,
    FA: FnMut(ApprovalRequest) -> AF,
    AF: Future<Output = Result<ApprovalDecision>>,
{
    if matches!(request.target, Target::Local) {
        check_api_override()?;
    }
    if *cancelled.borrow() {
        return Ok(RunResult {
            session_id: request.resume_id,
            result: String::new(),
            is_error: false,
            cancelled: true,
        });
    }
    let status = tokio::select! {
        biased;
        changed = cancelled.changed() => {
            if changed.is_err() || *cancelled.borrow() {
                return Ok(RunResult { session_id: request.resume_id, result: String::new(), is_error: false, cancelled: true });
            }
            preflight(&request.target, &request.cwd, &request.executable, request.config_dir.as_deref()).await?
        }
        status = preflight(&request.target, &request.cwd, &request.executable, request.config_dir.as_deref()) => status?,
    };
    if !status.available || !status.authenticated {
        bail!(
            "Claude subscription login is unavailable on the selected host: {}",
            status.reason.unwrap_or_else(|| {
                "run claude auth login with a supported Claude Code installation".to_string()
            })
        );
    }
    tokio::select! {
        biased;
        changed = cancelled.changed() => {
            if changed.is_err() || *cancelled.borrow() {
                return Ok(RunResult { session_id: request.resume_id, result: String::new(), is_error: false, cancelled: true });
            }
            verify_native_transcript(&request).await?;
        }
        checked = verify_native_transcript(&request) => checked?,
    }
    let bridge = bridge_path();
    if !bridge.is_file()
        || !bridge
            .parent()
            .is_some_and(|parent| parent.join("node_modules").is_dir())
    {
        bail!("Claude Agent SDK bridge is not installed; run make setup");
    }
    let remote_handle = remote_process_handle(&request);
    let local_handle = local_process_handle(&request);
    let mut command = Command::new("node");
    command.arg(&bridge);
    if matches!(request.target, Target::Local) {
        command.current_dir(&request.cwd);
        let handle = local_handle
            .as_ref()
            .ok_or_else(|| anyhow!("HOME is needed for local Claude process identity"))?;
        command = Command::new("bash");
        command
            .arg("-lc")
            .arg(ssh_wrapper_script(SANDBOX_EXEC_WRAPPER));
        command.arg("nac-claude");
        command.arg(format!(
            "node {}",
            shell_quote(&bridge.display().to_string())
        ));
        command.arg(&handle.pidfile);
        command.current_dir(&request.cwd);
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // The bridge receives selected configuration through the start message.
    // Its process environment must not carry NAC-host credentials to the SDK
    // or onward to a selected SSH host.
    command.env_remove("CLAUDE_CONFIG_DIR");
    command.env_remove("CLAUDE_CODE_HOST_SESSION_ID");
    command.env_remove("CLAUDE_CODE_USE_BEDROCK");
    command.env_remove("CLAUDE_CODE_USE_VERTEX");
    command.env_remove("CLAUDE_CODE_USE_FOUNDRY");
    for (name, _) in std::env::vars_os() {
        let label = name.to_string_lossy();
        if label.starts_with("ANTHROPIC_")
            || (label.starts_with("CLAUDE_CODE_") && label.ends_with("_TOKEN"))
        {
            command.env_remove(name);
        }
    }
    command.kill_on_drop(true);
    if let (Target::Ssh(connection), Some(handle)) = (&request.target, &remote_handle) {
        let paths = PathContext::new(std::env::current_dir()?);
        let control = connection.control_path(&paths);
        prepare_control_socket_dir(&control)?;
        command.env("NAC_CLAUDE_SSH_CONTROL_PATH", &control);
        // The remote CLI shim receives only connection and workspace metadata.
        // No local Claude credential or API key is forwarded.
        command.env("NAC_CLAUDE_REMOTE_PIDFILE", &handle.pidfile);
    }
    let (mut child, mut tree) = ProcessTreeGuard::spawn_supervised(&mut command)?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("Claude bridge stdin unavailable"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("Claude bridge stdout unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("Claude bridge stderr unavailable"))?;
    let stderr_task = tokio::spawn(async move {
        let mut reader = BufReader::new(stderr);
        let mut captured = Vec::new();
        let mut bytes = [0u8; 1024];
        loop {
            match reader.read(&mut bytes).await {
                Ok(0) | Err(_) => break,
                Ok(n) if captured.len() < MAX_STDERR => {
                    captured.extend_from_slice(&bytes[..n.min(MAX_STDERR - captured.len())]);
                }
                Ok(_) => {}
            }
        }
        bounded_error(&captured)
    });
    let remote = match (&request.target, &remote_handle) {
        (Target::Ssh(connection), Some(handle)) => {
            let paths = PathContext::new(std::env::current_dir()?);
            let control = connection.control_path(&paths);
            Some(json!({
                "host": connection.host,
                "ssh_args": connection.ssh_args(&control),
                "cwd": request.cwd,
                "executable": request.executable,
                "config_dir": request.config_dir,
                "wrapper": ssh_wrapper_script(SANDBOX_EXEC_WRAPPER),
                "pidfile": handle.pidfile,
            }))
        }
        _ => None,
    };
    let start = json!({
        "type": "start", "cwd": if matches!(request.target, Target::Local) { request.cwd.clone() } else { std::env::current_dir()? }, "executable": request.executable,
        "config_dir": request.config_dir, "model": request.model,
        "resume_id": request.resume_id, "prompt": request.prompt,
        "remote": remote,
    });
    stdin.write_all(start.to_string().as_bytes()).await?;
    stdin.write_all(b"\n").await?;
    stdin.flush().await?;
    let mut lines = BufReader::new(stdout);
    let mut result = RunResult {
        session_id: request.resume_id.clone(),
        result: String::new(),
        is_error: true,
        cancelled: false,
    };
    let redactor = ClaudeOutputRedactor::from_environment();
    let mut protocol_error = None;
    loop {
        let line = tokio::select! {
            biased;
            changed = cancelled.changed() => {
                if changed.is_err() || *cancelled.borrow() {
                    result.cancelled = true;
                    break;
                }
                continue;
            }
            line = next_line(&mut lines) => line,
        };
        let line = match line {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(error) => {
                protocol_error = Some(error);
                break;
            }
        };
        let message: Value = match serde_json::from_slice(&line) {
            Ok(value) => value,
            Err(error) => {
                protocol_error = Some(error.into());
                break;
            }
        };
        match message.get("type").and_then(Value::as_str) {
            Some("init") => {
                let Some(id) = message.get("session_id").and_then(Value::as_str) else {
                    protocol_error = Some(anyhow!("Claude init omitted session ID"));
                    break;
                };
                let id = id.to_string();
                if let Err(error) = on_init(id.clone()).await {
                    protocol_error = Some(error);
                    break;
                }
                result.session_id = Some(id);
                if let Err(error) = async {
                    stdin
                        .write_all(b"{\"type\":\"init_ack\",\"ok\":true}\n")
                        .await?;
                    stdin.flush().await
                }
                .await
                {
                    protocol_error = Some(error.into());
                    break;
                }
            }
            Some("event") => {
                if let Some(event) = message.get("event") {
                    // Stream deltas stay inside the service's bounded partial
                    // buffer. Redacting each chunk could leak a secret split
                    // across two deltas; completed SDK messages are redacted
                    // before they reach either event sink.
                    let event = if event.get("type").and_then(Value::as_str) == Some("stream_event")
                    {
                        event.clone()
                    } else {
                        redactor.event(event.clone())
                    };
                    if let Err(error) = on_event(event).await {
                        protocol_error = Some(error);
                        break;
                    }
                }
            }
            Some("approval_request") => {
                let Some(tool_use_id) = message
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                else {
                    protocol_error = Some(anyhow!("Claude approval has no native tool call ID"));
                    break;
                };
                let approval = ApprovalRequest {
                    id: message
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    tool_use_id: tool_use_id.to_string(),
                    tool_name: message
                        .get("tool_name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    input: message.get("input").cloned().unwrap_or(Value::Null),
                    run_id: request.run_id.clone(),
                    generation: request.generation,
                };
                let decision = tokio::select! {
                    biased;
                    changed = cancelled.changed() => {
                        if changed.is_err() || *cancelled.borrow() {
                            result.cancelled = true;
                            break;
                        }
                        ApprovalDecision::Deny("Approval interrupted".to_string())
                    }
                    answer = on_approval(approval.clone()) => answer
                        .unwrap_or_else(|error| ApprovalDecision::Deny(format!("Approval unavailable: {error}"))),
                };
                let response = match decision {
                    ApprovalDecision::Allow => {
                        json!({"type":"approval","id":approval.id,"allow":true})
                    }
                    ApprovalDecision::Deny(reason) => {
                        json!({"type":"approval","id":approval.id,"allow":false,"reason":reason})
                    }
                };
                if let Err(error) = async {
                    stdin.write_all(response.to_string().as_bytes()).await?;
                    stdin.write_all(b"\n").await?;
                    stdin.flush().await
                }
                .await
                {
                    protocol_error = Some(error.into());
                    break;
                }
            }
            Some("result") => {
                result.result = redactor.text(
                    message.get("result").and_then(Value::as_str).unwrap_or(""),
                    MAX_PROTOCOL_LINE,
                );
                result.is_error = message
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                if let Some(id) = message.get("session_id").and_then(Value::as_str) {
                    result.session_id = Some(id.to_string());
                }
                break;
            }
            Some("error") => {
                protocol_error = Some(anyhow!(
                    "Claude Agent SDK bridge failed: {}",
                    redactor.text(
                        message
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown error"),
                        MAX_STDERR,
                    )
                ));
                break;
            }
            _ => {
                protocol_error = Some(anyhow!("unknown Claude bridge message"));
                break;
            }
        }
    }
    drop(stdin);
    if result.cancelled || protocol_error.is_some() {
        // Reconcile the identity-recording wrapper while it is still alive.
        // Killing the transport first can leave a stale pidfile whose macOS
        // process-table lookup is necessarily uncertain after leader exit.
        let remote_cleanup = match &remote_handle {
            Some(handle) => reconcile_remote_process(handle).await,
            None => Ok(()),
        };
        let local_cleanup = match &local_handle {
            Some(handle) => reconcile_local_process(handle).await,
            None => Ok(()),
        };
        let transport_cleanup = tree.terminate(&mut child).await;
        remote_cleanup.context("remote Claude cleanup incomplete")?;
        local_cleanup.context("local Claude cleanup incomplete")?;
        transport_cleanup.context("local Claude bridge cleanup incomplete")?;
    } else {
        let wait = timeout(SHUTDOWN_GRACE, child.wait()).await;
        match wait {
            Ok(status) => {
                let status = status?;
                tree.mark_leader_reaped();
                tree.finish().await;
                if !status.success() && !result.is_error {
                    result.is_error = true;
                }
                if !status.success() {
                    if let Some(handle) = &remote_handle {
                        reconcile_remote_process(handle)
                            .await
                            .context("remote Claude cleanup after SSH failure incomplete")?;
                    }
                    if let Some(handle) = &local_handle {
                        reconcile_local_process(handle)
                            .await
                            .context("local Claude cleanup after bridge failure incomplete")?;
                    }
                }
            }
            Err(_) => {
                tree.terminate(&mut child).await?;
                if let Some(handle) = &remote_handle {
                    reconcile_remote_process(handle).await?;
                }
                if let Some(handle) = &local_handle {
                    reconcile_local_process(handle).await?;
                }
                bail!("Claude bridge did not exit after its final event");
            }
        }
    }
    let stderr = redactor.text(&stderr_task.await.unwrap_or_default(), MAX_STDERR);
    if let Some(error) = protocol_error {
        return Err(error.context(stderr));
    }
    if result.is_error && result.result.is_empty() && !result.cancelled {
        return Err(anyhow!("Claude failed: {stderr}"));
    }
    Ok(result)
}

#[cfg(all(test, unix))]
mod path_tests {
    use super::SSH_PATH_PROBE;
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::process::Command;
    use uuid::Uuid;

    #[test]
    fn ssh_path_probe_rejects_symlinks_and_escape_but_allows_new_write_path() {
        let root = std::env::temp_dir().join(format!("nac-claude-path-{}", Uuid::new_v4()));
        fs::create_dir_all(root.join("src")).unwrap();
        symlink("src", root.join("link")).unwrap();
        let probe = |path: &str| {
            Command::new("sh")
                .arg("-c")
                .arg(SSH_PATH_PROBE)
                .arg("nac-path")
                .arg(&root)
                .arg(path)
                .output()
                .unwrap()
        };
        let allowed = probe("src/new/file.rs");
        assert!(allowed.status.success(), "{:?}", allowed);
        assert!(String::from_utf8_lossy(&allowed.stdout).contains("/src/new/file.rs"));
        assert!(!probe("link/file.rs").status.success());
        assert!(!probe("src/../../outside").status.success());
        assert!(!probe("/tmp/outside").status.success());
        fs::remove_dir_all(root).unwrap();
    }
}
