//! Construction of the separate Claude Agent runtime. No NAC model client is
//! resolved here: Claude Code uses its own installation and login on the
//! selected execution host.

use super::*;

/// The durable identity and immutable execution target passed to the Claude
/// session engine after creation or attachment.
pub struct ClaudeRunConfig {
    pub(crate) snapshot: SessionSnapshot,
    pub(crate) store_path: PathBuf,
    pub(crate) workspace_display: String,
    pub(crate) workspace_git: GitTarget,
    pub(crate) resume_base_cwd: PathBuf,
}

impl ClaudeRunConfig {
    pub fn resume_base_cwd(&self) -> &Path {
        &self.resume_base_cwd
    }
}

pub async fn build_claude_run_config_for_project(
    options: RunOptions,
    config: &NacConfig,
    project_id: Option<String>,
    claude: sessions::ClaudeAgentSession,
) -> Result<ClaudeRunConfig> {
    if !claude.trusted_workspace {
        anyhow::bail!("Claude Agent needs explicit trust for this workspace before creation");
    }
    if claude.executable.trim().is_empty() {
        anyhow::bail!("Claude Agent executable cannot be empty");
    }
    if claude
        .config_dir
        .as_deref()
        .is_some_and(|dir| !Path::new(dir).is_absolute())
    {
        anyhow::bail!("Claude Agent configuration directory must be an absolute path");
    }
    let sandbox = effective_sandbox_options(options.sandbox, config);
    if sandbox.sandbox_enabled() || sandbox.explicit_sandbox_config_flags_present() {
        anyhow::bail!("Claude Agent does not support Podman or sandbox execution");
    }
    let ssh_host = options.ssh.host();
    let config_cwd = options
        .config_cwd
        .unwrap_or_else(|| default_config_cwd(&options.workspace_cwd, ssh_host.as_deref()));
    let paths = PathContext::new(&config_cwd);
    options.ssh.validate(&paths)?;
    let store_base_cwd = if ssh_host.is_some() {
        &config_cwd
    } else {
        &options.workspace_cwd
    };
    let store_path = resolve_store_path(store_base_cwd, options.store, config);
    store::initialize(&store_path)?;

    let (cwd, ssh) = match options.ssh.connection(&paths) {
        Some(connection) => {
            let requested = remote_cwd_or_home(options.workspace_cwd);
            let requested = requested
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("remote Claude workspace is not valid UTF-8"))?;
            let cwd = canonical_remote_session_cwd(&connection, requested, &paths).await?;
            (cwd, Some(connection))
        }
        None => {
            let cwd = options.workspace_cwd.canonicalize().with_context(|| {
                format!(
                    "failed to resolve Claude workspace {}",
                    options.workspace_cwd.display()
                )
            })?;
            if !cwd.is_dir() {
                anyhow::bail!("Claude workspace '{}' is not a directory", cwd.display());
            }
            (cwd, None)
        }
    };

    let session_id = Uuid::new_v4().to_string();
    let mut snapshot = sessions::new_snapshot(
        session_id,
        cwd,
        claude
            .model
            .clone()
            .unwrap_or_else(|| "default".to_string()),
        String::new(),
        BackendKind::ClaudeAgent,
        None,
        None,
        ssh,
        Vec::new(),
        None,
        BTreeMap::new(),
    );
    snapshot.behavior = sessions::SessionBehavior::Direct;
    snapshot.agent_runtime = sessions::AgentRuntime::ClaudeAgent;
    snapshot.claude_agent = Some(claude);
    snapshot.project_id = project_id;
    sessions::create_session(&store_path, &snapshot)?;
    from_snapshot(snapshot, store_path, config_cwd)
}

pub async fn build_claude_resume_config_for_session(
    store_path: PathBuf,
    session_id: &str,
    resume_base_cwd: PathBuf,
) -> Result<ClaudeRunConfig> {
    let _lease = sessions::SessionOperationLease::try_acquire(&store_path, session_id)?;
    let snapshot = sessions::load_session_async(store_path.clone(), session_id.to_string()).await?;
    from_snapshot(snapshot, store_path, resume_base_cwd)
}

/// Attachment may serve read-only snapshots while a peer owns the operation
/// lease. Claude recovery is performed under that lease at admission, after
/// the adapter has checked remote process identity.
pub async fn build_claude_resume_config_for_session_attachment(
    store_path: PathBuf,
    session_id: &str,
    resume_base_cwd: PathBuf,
) -> Result<(
    ClaudeRunConfig,
    bool,
    Option<sessions::SessionOperationLease>,
)> {
    let snapshot = sessions::load_session_async(store_path.clone(), session_id.to_string()).await?;
    let config = from_snapshot(snapshot, store_path, resume_base_cwd)?;
    Ok((config, true, None))
}

pub async fn build_claude_resume_config_for_session_with_lease(
    store_path: PathBuf,
    session_id: &str,
    resume_base_cwd: PathBuf,
    operation_lease: &sessions::SessionOperationLease,
) -> Result<ClaudeRunConfig> {
    operation_lease.validate(&store_path, session_id)?;
    let snapshot = sessions::load_session_async(store_path.clone(), session_id.to_string()).await?;
    from_snapshot(snapshot, store_path, resume_base_cwd)
}

fn from_snapshot(
    snapshot: SessionSnapshot,
    store_path: PathBuf,
    resume_base_cwd: PathBuf,
) -> Result<ClaudeRunConfig> {
    if snapshot.agent_runtime != sessions::AgentRuntime::ClaudeAgent
        || snapshot.behavior != sessions::SessionBehavior::Direct
        || snapshot.backend != BackendKind::ClaudeAgent
        || snapshot.sandbox_spec.is_some()
        || snapshot.claude_agent.is_none()
    {
        anyhow::bail!("session is not a valid Claude Agent direct session");
    }
    if snapshot
        .claude_agent
        .as_ref()
        .and_then(|claude| claude.config_dir.as_deref())
        .is_some_and(|dir| !Path::new(dir).is_absolute())
    {
        anyhow::bail!("Claude Agent configuration directory binding is not absolute");
    }
    let snapshot = resume::normalize_snapshot_paths(snapshot, &resume_base_cwd)?;
    let workspace_git = match snapshot.ssh.clone() {
        Some(connection) => GitTarget::ssh(connection, snapshot.cwd.clone(), &resume_base_cwd),
        None => GitTarget::local(snapshot.cwd.clone()),
    };
    let workspace_display = directory_display(&snapshot.cwd);
    Ok(ClaudeRunConfig {
        snapshot,
        store_path,
        workspace_display,
        workspace_git,
        resume_base_cwd,
    })
}
