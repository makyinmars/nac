//! Claude-specific composition of host status, workspace trust, and approval
//! actions. Durable admission and process control remain in nac-core.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use nac_core::{
    claude_agent::{self, Target},
    runtime::SshOptions,
};

use crate::{
    ClaudeStatusQuery, ClaudeStatusResponse, ReplyClaudePermissionRequest, SessionManager,
};

impl SessionManager {
    pub async fn claude_status(&self, query: ClaudeStatusQuery) -> Result<ClaudeStatusResponse> {
        match self.probe_claude_host(query).await {
            Ok(status) => Ok(ClaudeStatusResponse {
                available: status.available,
                authenticated: status.authenticated,
                version: status.version,
                reason: status.reason,
            }),
            Err(error) => Ok(ClaudeStatusResponse {
                available: false,
                authenticated: false,
                version: None,
                reason: Some(error.to_string()),
            }),
        }
    }

    async fn probe_claude_host(
        &self,
        query: ClaudeStatusQuery,
    ) -> Result<claude_agent::PreflightStatus> {
        let ssh = SshOptions {
            host: query.ssh_host,
            port: query.ssh_port,
            identity_file: query.ssh_identity_file.map(PathBuf::from),
        };
        if ssh.host().is_none() && (ssh.port.is_some() || ssh.identity_file.is_some()) {
            return Err(anyhow!(
                "invalid request: ssh_port or ssh_identity_file requires ssh_host"
            ));
        }
        let target = ssh
            .resolved_connection(&self.inner.root_cwd)
            .map(Target::Ssh)
            .unwrap_or(Target::Local);
        let cwd = if matches!(target, Target::Ssh(_)) {
            Path::new(".")
        } else {
            self.inner.root_cwd.as_path()
        };
        let executable = query.claude_executable.as_deref().unwrap_or("claude");
        if executable.trim().is_empty() {
            return Err(anyhow!(
                "invalid request: claude_executable cannot be blank"
            ));
        }
        if query
            .claude_config_dir
            .as_deref()
            .is_some_and(|dir| dir.trim().is_empty())
        {
            return Err(anyhow!(
                "invalid request: claude_config_dir cannot be blank"
            ));
        }
        if query
            .claude_config_dir
            .as_deref()
            .is_some_and(|dir| !Path::new(dir).is_absolute())
        {
            return Err(anyhow!(
                "invalid request: claude_config_dir must be an absolute path on the execution host"
            ));
        }
        claude_agent::preflight(
            &target,
            cwd,
            executable,
            query.claude_config_dir.as_deref().map(Path::new),
        )
        .await
    }

    pub async fn trust_claude_worker_workspace(&self, session_id: &str) -> Result<()> {
        self.session_configuration()
            .trust_claude_worker_workspace(session_id)
            .await
    }

    pub async fn claude_permission_requests(
        &self,
        session_id: &str,
    ) -> Result<Vec<nac_core::claude_approval::ClaudePermissionRequest>> {
        Ok(self
            .attach_session(session_id)
            .await?
            .list_claude_permission_requests())
    }

    pub async fn reply_claude_permission_request(
        &self,
        session_id: &str,
        request_id: &str,
        request: ReplyClaudePermissionRequest,
    ) -> Result<()> {
        self.attach_session(session_id)
            .await?
            .reply_claude_permission_request(
                request_id,
                &request.run_id,
                request.generation,
                request.reply,
            )
    }
}
