use super::*;

/// A process that may still be editing a workspace after NAC loses contact.
/// The marker is committed before spawn and removed only after cleanup is
/// confirmed by the execution adapter or the recovery coordinator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaudeProcessKind {
    Session,
    Worker,
}

impl ClaudeProcessKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Worker => "worker",
        }
    }
    fn parse(raw: &str) -> Result<Self> {
        match raw {
            "session" => Ok(Self::Session),
            "worker" => Ok(Self::Worker),
            _ => Err(anyhow!("unsupported Claude process kind '{raw}'")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeProcessMarker {
    pub session_id: String,
    pub operation_id: String,
    pub generation: i64,
    pub kind: ClaudeProcessKind,
    pub thread_name: Option<String>,
    pub host_id: Option<String>,
    pub ssh_port: Option<u16>,
    pub ssh_identity_file: Option<String>,
    pub workspace: PathBuf,
    pub config_dir: Option<String>,
    pub pidfile: String,
    pub native_session_id: Option<String>,
}

pub fn insert_claude_process_marker(path: &Path, marker: &ClaudeProcessMarker) -> Result<()> {
    if marker.operation_id.is_empty() || marker.generation < 0 || marker.pidfile.is_empty() {
        return Err(anyhow!("Claude process marker identity is incomplete"));
    }
    if (marker.kind == ClaudeProcessKind::Worker) != marker.thread_name.is_some() {
        return Err(anyhow!(
            "Claude worker process marker thread identity is invalid"
        ));
    }
    let conn = open_runtime_connection(path)?;
    conn.execute(
        "INSERT INTO claude_processes
         (session_id, operation_id, generation, kind, thread_name, host_id, ssh_port, ssh_identity_file,
          workspace, config_dir, pidfile, native_session_id, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            marker.session_id,
            marker.operation_id,
            marker.generation,
            marker.kind.as_str(),
            marker.thread_name,
            marker.host_id,
            marker.ssh_port,
            marker.ssh_identity_file,
            marker.workspace.display().to_string(),
            marker.config_dir,
            marker.pidfile,
            marker.native_session_id,
            now_utc()
        ],
    )?;
    Ok(())
}

pub fn update_claude_process_native_session_id(
    path: &Path,
    session_id: &str,
    operation_id: &str,
    generation: i64,
    native_id: &str,
) -> Result<()> {
    if native_id.trim().is_empty() {
        return Err(anyhow!("Claude native session id is empty"));
    }
    let conn = open_runtime_connection(path)?;
    let changed = conn.execute(
        "UPDATE claude_processes SET native_session_id = ?4
         WHERE session_id = ?1 AND operation_id = ?2 AND generation = ?3
           AND (native_session_id IS NULL OR native_session_id = ?4)",
        params![session_id, operation_id, generation, native_id],
    )?;
    if changed == 0 {
        return Err(anyhow!(
            "Claude process marker is missing or has a different native session id"
        ));
    }
    Ok(())
}

/// Store SDK initialization as one durable fact for a worker. A crash cannot
/// leave the process marker resumable while the named thread still appears
/// to have no native session.
pub fn save_worker_claude_init(
    path: &Path,
    session_id: &str,
    thread_name: &str,
    dispatch_id: &str,
    generation: i64,
    binding: &ClaudeThreadBinding,
    native_id: &str,
) -> Result<()> {
    if native_id.trim().is_empty() || generation < 0 {
        return Err(anyhow!("Claude worker initialization identity is invalid"));
    }
    let mut conn = open_runtime_connection(path)?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let thread: Option<(String, Option<String>, Option<String>)> = tx
        .query_row(
            "SELECT agent, claude_binding_json, claude_native_session_id
         FROM threads WHERE session_id = ?1 AND name = ?2",
            params![session_id, thread_name],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((agent, binding_json, thread_native_id)) = thread else {
        return Err(anyhow!("Claude worker thread is missing"));
    };
    if agent != ThreadAgent::Claude.as_str() {
        return Err(anyhow!("worker thread is not bound to Claude"));
    }
    let stored: ClaudeThreadBinding = serde_json::from_str(
        binding_json
            .as_deref()
            .ok_or_else(|| anyhow!("Claude worker binding is missing"))?,
    )?;
    if stored.host_id != binding.host_id
        || stored.ssh_port != binding.ssh_port
        || stored.ssh_identity_file != binding.ssh_identity_file
        || stored.workspace != binding.workspace
        || stored.config_dir != binding.config_dir
    {
        return Err(anyhow!(
            "Claude worker host, workspace, or config binding changed"
        ));
    }
    type WorkerMarkerRow = (
        String,
        Option<String>,
        Option<String>,
        Option<u16>,
        Option<String>,
        String,
        Option<String>,
        Option<String>,
    );
    let marker: Option<WorkerMarkerRow> = tx.query_row(
        "SELECT kind, thread_name, host_id, ssh_port, ssh_identity_file, workspace, config_dir, native_session_id
         FROM claude_processes
         WHERE session_id = ?1 AND operation_id = ?2 AND generation = ?3",
        params![session_id, dispatch_id, generation],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?, row.get(7)?)),
    ).optional()?;
    let Some((
        kind,
        marker_thread_name,
        host,
        port,
        identity,
        workspace,
        config_dir,
        marker_native_id,
    )) = marker
    else {
        return Err(anyhow!("Claude worker process marker is missing"));
    };
    if kind != ClaudeProcessKind::Worker.as_str()
        || marker_thread_name.as_deref() != Some(thread_name)
        || host != binding.host_id
        || port != binding.ssh_port
        || identity != binding.ssh_identity_file
        || Path::new(&workspace) != binding.workspace
        || config_dir != binding.config_dir
    {
        return Err(anyhow!("Claude worker process marker binding changed"));
    }
    if thread_native_id
        .as_deref()
        .is_some_and(|stored| stored != native_id)
        || marker_native_id
            .as_deref()
            .is_some_and(|stored| stored != native_id)
    {
        return Err(anyhow!(
            "Claude worker native session id conflicts with durable identity"
        ));
    }
    tx.execute(
        "UPDATE threads SET claude_native_session_id = ?3
                WHERE session_id = ?1 AND name = ?2",
        params![session_id, thread_name, native_id],
    )?;
    tx.execute(
        "UPDATE claude_processes SET native_session_id = ?4
                WHERE session_id = ?1 AND operation_id = ?2 AND generation = ?3",
        params![session_id, dispatch_id, generation, native_id],
    )?;
    tx.commit()?;
    Ok(())
}

/// Check a recovered worker marker against its immutable thread binding. An
/// older two-write initialization may have stored the native ID in the marker
/// alone; finish that write only after the process has been cleaned up.
pub fn reconcile_worker_claude_marker_identity(
    path: &Path,
    marker: &ClaudeProcessMarker,
) -> Result<()> {
    if marker.kind != ClaudeProcessKind::Worker {
        return Err(anyhow!("Claude process marker is not a worker"));
    }
    let thread_name = marker
        .thread_name
        .as_deref()
        .ok_or_else(|| anyhow!("Claude worker process marker has no thread identity"))?;
    let mut conn = open_runtime_connection(path)?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let current: Option<(Option<String>, Option<String>)> = tx
        .query_row(
            "SELECT native_session_id, thread_name FROM claude_processes
         WHERE session_id = ?1 AND operation_id = ?2 AND generation = ?3 AND kind = 'worker'",
            params![marker.session_id, marker.operation_id, marker.generation],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((current_native_id, current_thread_name)) = current else {
        return Err(anyhow!("Claude worker process marker is missing"));
    };
    if current_thread_name.as_deref() != Some(thread_name)
        || current_native_id != marker.native_session_id
    {
        return Err(anyhow!("Claude worker process marker identity changed"));
    }
    let thread: Option<(String, Option<String>, Option<String>)> = tx
        .query_row(
            "SELECT agent, claude_binding_json, claude_native_session_id FROM threads
         WHERE session_id = ?1 AND name = ?2",
            params![marker.session_id, thread_name],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((agent, binding_json, thread_native_id)) = thread else {
        return Err(anyhow!("Claude worker thread is missing"));
    };
    if agent != ThreadAgent::Claude.as_str() {
        return Err(anyhow!("worker thread is not bound to Claude"));
    }
    let binding: ClaudeThreadBinding = serde_json::from_str(
        binding_json
            .as_deref()
            .ok_or_else(|| anyhow!("Claude worker binding is missing"))?,
    )?;
    if binding.host_id != marker.host_id
        || binding.ssh_port != marker.ssh_port
        || binding.ssh_identity_file != marker.ssh_identity_file
        || binding.workspace != marker.workspace
        || binding.config_dir != marker.config_dir
    {
        return Err(anyhow!(
            "Claude worker marker conflicts with thread host or workspace"
        ));
    }
    if let Some(native_id) = marker.native_session_id.as_deref() {
        if thread_native_id
            .as_deref()
            .is_some_and(|stored| stored != native_id)
        {
            return Err(anyhow!(
                "Claude worker marker conflicts with thread native session"
            ));
        }
        tx.execute(
            "UPDATE threads SET claude_native_session_id = ?3
                    WHERE session_id = ?1 AND name = ?2",
            params![marker.session_id, thread_name, native_id],
        )?;
    }
    tx.commit()?;
    Ok(())
}

pub fn clear_claude_process_marker(
    path: &Path,
    session_id: &str,
    operation_id: &str,
    generation: i64,
) -> Result<()> {
    let conn = open_runtime_connection(path)?;
    conn.execute("DELETE FROM claude_processes WHERE session_id = ?1 AND operation_id = ?2 AND generation = ?3",
        params![session_id, operation_id, generation])?;
    Ok(())
}

pub fn list_claude_process_markers(
    path: &Path,
    session_id: &str,
) -> Result<Vec<ClaudeProcessMarker>> {
    let conn = open_runtime_connection(path)?;
    let mut stmt = conn.prepare(
        "SELECT session_id, operation_id, generation, kind, thread_name, host_id, ssh_port, ssh_identity_file,
                workspace, config_dir, pidfile, native_session_id
         FROM claude_processes WHERE session_id = ?1 ORDER BY created_at, operation_id",
    )?;
    let rows = stmt.query_map(params![session_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, Option<String>>(5)?,
            row.get::<_, Option<u16>>(6)?,
            row.get::<_, Option<String>>(7)?,
            row.get::<_, String>(8)?,
            row.get::<_, Option<String>>(9)?,
            row.get::<_, String>(10)?,
            row.get::<_, Option<String>>(11)?,
        ))
    })?;
    rows.map(|row| {
        let (
            session_id,
            operation_id,
            generation,
            kind,
            thread_name,
            host_id,
            ssh_port,
            ssh_identity_file,
            workspace,
            config_dir,
            pidfile,
            native_session_id,
        ) = row?;
        Ok(ClaudeProcessMarker {
            session_id,
            operation_id,
            generation,
            kind: ClaudeProcessKind::parse(&kind)?,
            thread_name,
            host_id,
            ssh_port,
            ssh_identity_file,
            workspace: PathBuf::from(workspace),
            config_dir,
            pidfile,
            native_session_id,
        })
    })
    .collect()
}
