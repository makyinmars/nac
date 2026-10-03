use super::*;

pub fn append_episode(
    path: &Path,
    session_id: &str,
    thread_name: &str,
    action: &str,
    content: &str,
) -> Result<()> {
    append_episode_with_status(
        path,
        session_id,
        thread_name,
        action,
        content,
        EpisodeStatus::Ok,
    )
}

pub fn append_episode_with_status(
    path: &Path,
    session_id: &str,
    thread_name: &str,
    action: &str,
    content: &str,
    status: EpisodeStatus,
) -> Result<()> {
    let mut conn = open_runtime_connection(path)?;
    let tx = conn.transaction()?;
    ensure_thread_in_tx(&tx, session_id, thread_name)?;

    tx.execute(
        "INSERT INTO episodes (thread_name, session_id, action, content, status, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            thread_name,
            session_id,
            action,
            content,
            status.as_str(),
            now_utc()
        ],
    )?;

    tx.execute(
        "UPDATE threads
         SET updated_at = ?1
         WHERE name = ?2 AND session_id = ?3",
        params![now_utc(), thread_name, session_id],
    )?;

    tx.commit()?;
    Ok(())
}

/// Commit exactly one successful handoff for a dispatch. A retry returns the
/// original committed answer and rejects a reused ID for another action or
/// thread; the unique key is enforced by SQLite across processes.
pub fn append_episode_for_dispatch_once(
    path: &Path,
    session_id: &str,
    thread_name: &str,
    dispatch_id: &str,
    action: &str,
    content: &str,
) -> Result<EpisodeRecord> {
    if dispatch_id.trim().is_empty() {
        return Err(anyhow!("worker dispatch id is empty"));
    }
    let mut conn = open_runtime_connection(path)?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    if let Some(existing) = episode_for_dispatch_with_connection(&tx, session_id, dispatch_id)? {
        if existing.thread_name != thread_name || existing.action != action {
            return Err(anyhow!(
                "dispatch '{dispatch_id}' already belongs to a different thread or action"
            ));
        }
        tx.commit()?;
        return Ok(existing);
    }
    ensure_thread_in_tx(&tx, session_id, thread_name)?;
    let created_at = now_utc();
    tx.execute(
        "INSERT INTO episodes (thread_name, session_id, action, content, status, created_at, dispatch_id)
         VALUES (?1, ?2, ?3, ?4, 'ok', ?5, ?6)",
        params![thread_name, session_id, action, content, created_at, dispatch_id],
    )?;
    let id = tx.last_insert_rowid();
    tx.execute(
        "UPDATE threads SET updated_at = ?1 WHERE name = ?2 AND session_id = ?3",
        params![created_at, thread_name, session_id],
    )?;
    let committed = tx.query_row(
        "SELECT id, thread_name, session_id, action, content, status, created_at
         FROM episodes WHERE id = ?1",
        params![id],
        row_to_episode,
    )?;
    tx.commit()?;
    Ok(committed)
}

pub fn load_episode_for_dispatch(
    path: &Path,
    session_id: &str,
    dispatch_id: &str,
) -> Result<Option<EpisodeRecord>> {
    let conn = open_runtime_connection(path)?;
    episode_for_dispatch_with_connection(&conn, session_id, dispatch_id)
}

fn episode_for_dispatch_with_connection(
    conn: &Connection,
    session_id: &str,
    dispatch_id: &str,
) -> Result<Option<EpisodeRecord>> {
    conn.query_row(
        "SELECT id, thread_name, session_id, action, content, status, created_at
         FROM episodes WHERE session_id = ?1 AND dispatch_id = ?2",
        params![session_id, dispatch_id],
        row_to_episode,
    )
    .optional()
    .map_err(Into::into)
}

pub fn load_worker_context(
    path: &Path,
    session_id: &str,
    thread_name: &str,
    source_threads: &[String],
) -> Result<WorkerContext> {
    let conn = open_runtime_connection(path)?;
    let self_episodes = load_thread_episodes(&conn, session_id, thread_name)?;
    let mut source_episodes = Vec::with_capacity(source_threads.len());

    for source_thread in source_threads {
        let episode = latest_episode(&conn, session_id, source_thread)?
            .ok_or_else(|| anyhow!("Source thread '{source_thread}' has no retained episode"))?;
        source_episodes.push(episode);
    }

    Ok(WorkerContext {
        self_episodes,
        source_episodes,
    })
}

/// Every dispatch of every thread in one query, failures included, grouped by
/// thread name and ordered by id ASC (chronological order). The all-threads
/// twin of [`thread_dispatches`], and like it only the panel wants it.
pub fn load_all_dispatches(
    store_path: &Path,
    session_id: &str,
) -> Result<HashMap<String, Vec<EpisodeRecord>>> {
    let conn = open_runtime_connection(store_path)?;
    load_all_dispatches_with_connection(&conn, session_id)
}

pub(crate) fn load_all_dispatches_with_connection(
    conn: &Connection,
    session_id: &str,
) -> Result<HashMap<String, Vec<EpisodeRecord>>> {
    group_episodes(conn, session_id, false)
}

/// Retained handoffs for every thread, grouped and ordered the same way. The
/// all-threads twin of [`thread_read`], so a reader that means "what the
/// workers produced" never has a failed dispatch handed to it.
pub fn load_all_retained_episodes(
    store_path: &Path,
    session_id: &str,
) -> Result<HashMap<String, Vec<EpisodeRecord>>> {
    let conn = open_runtime_connection(store_path)?;
    group_episodes(&conn, session_id, true)
}

fn group_episodes(
    conn: &Connection,
    session_id: &str,
    retained_only: bool,
) -> Result<HashMap<String, Vec<EpisodeRecord>>> {
    let retained_filter = if retained_only {
        "AND e.status = 'ok'"
    } else {
        ""
    };
    let mut stmt = conn.prepare(&format!(
        "SELECT e.id, e.thread_name, e.session_id, e.action, e.content, e.status, e.created_at
         FROM episodes e
         INNER JOIN threads t ON e.thread_name = t.name AND e.session_id = t.session_id
         WHERE e.session_id = ? {retained_filter}
         ORDER BY e.thread_name, e.id"
    ))?;
    let rows = stmt.query_map(params![session_id], row_to_episode)?;

    let mut grouped: HashMap<String, Vec<EpisodeRecord>> = HashMap::new();
    for row in rows {
        let episode = row?;
        grouped
            .entry(episode.thread_name.clone())
            .or_default()
            .push(episode);
    }
    Ok(grouped)
}

pub fn list_threads(path: &Path, session_id: &str) -> Result<Vec<ThreadRecord>> {
    let conn = open_runtime_connection(path)?;
    list_threads_with_connection(&conn, session_id)
}

pub(crate) fn list_threads_with_connection(
    conn: &Connection,
    session_id: &str,
) -> Result<Vec<ThreadRecord>> {
    // The count is what a later dispatch can actually read back, so it only
    // counts retained episodes. The action is the last thing the thread was
    // asked to do whether or not it got there, which is what makes a thread
    // that has only ever failed still describable.
    let mut stmt = conn.prepare(
        "SELECT t.name, t.session_id, t.created_at, t.updated_at,
                (SELECT COUNT(*) FROM episodes e
                 WHERE e.thread_name = t.name AND e.session_id = t.session_id
                   AND e.status = 'ok') AS episode_count,
                (SELECT e.action FROM episodes e
                 WHERE e.thread_name = t.name AND e.session_id = t.session_id
                 ORDER BY e.id DESC
                 LIMIT 1) AS latest_action, t.agent
         FROM threads t
         WHERE t.session_id = ?1
         ORDER BY t.updated_at DESC, t.name ASC",
    )?;

    let mut rows = stmt.query([session_id])?;
    let mut threads = Vec::new();
    while let Some(row) = rows.next()? {
        threads.push(ThreadRecord {
            name: row.get(0)?,
            session_id: row.get(1)?,
            created_at: row.get(2)?,
            updated_at: row.get(3)?,
            episode_count: row.get(4)?,
            latest_action: row.get(5)?,
            agent: row
                .get::<_, String>(6)?
                .parse()
                .map_err(|error: anyhow::Error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        6,
                        rusqlite::types::Type::Text,
                        error.into(),
                    )
                })?,
        });
    }
    Ok(threads)
}

/// Establishes a named thread's immutable agent and Claude resume binding.
pub fn ensure_thread_agent(
    path: &Path,
    session_id: &str,
    thread_name: &str,
    agent: ThreadAgent,
    binding: Option<&ClaudeThreadBinding>,
) -> Result<()> {
    if (agent == ThreadAgent::Claude) != binding.is_some() {
        return Err(anyhow!(
            "Claude threads require a binding; NAC threads cannot have one"
        ));
    }
    let mut conn = open_runtime_connection(path)?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let existing: Option<(String, Option<String>)> = tx
        .query_row(
            "SELECT agent, claude_binding_json FROM threads WHERE session_id = ?1 AND name = ?2",
            params![session_id, thread_name],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    match existing {
        Some((stored_agent, stored_binding)) => {
            if stored_agent != agent.as_str() {
                return Err(anyhow!(
                    "thread '{thread_name}' is already bound to agent '{stored_agent}'"
                ));
            }
            if let Some(expected) = binding {
                let stored: ClaudeThreadBinding = serde_json::from_str(
                    stored_binding
                        .as_deref()
                        .ok_or_else(|| anyhow!("Claude thread binding is missing"))?,
                )?;
                if stored.host_id != expected.host_id
                    || stored.ssh_port != expected.ssh_port
                    || stored.ssh_identity_file != expected.ssh_identity_file
                    || stored.workspace != expected.workspace
                    || stored.config_dir != expected.config_dir
                {
                    return Err(anyhow!(
                        "Claude thread '{thread_name}' host, workspace, or config binding changed"
                    ));
                }
            }
        }
        None => {
            let now = now_utc();
            let binding_json = binding.map(serde_json::to_string).transpose()?;
            tx.execute("INSERT INTO threads (name, session_id, created_at, updated_at, agent, claude_binding_json) VALUES (?1, ?2, ?3, ?3, ?4, ?5)",
                params![thread_name, session_id, now, agent.as_str(), binding_json])?;
        }
    }
    tx.commit()?;
    Ok(())
}

pub fn load_thread_claude_binding(
    path: &Path,
    session_id: &str,
    thread_name: &str,
) -> Result<Option<ClaudeThreadBinding>> {
    let conn = open_runtime_connection(path)?;
    let row: Option<(String, Option<String>, Option<String>)> = conn.query_row(
        "SELECT agent, claude_binding_json, claude_native_session_id FROM threads WHERE session_id = ?1 AND name = ?2",
        params![session_id, thread_name], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    ).optional()?;
    let Some((agent, binding, native_session_id)) = row else {
        return Ok(None);
    };
    if agent != ThreadAgent::Claude.as_str() {
        return Ok(None);
    }
    let mut binding: ClaudeThreadBinding = serde_json::from_str(
        binding
            .as_deref()
            .ok_or_else(|| anyhow!("Claude thread binding is missing"))?,
    )?;
    binding.native_session_id = native_session_id;
    Ok(Some(binding))
}

pub fn save_thread_claude_session_id(
    path: &Path,
    session_id: &str,
    thread_name: &str,
    binding: &ClaudeThreadBinding,
    native_session_id: &str,
) -> Result<()> {
    if native_session_id.trim().is_empty() {
        return Err(anyhow!("Claude native session id is empty"));
    }
    ensure_thread_agent(
        path,
        session_id,
        thread_name,
        ThreadAgent::Claude,
        Some(binding),
    )?;
    let conn = open_runtime_connection(path)?;
    let changed = conn.execute("UPDATE threads SET claude_native_session_id = ?4 WHERE session_id = ?1 AND name = ?2 AND agent = 'claude' AND (claude_native_session_id IS NULL OR claude_native_session_id = ?3)",
        params![session_id, thread_name, native_session_id, native_session_id])?;
    if changed == 0 {
        return Err(anyhow!(
            "Claude native session id conflicts with the durable thread binding"
        ));
    }
    Ok(())
}

pub fn thread_read(path: &Path, session_id: &str, thread_name: &str) -> Result<Vec<EpisodeRecord>> {
    let conn = open_runtime_connection(path)?;
    load_thread_episodes(&conn, session_id, thread_name)
}

/// Every dispatch of one thread, failures included. Only the panel wants this;
/// model-facing reads go through [`thread_read`] so a failed dispatch never
/// becomes context.
pub fn thread_dispatches(
    path: &Path,
    session_id: &str,
    thread_name: &str,
) -> Result<Vec<EpisodeRecord>> {
    let conn = open_runtime_connection(path)?;
    let mut stmt = conn.prepare(
        "SELECT id, thread_name, session_id, action, content, status, created_at
         FROM episodes
         WHERE thread_name = ?1 AND session_id = ?2
         ORDER BY id ASC",
    )?;
    let mut rows = stmt.query(params![thread_name, session_id])?;
    let mut episodes = Vec::new();
    while let Some(row) = rows.next()? {
        episodes.push(row_to_episode(row)?);
    }
    Ok(episodes)
}

/// Highest episode id this thread holds, or 0 when it holds none. Taken before
/// a dispatch runs, it marks off everything the thread already had, so what the
/// dispatch itself wrote can be recognised afterwards.
pub fn latest_episode_id(path: &Path, session_id: &str, thread_name: &str) -> Result<i64> {
    let conn = open_runtime_connection(path)?;
    conn.query_row(
        "SELECT COALESCE(MAX(id), 0) FROM episodes
         WHERE thread_name = ?1 AND session_id = ?2",
        params![thread_name, session_id],
        |row| row.get(0),
    )
    .map_err(Into::into)
}

/// Whether the thread retained a handoff past `watermark`, which is how a
/// dispatch that answered before it was killed is told from one that never
/// produced anything.
pub fn has_retained_episode_after(
    path: &Path,
    session_id: &str,
    thread_name: &str,
    watermark: i64,
) -> Result<bool> {
    let conn = open_runtime_connection(path)?;
    conn.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM episodes
             WHERE thread_name = ?1 AND session_id = ?2 AND status = 'ok' AND id > ?3
         )",
        params![thread_name, session_id, watermark],
        |row| row.get(0),
    )
    .map_err(Into::into)
}

pub fn delete_thread(path: &Path, session_id: &str, thread_name: &str) -> Result<bool> {
    // The reserved orchestrator target names transcript log rows in
    // thread_events (store/transcript.rs) and orchestrator steering rows; it
    // is never a deletable thread. Reject BEFORE any DELETE: the unconditional
    // thread_events delete below would otherwise let a model-callable
    // `thread_delete("__orchestrator__")` wipe the transcript tail while
    // reporting "does not exist".
    if thread_name == ORCHESTRATOR_STEERING_TARGET {
        return Err(anyhow!(
            "thread name '{ORCHESTRATOR_STEERING_TARGET}' is reserved and cannot be deleted"
        ));
    }
    let mut conn = open_runtime_connection(path)?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    tx.execute(
        "DELETE FROM thread_steering WHERE session_id = ?1 AND thread_name = ?2",
        params![session_id, thread_name],
    )?;
    tx.execute(
        "DELETE FROM thread_events WHERE session_id = ?1 AND thread_name = ?2",
        params![session_id, thread_name],
    )?;
    tx.execute(
        "DELETE FROM episodes WHERE session_id = ?1 AND thread_name = ?2",
        params![session_id, thread_name],
    )?;
    let deleted = tx.execute(
        "DELETE FROM threads WHERE session_id = ?1 AND name = ?2",
        params![session_id, thread_name],
    )?;
    tx.commit()?;
    Ok(deleted > 0)
}

fn ensure_thread_in_tx(tx: &Transaction<'_>, session_id: &str, thread_name: &str) -> Result<()> {
    let now = now_utc();
    tx.execute(
        "INSERT OR IGNORE INTO threads (name, session_id, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?3)",
        params![thread_name, session_id, now],
    )?;
    Ok(())
}

fn load_thread_episodes(
    conn: &Connection,
    session_id: &str,
    thread_name: &str,
) -> Result<Vec<EpisodeRecord>> {
    let mut stmt = conn.prepare(
        "SELECT id, thread_name, session_id, action, content, status, created_at
         FROM episodes
         WHERE thread_name = ?1 AND session_id = ?2 AND status = 'ok'
         ORDER BY id ASC",
    )?;
    let mut rows = stmt.query(params![thread_name, session_id])?;
    let mut episodes = Vec::new();
    while let Some(row) = rows.next()? {
        episodes.push(row_to_episode(row)?);
    }
    Ok(episodes)
}

fn latest_episode(
    conn: &Connection,
    session_id: &str,
    thread_name: &str,
) -> Result<Option<EpisodeRecord>> {
    conn.query_row(
        "SELECT id, thread_name, session_id, action, content, status, created_at
         FROM episodes
         WHERE thread_name = ?1 AND session_id = ?2 AND status = 'ok'
         ORDER BY id DESC
         LIMIT 1",
        params![thread_name, session_id],
        row_to_episode,
    )
    .optional()
    .map_err(Into::into)
}

fn row_to_episode(row: &rusqlite::Row<'_>) -> rusqlite::Result<EpisodeRecord> {
    Ok(EpisodeRecord {
        id: row.get(0)?,
        thread_name: row.get(1)?,
        session_id: row.get(2)?,
        action: row.get(3)?,
        content: row.get(4)?,
        status: row.get(5)?,
        created_at: row.get(6)?,
    })
}
