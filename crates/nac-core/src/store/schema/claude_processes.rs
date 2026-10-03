use anyhow::Result;
use rusqlite::Connection;

/// Persistent process identity for crash and SSH disconnect recovery.
pub(super) fn create_claude_processes_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS claude_processes (
             session_id TEXT NOT NULL,
             operation_id TEXT NOT NULL,
             generation INTEGER NOT NULL CHECK (generation >= 0),
             kind TEXT NOT NULL CHECK (kind IN ('session', 'worker')),
             thread_name TEXT,
             host_id TEXT,
             ssh_port INTEGER,
             ssh_identity_file TEXT,
             workspace TEXT NOT NULL,
             config_dir TEXT,
             pidfile TEXT NOT NULL,
             native_session_id TEXT,
             created_at TEXT NOT NULL,
             PRIMARY KEY (session_id, operation_id, generation),
             FOREIGN KEY (session_id) REFERENCES sessions(session_id) ON DELETE CASCADE
         );
         CREATE INDEX IF NOT EXISTS idx_claude_processes_session
         ON claude_processes(session_id, created_at);",
    )?;
    super::ensure_column(conn, "claude_processes", "thread_name", "TEXT")?;
    super::ensure_column(conn, "claude_processes", "config_dir", "TEXT")?;
    Ok(())
}

/// A dispatch can commit at most one retained handoff, even across crashes.
pub(super) fn migrate_claude_dispatch_handoffs(conn: &Connection) -> Result<()> {
    super::ensure_column(conn, "episodes", "dispatch_id", "TEXT")?;
    conn.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_episode_dispatch_handoff
         ON episodes(session_id, dispatch_id) WHERE dispatch_id IS NOT NULL;",
    )?;
    Ok(())
}
